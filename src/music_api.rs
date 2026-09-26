use async_trait::async_trait;
use color_eyre::eyre::Result;
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use strsim::normalized_levenshtein;
use tracing::debug;

use crate::utils::generic_name_clean;

pub const PLAYLIST_DESC: &str = "Playlist created by SyncDisBoi";

pub type DynMusicApi = Box<dyn MusicApi + Sync>;

/// Result of looking a song up on a platform, distinguishing "the platform
/// returned candidates we could not read" from a genuine miss.
#[derive(Debug)]
pub enum SearchOutcome {
    Found(Song),
    NotFound,
    /// The platform returned `candidates` results, but none could be parsed.
    Unparseable { candidates: usize },
}

#[async_trait]
pub trait MusicApi {
    fn request_concurrency(&self) -> usize {
        4
    }

    fn api_type(&self) -> MusicApiType;
    fn country_code(&self) -> &str;

    async fn create_playlist(&self, name: &str, public: bool) -> Result<Playlist>;
    async fn get_playlists_info(&self) -> Result<Vec<Playlist>>;
    async fn get_playlist_songs(&self, id: &str) -> Result<Vec<Song>>;

    async fn get_playlists_full(&self) -> Result<Vec<Playlist>> {
        let mut playlists = self.get_playlists_info().await?;

        let playlist_ids = playlists
            .iter()
            .enumerate()
            .map(|(index, playlist)| (index, playlist.id.clone()))
            .collect::<Vec<_>>();
        let requests = playlist_ids.into_iter().map(|(index, playlist_id)| async move {
                let songs = self.get_playlist_songs(&playlist_id).await?;
                Ok::<_, color_eyre::Report>((index, songs))
            });
        let results: Vec<(usize, Vec<Song>)> = stream::iter(requests)
            .buffered(self.request_concurrency())
            .try_collect()
            .await?;
        let mut results = results;
        results.sort_by_key(|(index, _)| *index);
        for (index, songs) in results {
            playlists[index].songs = songs;
        }

        Ok(playlists)
    }

    async fn add_songs_to_playlist(&self, playlist: &mut Playlist, songs: &[Song]) -> Result<()>;
    async fn remove_songs_from_playlist(
        &self,
        playlist: &mut Playlist,
        songs_ids: &[Song],
    ) -> Result<()>;
    async fn delete_playlist(&self, playlist: Playlist) -> Result<()>;

    async fn search_song(&self, song: &Song) -> Result<Option<Song>>;

    /// Like `search_song`, but reports unparseable results separately.
    /// Platforms that can tell the difference override this.
    async fn search_song_outcome(&self, song: &Song) -> Result<SearchOutcome> {
        Ok(match self.search_song(song).await? {
            Some(found) => SearchOutcome::Found(found),
            None => SearchOutcome::NotFound,
        })
    }

    async fn search_songs(&self, songs: &[Song]) -> Result<Vec<Option<Song>>> {
        let owned_songs = songs
            .iter()
            .cloned()
            .enumerate()
            .collect::<Vec<_>>();
        let requests = owned_songs.into_iter().map(|(index, song)| async move {
            let result = self.search_song(&song).await?;
            Ok::<_, color_eyre::Report>((index, result))
        });
        let mut results: Vec<(usize, Option<Song>)> = stream::iter(requests)
            .buffered(self.request_concurrency())
            .try_collect()
            .await?;
        results.sort_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, song)| song).collect())
    }

    async fn add_likes(&self, songs: &[Song]) -> Result<()>;
    async fn get_likes(&self) -> Result<Vec<Song>>;
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub enum MusicApiType {
    Spotify,
    YtMusic,
    Tidal,
}

