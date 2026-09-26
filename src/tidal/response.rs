use color_eyre::eyre::{Error, OptionExt, Result, eyre};
use tracing::{error, warn};

use super::model::{
    TidalMediaData, TidalMediaResponse, TidalPageResponse, TidalPlaylistResponse,
    TidalSearchResponse, TidalSongItemResponse, TidalSongResponse,
};
use crate::{
    music_api::{Album, Artist, MusicApiType, Playlist, Playlists, Song, Songs},
    utils::clean_isrc,
};

// multiples

impl TryInto<Playlists> for TidalPageResponse<TidalPlaylistResponse> {
    type Error = Error;

    fn try_into(self) -> Result<Playlists, Self::Error> {
        let mut res = vec![];
        for item in self.items {
            let playlist = match item.try_into() {
                Ok(p) => p,
                Err(e) => {
                    error!("failed to parse playlist in response, skipping it: {}", e);
                    continue;
                }
            };
            res.push(playlist);
        }
        Ok(Playlists(res))
    }
}

impl TryInto<Songs> for TidalPageResponse<TidalSongItemResponse> {
    type Error = Error;

    fn try_into(self) -> Result<Songs, Self::Error> {
        let mut res = vec![];
        for item in self.items {
            let song = match item.item.try_into() {
                Ok(s) => s,
                Err(e) => {
                    error!("failed to parse song in response, skipping it: {}", e);
                    continue;
                }
            };
            res.push(song);
        }
        Ok(Songs(res))
    }
}

impl TryInto<Songs> for TidalSearchResponse {
    type Error = Error;

    fn try_into(self) -> Result<Songs, Self::Error> {
        let mut res = vec![];
        for track in self.tracks.items {
            match track.try_into() {
                Ok(s) => res.push(s),
                Err(e) => {
                    error!(
                        "failed to parse song in response, skipping it. error log: `{}`",
                        e
                    );
                }
            }
        }
        Ok(Songs(res))
    }
}

// singles

impl TryInto<Playlist> for TidalPlaylistResponse {
    type Error = Error;
    fn try_into(self) -> Result<Playlist, Self::Error> {
        Ok(Playlist {
            id: self.uuid,
            name: self.title.trim().to_string(),
            songs: vec![],
        })
    }
}

impl TryInto<Song> for TidalSongResponse {
    type Error = Error;
    fn try_into(self) -> Result<Song, Self::Error> {
        let Some(album) = self.album else {
            return Err(eyre!("{}: missing song album data", self.title));
        };
        let album = Album {
            id: Some(album.id.to_string()),
            name: album.title,
        };
        let artists = self
            .artists
            .into_iter()
            .map(|a| Artist {
                id: Some(a.id.to_string()),
                name: a.name,
            })
            .collect();

        Ok(Song {
            source: MusicApiType::Tidal,
            id: self.id.to_string(),
            sid: None,
            isrc: clean_isrc(self.isrc),
            name: self.title,
            album: Some(album),
            artists,
            duration_ms: self.duration * 1000,
        })
    }
}

impl TryInto<Songs> for TidalMediaResponse {
    type Error = Error;

    fn try_into(self) -> Result<Songs, Self::Error> {
        let (songs, _skipped) = self.into_songs()?;
        Ok(songs)
    }
}

impl TidalMediaResponse {
    /// Parse every track, most popular first; returns the songs and how many
    /// tracks could not be parsed.
    pub fn into_songs(mut self) -> Result<(Songs, usize)> {
        if self.data.is_empty() {
            return Err(eyre!("missing song data"));
        }
        // return the most popular song first
        self.data.sort_by(|a, b| {
            a.attributes
                .popularity
                .partial_cmp(&b.attributes.popularity)
                .unwrap()
                .reverse()
        });
        let included = self.included.ok_or_eyre("missing included data")?;

        let mut songs = Vec::new();
        let mut skipped = 0;
        for data in self.data {
            match media_data_to_song(data, &included) {
                Ok(s) => songs.push(s),
                Err(e) => {
                    skipped += 1;
                    error!("failed to parse song in response, skipping it: {}", e);
                }
            }
        }
        Ok((Songs(songs), skipped))
    }
}

/// The included resource a relationship points at. TIDAL's JSON:API
/// responses can omit some of them (e.g. a track's second artist), and IDs
/// are only unique per type.
fn find_included<'a>(
    included: &'a [TidalMediaData],
    typ: &str,
    id: &str,
) -> Option<&'a TidalMediaData> {
    included.iter().find(|i| i.typ == typ && i.id == id)
}

