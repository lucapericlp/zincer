use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, eyre};
use serde::Serialize;
use serde_json::json;
use time::{OffsetDateTime, macros::format_description};
use tracing::{debug, info, warn};

use crate::ConfigArgs;
use crate::music_api::{DynMusicApi, MusicApiType, Playlist, Song};
use crate::utils::dedup_songs;

// TODO: Parse playlist owner to ignore platform-specific playlists?
const SKIPPED_PLAYLISTS: [&str; 10] = [
    // Yt Music specific
    "New playlist",
    "Your Likes",
    "My Supermix",
    "Discover Mix",
    "Episodes for Later",
    // Spotify specific
    "Liked Songs",
    "Discover Weekly",
    "Big Room House Mix",
    "Motivation Electronic Mix",
    "High Energy Mix",
];

#[derive(Default, Serialize)]
struct SyncReport {
    playlists: Vec<PlaylistSyncReport>,
}

#[derive(Serialize)]
struct PlaylistSyncReport {
    name: String,
    source_playlist_id: String,
    destination_playlist_id: String,
    source_tracks: usize,
    duplicate_tracks_skipped: usize,
    already_synced_tracks: usize,
    newly_synced_tracks: usize,
    not_synced_tracks_count: usize,
    success_rate: f64,
    not_synced_tracks: Vec<NotSyncedTrack>,
}