impl MusicApiType {
    pub const fn short_name(&self) -> &'static str {
        match self {
            MusicApiType::Spotify => "spotify",
            MusicApiType::YtMusic => "ytmusic",
            MusicApiType::Tidal => "tidal",
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Playlists(pub Vec<Playlist>);

#[derive(Deserialize, Serialize, Debug)]
pub struct Songs(pub Vec<Song>);

#[derive(Deserialize, Serialize, Debug)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub songs: Vec<Song>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Song {
    pub source: MusicApiType,
    pub id: String,
    pub sid: Option<String>,
    pub isrc: Option<String>,
    pub name: String,
    pub album: Option<Album>,
    pub artists: Vec<Artist>,
    pub duration_ms: usize,
}

impl Song {
    pub fn clean_name(&self) -> String {
        match self.source {
            MusicApiType::Spotify | MusicApiType::Tidal | MusicApiType::YtMusic => {
                let name = generic_name_clean(&self.name);
                let name = name.split(" - ").next().unwrap_or(&name);
                let name = name.split(" pts. ").next().unwrap_or(name);
                let name = name.split(" feat. ").next().unwrap_or(name);
                name.trim_end().to_string()
            }
        }
    }

    pub fn is_single(&self) -> bool {
        // TODO: improve this, leverage metadata from APIs when it exists
        if let Some(album) = &self.album {
            album.name == self.name
        } else {
            false
        }
    }

    pub fn compare(&self, other: &Self) -> bool {
        if self.source == other.source {
            return self.id == other.id;
        }
        // A shared ISRC is conclusive. Different ISRCs are not: labels
        // sometimes register the same recording twice (e.g. once per
        // platform delivery), so fall through to the metadata checks, and
        // additionally require a shared artist to keep remixes and edits
        // with coincidentally equal durations apart.
        let isrc_mismatch = match (&self.isrc, &other.isrc) {
            (Some(a), Some(b)) if a == b => return true,
            (Some(_), Some(_)) => true,
            _ => false,
        };
        if isrc_mismatch && !self.shares_artist_with(other) {
            return false;
        }

        // Check song name resemblance
        let name1 = self.clean_name();
        let name2 = other.clean_name();
        let score = normalized_levenshtein(&name1, &name2).abs();
        if score < 0.8 {
            return false;
        }

        // INFO: We can't really compare artists names since they are not always the
        // same order.
        // For certain platforms they are included in the song name but not in the
        // metadata

        // Check song duration resemblance
        // NOTE: YtMusic duration is sometimes garbage, it's incorrect on certain songs
        // it's still better to use it for accuracy
        let dur1 = self.duration_ms / 1000;
        let dur2 = other.duration_ms / 1000;

        // we allow a 1 second difference
        if !(dur1 - 1..=dur1 + 1).contains(&dur2) {
            debug!("Duration: {} vs {} --> {} VS {}", dur1, dur2, self, other);
            return false;
        }

        if let (Some(album1), Some(album2)) = (&self.album, &other.album) {
            // INFO: Sometimes Youtube Music maps the album song to the Youtube Video
            // Sometimes, the album song is just suppressed from the 'Songs' filter
            // In these cases, we can get the single instead so we shouldn't compare album
            // names
            if !self.is_single() && !other.is_single() {
                // Check album name resemblance
                let name1 = album1.clean_name();
                let name2 = album2.clean_name();
                let score = normalized_levenshtein(&name1, &name2).abs();
                if score < 0.8 {
                    return false;
                }
            }
        }

        true
    }

    fn shares_artist_with(&self, other: &Self) -> bool {
        self.artists.iter().any(|a| {
            let name = a.clean_name();
            other.artists.iter().any(|b| b.clean_name() == name)
        })
    }

