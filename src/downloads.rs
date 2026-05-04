use crate::player::{AudioCmd, AudioHandle};
use crate::playlist_cache::{PlaylistCache, PlaylistDownloadStatus};
use crate::spotify_api::{PlaylistSummary, PlaylistTrack};
use eframe::egui;
use rspotify::AuthCodeSpotify;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

pub const DOWNLOAD_DOWNLOADING: &str = "downloading";
pub const DOWNLOAD_DOWNLOADED: &str = "downloaded";
pub const DOWNLOAD_CANCELLED: &str = "cancelled";
pub const DOWNLOAD_ERROR: &str = "error";

pub type DownloadStatuses = Arc<Mutex<HashMap<String, PlaylistDownloadStatus>>>;

pub fn spawn_playlist_download(
    rt: &tokio::runtime::Handle,
    spotify: AuthCodeSpotify,
    audio: AudioHandle,
    statuses: DownloadStatuses,
    playlist: PlaylistSummary,
    cached_tracks: Vec<PlaylistTrack>,
    ctx: egui::Context,
) -> JoinHandle<()> {
    rt.spawn(async move {
        let playlist_id = playlist.id.clone();
        let mut tracks = cached_tracks;

        set_status(
            &statuses,
            PlaylistDownloadStatus {
                playlist_id: playlist_id.clone(),
                desired: true,
                state: DOWNLOAD_DOWNLOADING.to_string(),
                downloaded_count: 0,
                total_count: playlist.track_count,
                last_error: None,
                updated_at: None,
            },
        );
        ctx.request_repaint();

        if tracks.is_empty() && !crate::spotify_api::cache_only_mode() {
            match crate::spotify_api::playlist_tracks_batched(&spotify, &playlist_id, |batch| {
                if let Ok(mut cache) = PlaylistCache::new() {
                    if let Err(e) = cache.save_track_batch(&playlist_id, &batch) {
                        log::warn!("Failed to cache downloaded playlist tracks: {}", e);
                    }
                }
            })
            .await
            {
                Ok(fetched) => tracks = fetched,
                Err(e) => {
                    set_status(
                        &statuses,
                        PlaylistDownloadStatus {
                            playlist_id,
                            desired: true,
                            state: DOWNLOAD_ERROR.to_string(),
                            downloaded_count: 0,
                            total_count: playlist.track_count,
                            last_error: Some(e.to_string()),
                            updated_at: None,
                        },
                    );
                    ctx.request_repaint();
                    return;
                }
            }
        }

        let total = tracks.len() as u32;
        for (index, track) in tracks.iter().enumerate() {
            let _ = audio.send(AudioCmd::Preload {
                uri: track.spotify_uri.clone(),
            });
            set_status(
                &statuses,
                PlaylistDownloadStatus {
                    playlist_id: playlist_id.clone(),
                    desired: true,
                    state: DOWNLOAD_DOWNLOADING.to_string(),
                    downloaded_count: index as u32 + 1,
                    total_count: total,
                    last_error: None,
                    updated_at: None,
                },
            );
            ctx.request_repaint();
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        set_status(
            &statuses,
            PlaylistDownloadStatus {
                playlist_id,
                desired: true,
                state: DOWNLOAD_DOWNLOADED.to_string(),
                downloaded_count: total,
                total_count: total,
                last_error: None,
                updated_at: None,
            },
        );
        ctx.request_repaint();
    })
}

pub fn set_cancelled(statuses: &DownloadStatuses, playlist_id: &str) {
    set_status(
        statuses,
        PlaylistDownloadStatus {
            playlist_id: playlist_id.to_string(),
            desired: false,
            state: DOWNLOAD_CANCELLED.to_string(),
            downloaded_count: 0,
            total_count: 0,
            last_error: None,
            updated_at: None,
        },
    );
}

pub fn remove_download(statuses: &DownloadStatuses, playlist_id: &str) {
    if let Ok(mut statuses) = statuses.lock() {
        statuses.remove(playlist_id);
    }
    if let Ok(cache) = PlaylistCache::new() {
        if let Err(e) = cache.remove_download_status(playlist_id) {
            log::warn!("Failed to remove download status: {}", e);
        }
    }
}

fn set_status(statuses: &DownloadStatuses, status: PlaylistDownloadStatus) {
    if let Ok(mut statuses) = statuses.lock() {
        statuses.insert(status.playlist_id.clone(), status.clone());
    }
    if let Ok(cache) = PlaylistCache::new() {
        if let Err(e) = cache.save_download_status(&status) {
            log::warn!("Failed to save download status: {}", e);
        }
    }
}