fn media_data_to_song(data: TidalMediaData, included: &[TidalMediaData]) -> Result<Song> {
    let duration = &data
        .attributes
        .duration
        .ok_or_eyre("missing song duration")?;
    let duration = iso8601::duration(duration).map_err(|e| eyre!(e))?;
    let iso8601::Duration::YMDHMS {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
    } = duration
    else {
        unreachable!("invalid iso8601 duration");
    };
    assert!(year == 0 && month == 0 && day == 0);
    // convert to ms
    let duration = hour as usize * 60 * 60 * 1000
        + minute as usize * 60 * 1000
        + second as usize * 1000
        + millisecond as usize;

    let mut artists = Vec::new();
    let mut album = None;

    if let Some(album_rel) = data
        .relationships
        .as_ref()
        .and_then(|r| r.albums.as_ref())
        .and_then(|a| a.data.as_ref())
    {
        if album_rel.len() != 1 {
            return Err(eyre!("invalid song with multiple albums"));
        }
        let Some(album_rel) = album_rel.first() else {
            return Err(eyre!("missing song album data"));
        };
        if let Some(album_data) = find_included(included, "albums", &album_rel.id) {
            let title = album_data
                .attributes
                .title
                .clone()
                .ok_or_eyre("missing song album title")?;
            album = Some(Album {
                id: Some(album_data.id.clone()),
                name: title,
            });
        } else {
            warn!(
                "album {} of track {} missing from TIDAL response, continuing without it",
                album_rel.id, data.id
            );
        }
    }
    if let Some(artists_rel) = data
        .relationships
        .as_ref()
        .and_then(|r| r.artists.as_ref())
        .and_then(|a| a.data.as_ref())
    {
        for artist_rel in artists_rel {
            let Some(artist_data) = find_included(included, "artists", &artist_rel.id) else {
                warn!(
                    "artist {} of track {} missing from TIDAL response, skipping the artist",
                    artist_rel.id, data.id
                );
                continue;
            };
            let name = artist_data
                .attributes
                .name
                .clone()
                .ok_or_eyre("missing song artist name")?;
            artists.push(Artist {
                id: Some(artist_data.id.clone()),
                name,
            });
        }
    }

    Ok(Song {
        source: MusicApiType::Tidal,
        id: data.id,
        sid: None,
        isrc: clean_isrc(data.attributes.isrc),
        name: data.attributes.title.ok_or_eyre("missing song title")?,
        album,
        artists,
        duration_ms: duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape of TIDAL's `GET /v2/tracks?filter[isrc]=GBAHS2501558&include=albums,artists`
    // response: the track names two artists, but `included` only carries one.
    const TRUNCATED_INCLUDES: &str = r#"{
        "data": [{
            "id": "470727704", "type": "tracks",
            "attributes": {"title": "Beto’s Horns (fred remix)", "isrc": "GBAHS2501558",
                           "duration": "PT3M46S", "popularity": 0.57},
            "relationships": {
                "albums": {"data": [{"id": "470727703", "type": "albums"}]},
                "artists": {"data": [{"id": "17062018", "type": "artists"},
                                     {"id": "10313951", "type": "artists"}]}
            }
        }],
        "included": [
            {"id": "470727703", "type": "albums", "attributes": {"title": "Beto’s Horns (fred remix)"}},
            {"id": "17062018", "type": "artists", "attributes": {"name": "Fred again.."}}
        ]
    }"#;

    #[test]
    fn missing_included_artist_keeps_the_track() {
        let res: TidalMediaResponse = serde_json::from_str(TRUNCATED_INCLUDES).unwrap();
        let (songs, skipped) = res.into_songs().unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(songs.0.len(), 1);
        let song = &songs.0[0];
        assert_eq!(song.id, "470727704");
        assert_eq!(song.isrc.as_deref(), Some("GBAHS2501558"));
        assert_eq!(song.duration_ms, 226_000);
        assert_eq!(song.album.as_ref().unwrap().name, "Beto’s Horns (fred remix)");
        let artists: Vec<&str> = song.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(artists, vec!["Fred again.."]);
    }

    #[test]
    fn included_lookup_matches_type_as_well_as_id() {
        // An album and an artist sharing an ID must not be confused.
        let json = TRUNCATED_INCLUDES.replace(
            r#"{"id": "17062018", "type": "artists", "attributes": {"name": "Fred again.."}}"#,
            r#"{"id": "17062018", "type": "albums", "attributes": {"title": "Not an artist"}}"#,
        );
        let res: TidalMediaResponse = serde_json::from_str(&json).unwrap();
        let (songs, _) = res.into_songs().unwrap();
        assert!(songs.0[0].artists.is_empty());
    }

    #[test]
    fn unparseable_tracks_are_counted() {
        let json = TRUNCATED_INCLUDES.replace(r#""duration": "PT3M46S", "#, "");
        let res: TidalMediaResponse = serde_json::from_str(&json).unwrap();
        let (songs, skipped) = res.into_songs().unwrap();
        assert!(songs.0.is_empty());
        assert_eq!(skipped, 1);
    }
}