    pub fn build_queries(&self) -> Vec<String> {
        let mut queries = vec![];
        let track_name = self.clean_name();

        // Query: Track + Album
        if let Some(album) = self.album.as_ref() {
            let album_name = album.clean_name();
            let tr_al_query = format!("{} {}", track_name, album_name);
            queries.push(tr_al_query);
        }
        // Query: Track + Artist
        for artist in self.artists.iter().rev() {
            let artist_name = artist.clean_name();
            let tr_ar_query = format!("{} {}", track_name, artist_name);
            queries.push(tr_ar_query);
        }
        // Query: Track + Artist + Album
        if let Some(album) = self.album.as_ref() {
            let album_name = album.clean_name();
            for artist in self.artists.iter().rev() {
                let artist_name = artist.clean_name();
                let tr_ar_al_query = format!("{} {} {}", track_name, artist_name, album_name);
                queries.push(tr_ar_al_query);
            }
        }
        queries
    }
}

impl PartialEq for Song {
    fn eq(&self, other: &Self) -> bool {
        self.compare(other)
    }
}

impl Eq for Song {}

impl std::fmt::Display for Song {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let artists = self
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<&str>>()
            .join(" ");
        let artists = String::from(" - ") + &artists;
        let album = if let Some(a) = &self.album {
            format!(" ({})", a.name)
        } else {
            String::new()
        };
        f.write_fmt(format_args!("{}{}{}", self.name, album, artists))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Album {
    pub id: Option<String>,
    pub name: String,
}

impl Album {
    pub fn clean_name(&self) -> String {
        generic_name_clean(&self.name)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Artist {
    pub id: Option<String>,
    pub name: String,
}

impl Artist {
    pub fn clean_name(&self) -> String {
        // TODO: Add ' - ' parsing?
        generic_name_clean(&self.name)
    }
}

#[derive(Serialize, Debug)]
pub struct OAuthReqToken {
    pub client_id: String,
    pub device_code: String,
    pub grant_type: String,
    pub scope: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct OAuthToken {
    pub scope: String,
    pub token_type: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

#[derive(Deserialize, Debug)]
pub struct OAuthRefreshToken {
    pub access_token: String,
    pub expires_in: u64,
    pub scope: String,
    pub token_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(source: MusicApiType, isrc: &str, name: &str, album: &str, artists: &[&str], ms: usize) -> Song {
        Song {
            source,
            id: format!("{name}-{isrc}"),
            sid: None,
            isrc: Some(isrc.to_string()),
            name: name.to_string(),
            album: Some(Album { id: None, name: album.to_string() }),
            artists: artists
                .iter()
                .map(|a| Artist { id: None, name: (*a).to_string() })
                .collect(),
            duration_ms: ms,
        }
    }

    fn spotify_nice_to_know_you() -> Song {
        song(
            MusicApiType::Spotify,
            "GBAYE2501225",
            "Nice to Know You + Loukeman + Leod",
            "Fancy Some More?",
            &["PinkPantheress", "Loukeman", "Leod"],
            202_065,
        )
    }

    #[test]
    fn same_isrc_is_a_match() {
        let tidal = song(MusicApiType::Tidal, "GBAYE2501225", "Other", "Other", &["X"], 1_000);
        assert!(spotify_nice_to_know_you().compare(&tidal));
    }

    #[test]
    fn different_isrc_falls_back_to_metadata() {
        // TIDAL registers this recording under another ISRC.
        let tidal = song(
            MusicApiType::Tidal,
            "GBAYE2501423",
            "Nice to Know You + Loukeman + Leod",
            "Fancy Some More?",
            &["PinkPantheress"],
            202_000,
        );
        assert!(spotify_nice_to_know_you().compare(&tidal));
    }

    #[test]
    fn different_isrc_without_a_shared_artist_is_not_a_match() {
        let tidal = song(
            MusicApiType::Tidal,
            "GBAYE2501423",
            "Nice to Know You + Loukeman + Leod",
            "Fancy Some More?",
            &["Someone Else"],
            202_000,
        );
        assert!(!spotify_nice_to_know_you().compare(&tidal));
    }

    #[test]
    fn different_isrc_and_duration_is_not_a_match() {
        // e.g. an extended mix sharing the cleaned title and album
        let tidal = song(
            MusicApiType::Tidal,
            "GBAYE2509999",
            "Nice to Know You + Loukeman + Leod (Extended)",
            "Fancy Some More?",
            &["PinkPantheress"],
            262_000,
        );
        assert!(!spotify_nice_to_know_you().compare(&tidal));
    }
}