#[derive(Serialize)]
struct NotSyncedTrack {
    reason: NotSyncedReason,
    source_track: Song,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum NotSyncedReason {
    MissingAlbumMetadata,
    NoMatchFound,
    DuplicateDestinationMatch,
}

pub async fn synchronize(
    src_api: DynMusicApi,
    dst_api: DynMusicApi,
    config: ConfigArgs,
) -> Result<()> {
    if !config.diff_country
        && src_api.api_type() != MusicApiType::YtMusic
        && dst_api.api_type() != MusicApiType::YtMusic
        && src_api.country_code() != dst_api.country_code()
    {
        return Err(eyre!(
            "source and destination music platforms are in different countries ({} vs {}). \
                You can specify --diff-country to allow it, \
                but this might result in incorrect sync results.",
            src_api.country_code(),
            dst_api.country_code()
        ));
    }

    if config.debug {
        std::fs::create_dir_all("debug")?;
    }

    info!("retrieving source playlists...");
    let src_playlists = src_api.get_playlists_full().await?;

    synchronize_playlists(src_playlists, &dst_api, &config).await?;

    if config.sync_likes {
        synchronize_likes(&src_api, &dst_api).await?;
    }

    Ok(())
}

pub async fn synchronize_playlists(
    src_playlists: Vec<Playlist>,
    dst_api: &DynMusicApi,
    config: &ConfigArgs,
) -> Result<()> {
    let mut all_missing_songs = json!({});
    let mut all_new_songs = json!({});
    let mut no_albums = json!({});
    let mut stats = json!({});
    let mut sync_report = SyncReport::default();

    info!("retrieving destination playlists...");
    let mut dst_playlists = dst_api.get_playlists_full().await?;
    let mut dst_likes = vec![];
    if config.like_all {
        info!("retrieving destination likes...");
        dst_likes = dst_api.get_likes().await?;
    }

    for mut src_playlist in src_playlists
        .into_iter()
        .filter(|p| !SKIPPED_PLAYLISTS.contains(&p.name.as_str()) && !p.songs.is_empty())
    {
        if src_playlist.songs.is_empty() {
            continue;
        }

        let mut dst_playlist = match dst_playlists
            .iter()
            .position(|p| p.name == src_playlist.name)
        {
            Some(i) => dst_playlists.remove(i),
            None => dst_api.create_playlist(&src_playlist.name, false).await?,
        };

        let mut missing_songs = json!([]);
        let mut new_songs = json!([]);
        let mut no_albums_songs = json!([]);
        let mut matched_songs = vec![];
        let mut not_synced_tracks = vec![];
        let mut already_synced_tracks = 0;
        let mut newly_synced_tracks = 0;
        let mut success = 0;
        let mut attempts = 0;
        let original_track_count = src_playlist.songs.len();

        let duplicate_tracks_skipped = if dedup_songs(&mut src_playlist.songs) {
            warn!(
                "duplicates found in source playlist \"{}\", they will be skipped",
                src_playlist.name
            );
            original_track_count - src_playlist.songs.len()
        } else {
            0
        };

        info!("synchronizing playlist \"{}\" ...", src_playlist.name);

        // 1. Search for each song in the destination playlist
        for src_song in &src_playlist.songs {
            // already in destination playlist
            if dst_playlist.songs.contains(src_song) {
                already_synced_tracks += 1;
                continue;
            }
            // no album metadata == youtube video
            if src_song.album.is_none() {
                warn!(
                    "No album metadata for source song \"{}\", skipping",
                    src_song
                );
                if config.debug {
                    no_albums_songs
                        .as_array_mut()
                        .unwrap()
                        .push(json!(src_song));
                }
                not_synced_tracks.push(NotSyncedTrack {
                    reason: NotSyncedReason::MissingAlbumMetadata,
                    source_track: src_song.clone(),
                });
                continue;
            }

            attempts += 1;

            let dst_song = dst_api.search_song(src_song).await?;
            let Some(dst_song) = dst_song else {
                debug!("no match found for song: {}", src_song);
                if config.debug {
                    missing_songs.as_array_mut().unwrap().push(json!(src_song));
                }
                not_synced_tracks.push(NotSyncedTrack {
                    reason: NotSyncedReason::NoMatchFound,
                    source_track: src_song.clone(),
                });
                continue;
            };
            matched_songs.push((src_song.clone(), dst_song));
            success += 1;
        }

        // 2. Add missing songs to the destination playlist
        if !matched_songs.is_empty() {
            let mut to_sync = Vec::new();
            for (src_song, dst_song) in &matched_songs {
                // HACK: takes into account discrepancy for YtMusic with no ISRC
                if dst_playlist.songs.contains(dst_song) {
                    debug!(
                        "discrepancy, song already in destination playlist: {}",
                        dst_song
                    );
                    attempts -= 1;
                    success -= 1;
                    already_synced_tracks += 1;
                    continue;
                }
                // Edge case: same song on different album/single that all resolve to the same
                // song on the destination platform resulting in duplicates
                if to_sync.contains(dst_song) {
                    debug!(
                        "discrepancy, duplicate song in songs to synchronize: {}",
                        dst_song
                    );
                    attempts -= 1;
                    success -= 1;
                    not_synced_tracks.push(NotSyncedTrack {
                        reason: NotSyncedReason::DuplicateDestinationMatch,
                        source_track: src_song.clone(),
                    });
                    continue;
                }
                if config.debug {
                    new_songs.as_array_mut().unwrap().push(json!(dst_song));
                }
                to_sync.push(dst_song.clone());
            }
            if !to_sync.is_empty() {
                debug!(
                    "adding {} songs to destination playlist \"{}\"",
                    to_sync.len(),
                    dst_playlist.name
                );
                dst_api
                    .add_songs_to_playlist(&mut dst_playlist, &to_sync)
                    .await?;
                newly_synced_tracks = to_sync.len();

                // like all songs that were added
                if config.like_all {
                    let new_likes = to_sync
                        .iter()
                        .filter(|s| !dst_likes.contains(s))
                        .cloned()
                        .collect::<Vec<Song>>();
                    dst_api.add_likes(&new_likes).await?;
                }
            }
        }

        let mut conversion_rate = 1.0;
        if attempts != 0 {
            conversion_rate = f64::from(success) / f64::from(attempts);
            info!(
                "synchronizing playlist \"{}\" [ok], {}/{} songs ({:.2}%)",
                src_playlist.name,
                success,
                attempts,
                conversion_rate * 100.0
            );
        } else {
            info!(
                "synchronizing playlist \"{}\" [ok], no new songs to add",
                src_playlist.name
            );
        }

        let source_tracks = src_playlist.songs.len();
        let successful_tracks = source_tracks - not_synced_tracks.len();
        let success_rate = sync_success_rate(successful_tracks, source_tracks)?;
        sync_report.playlists.push(PlaylistSyncReport {
            name: src_playlist.name.clone(),
            source_playlist_id: src_playlist.id.clone(),
            destination_playlist_id: dst_playlist.id.clone(),
            source_tracks,
            duplicate_tracks_skipped,
            already_synced_tracks,
            newly_synced_tracks,
            not_synced_tracks_count: not_synced_tracks.len(),
            success_rate,
            not_synced_tracks,
        });

        if config.debug {
            stats.as_object_mut().unwrap().insert(
                src_playlist.name.clone(),
                json!({
                    "percentage": conversion_rate,
                    "number": format!("{}/{}", success, attempts),
                }),
            );
            std::fs::write(
                "debug/conversion_rate.json",
                serde_json::to_string_pretty(&stats)?,
            )?;

            if !new_songs.as_array().unwrap().is_empty() {
                all_new_songs
                    .as_object_mut()
                    .unwrap()
                    .insert(src_playlist.name.clone(), new_songs);
                std::fs::write(
                    "debug/new_songs.json",
                    serde_json::to_string_pretty(&all_new_songs)?,
                )?;
            }

            if !missing_songs.as_array().unwrap().is_empty() {
                all_missing_songs
                    .as_object_mut()
                    .unwrap()
                    .insert(src_playlist.name.clone(), missing_songs);
                std::fs::write(
                    "debug/missing_songs.json",
                    serde_json::to_string_pretty(&all_missing_songs)?,
                )?;
            }

            if !no_albums_songs.as_array().unwrap().is_empty() {
                no_albums
                    .as_object_mut()
                    .unwrap()
                    .insert(src_playlist.name.clone(), no_albums_songs);
                std::fs::write(
                    "debug/song_with_no_albums.json",
                    serde_json::to_string_pretty(&no_albums)?,
                )?;
            }
        }
    }

    let sync_report_path = write_sync_report(&sync_report, &config.sync_report)?;
    info!("sync report written to: {:?}", sync_report_path);
    info!("Synchronization complete!");

    Ok(())
}

fn sync_success_rate(successful_tracks: usize, source_tracks: usize) -> Result<f64> {
    if source_tracks == 0 {
        return Ok(1.0);
    }

    let successful_tracks = u32::try_from(successful_tracks)?;
    let source_tracks = u32::try_from(source_tracks)?;
    Ok(f64::from(successful_tracks) / f64::from(source_tracks))
}

fn write_sync_report(report: &SyncReport, output: &Path) -> Result<PathBuf> {
    let output = timestamped_report_path(output)?;
    let report = serde_json::to_string_pretty(report)?;

    for attempt in 0.. {
        let output = if attempt == 0 {
            output.clone()
        } else {
            path_with_stem_suffix(&output, &format!("_{attempt}"))
        };
        if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        match OpenOptions::new().write(true).create_new(true).open(&output) {
            Ok(mut file) => {
                file.write_all(report.as_bytes())?;
                return Ok(output);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }

    unreachable!("unbounded sync report filename retry loop exhausted")
}

fn timestamped_report_path(output: &Path) -> Result<PathBuf> {
    let timestamp = OffsetDateTime::now_utc().format(format_description!(
        "[year][month][day]T[hour][minute][second]Z"
    ))?;
    Ok(path_with_timestamp(output, &timestamp))
}

fn path_with_timestamp(output: &Path, timestamp: &str) -> PathBuf {
    path_with_stem_suffix(output, &format!("_{timestamp}"))
}

fn path_with_stem_suffix(output: &Path, suffix: &str) -> PathBuf {
    let mut file_name = output
        .file_stem()
        .map_or_else(|| OsString::from("sync_report"), OsString::from);
    file_name.push(suffix);
    file_name.push(".");
    file_name.push(
        output
            .extension()
            .unwrap_or_else(|| std::ffi::OsStr::new("json")),
    );

    let mut timestamped = output.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    timestamped.push(file_name);
    timestamped
}

pub async fn synchronize_likes(src_api: &DynMusicApi, dst_api: &DynMusicApi) -> Result<()> {
    info!("retrieving source likes...");
    let src_likes = src_api.get_likes().await?;
    info!("retrieving destination likes...");
    let dst_likes = dst_api.get_likes().await?;

    let mut new_likes = Vec::new();
    let mut success = 0;
    let mut attempts = 0;

    info!("searching for all missing likes on destination platform...");
    for src_like in src_likes {
        if dst_likes.contains(&src_like) {
            continue;
        }
        attempts += 1;
        let Some(song) = dst_api.search_song(&src_like).await? else {
            debug!("no match found for song: {}", src_like);
            continue;
        };
        // HACK: takes into account discrepancy for YtMusic with no ISRC
        if dst_likes.contains(&song) {
            attempts -= 1;
            debug!("discrepancy, song already liked: {}", song);
            continue;
        }
        success += 1;
        new_likes.push(song);
    }

    if attempts != 0 {
        let conversion_rate = f64::from(success) / f64::from(attempts);
        info!(
            "synchronizing {}/{} ({:.2}%) new likes",
            success,
            attempts,
            conversion_rate * 100.0
        );
        dst_api.add_likes(&new_likes).await?;
        info!("[ok] synchronized new likes");
    } else {
        info!("[ok] no new likes to synchronize");
    }

    Ok(())
}
