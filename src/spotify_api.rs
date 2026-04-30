/// Spotify Web API helpers — playlist and library access.
///
/// Thin wrappers around `rspotify` for fetching user playlists and
/// their contents.
use anyhow::{Context, Result, anyhow};
use rspotify::{AuthCodeSpotify, prelude::*};
use std::time::Duration;

const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
const PLAYLIST_BATCH_DELAY: Duration = Duration::from_millis(150);

pub fn cache_only_mode() -> bool {
    std::env::var("ONYX_CACHE_ONLY")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

// ───────────────────────────────────────────────────────────────────
// Data models
// ───────────────────────────────────────────────────────────────────

/// A summary of a user's playlist (for list views).
#[derive(Debug, Clone)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub track_count: u32,
    pub image_url: Option<String>,
    pub thumbnail_url: Option<String>,
    pub owner_name: Option<String>,
    pub public_label: String,
    pub snapshot_id: Option<String>,
}

/// A track within a playlist.
#[derive(Debug, Clone)]
pub struct PlaylistTrack {
    pub position: u32,
    pub name: String,
    pub artist: String,
    pub album: String,
    pub album_image_url: Option<String>,
    pub album_thumbnail_url: Option<String>,
    pub added_at: Option<String>,
    pub duration_ms: u32,
    pub spotify_uri: String,
}

#[derive(Debug, Clone, Copy)]
pub struct RateLimitInfo {
    pub retry_after: Duration,
}

pub fn rate_limit_info(error: &anyhow::Error) -> Option<RateLimitInfo> {
    let error_text = error
        .chain()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    rate_limit_info_from_text(&error_text)
}

pub fn rate_limit_info_from_text(error_text: &str) -> Option<RateLimitInfo> {
    if !error_text.contains("429") && !error_text.contains("Too Many Requests") {
        return None;
    }

    Some(RateLimitInfo {
        retry_after: retry_after_from_text(error_text).unwrap_or(DEFAULT_RETRY_AFTER),
    })
}

pub fn rate_limit_status_message(error: &anyhow::Error) -> Option<String> {
    rate_limit_info(error).map(|info| {
        format!(
            "Spotify rate limited this request. Retrying is safer after about {} seconds.",
            info.retry_after.as_secs()
        )
    })
}

// ───────────────────────────────────────────────────────────────────
// Functions
// ───────────────────────────────────────────────────────────────────

/// Fetch all of the authenticated user's playlists.
pub async fn user_playlists(spotify: &AuthCodeSpotify) -> Result<Vec<PlaylistSummary>> {
    use futures_util::TryStreamExt;

    let stream = spotify.current_user_playlists();
    let playlists: Vec<_> = stream
        .try_collect()
        .await
        .context("Failed to fetch user playlists")?;

    let summaries = playlists
        .into_iter()
        .map(|p| PlaylistSummary {
            id: p.id.to_string(),
            name: p.name,
            track_count: p.items.total,
            image_url: best_image_url(&p.images, 160),
            thumbnail_url: best_image_url(&p.images, 48),
            owner_name: p.owner.display_name,
            public_label: if p.public.unwrap_or(false) {
                "Public Playlist".to_string()
            } else {
                "Playlist".to_string()
            },
            snapshot_id: Some(p.snapshot_id),
        })
        .collect();

    Ok(summaries)
}

pub async fn playlist_tracks(
    spotify: &AuthCodeSpotify,
    playlist_id: &str,
) -> Result<Vec<PlaylistTrack>> {
    use futures_util::StreamExt;
    use rspotify::model::PlaylistId;

    let id_str = playlist_id.split(':').last().unwrap_or(playlist_id);
    let id = PlaylistId::from_id(id_str).context("Invalid playlist ID")?;

    let mut stream = spotify.playlist_items(id, None, None);
    let mut tracks = Vec::new();
    let mut position = 0_u32;

    while let Some(item_res) = stream.next().await {
        match item_res {
            Ok(item) => {
                if let Some(track) = playlist_item_to_track(item, position) {
                    tracks.push(track);
                    position += 1;
                }
            }
            Err(e) => {
                log::warn!("Skipping a track due to error (often local files): {}", e);
            }
        }
    }

    Ok(tracks)
}

pub async fn playlist_tracks_batched<F>(
    spotify: &AuthCodeSpotify,
    playlist_id: &str,
    mut on_batch: F,
) -> Result<Vec<PlaylistTrack>>
where
    F: FnMut(Vec<PlaylistTrack>) + Send,
{
    use futures_util::StreamExt;
    use rspotify::model::PlaylistId;

    let id_str = playlist_id.split(':').last().unwrap_or(playlist_id);
    let id = PlaylistId::from_id(id_str).context("Invalid playlist ID")?;

    let mut stream = spotify.playlist_items(id, None, None);
    let mut tracks = Vec::new();
    let mut batch = Vec::new();
    let mut position = 0_u32;

    while let Some(item_res) = stream.next().await {
        match item_res {
            Ok(item) => {
                if let Some(track) = playlist_item_to_track(item, position) {
                    tracks.push(track.clone());
                    batch.push(track);
                    position += 1;

                    if batch.len() >= 100 {
                        on_batch(std::mem::take(&mut batch));
                        tokio::time::sleep(PLAYLIST_BATCH_DELAY).await;
                    }
                }
            }
            Err(e) => {
                if rate_limit_info_from_text(&e.to_string()).is_some() {
                    return Err(anyhow!("Failed to fetch playlist tracks: {}", e));
                }
                log::warn!("Skipping a track due to error (often local files): {}", e);
            }
        }
    }

    if !batch.is_empty() {
        on_batch(batch);
    }

    Ok(tracks)
}

/// Fetch all of the authenticated user's saved tracks (Liked Songs).
pub async fn user_saved_tracks(spotify: &AuthCodeSpotify) -> Result<Vec<PlaylistTrack>> {
    use futures_util::TryStreamExt;

    let stream = spotify.current_user_saved_tracks(None);
    let items: Vec<_> = stream
        .try_collect()
        .await
        .context("Failed to fetch saved tracks")?;

    let tracks = items
        .into_iter()
        .filter_map(|item| {
            let t = item.track;
            let artist = t
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let uri = t.id.as_ref()?.uri();
            Some(PlaylistTrack {
                position: 0,
                name: t.name,
                artist,
                album: t.album.name,
                album_image_url: best_image_url(&t.album.images, 160),
                album_thumbnail_url: best_image_url(&t.album.images, 36),
                added_at: Some(item.added_at.to_rfc3339()),
                duration_ms: t.duration.num_milliseconds() as u32,
                spotify_uri: uri,
            })
        })
        .collect();

    Ok(tracks)
}

/// Create a new playlist for the current user.
pub async fn create_playlist(
    spotify: &AuthCodeSpotify,
    name: &str,
    description: Option<&str>,
    public: bool,
) -> Result<String> {
    let user = spotify
        .current_user()
        .await
        .context("Failed to fetch current user")?;
    let user_id = user.id;

    let playlist = spotify
        .user_playlist_create(user_id, name, Some(public), Some(false), description)
        .await
        .context("Failed to create playlist")?;

    Ok(playlist.id.to_string())
}

/// Add tracks to a playlist.
pub async fn add_to_playlist(
    spotify: &AuthCodeSpotify,
    playlist_id: &str,
    track_uris: &[String],
) -> Result<()> {
    use rspotify::model::{PlayableId, PlaylistId, TrackId};

    let p_id = PlaylistId::from_id(playlist_id).context("Invalid playlist ID")?;

    let mut track_ids = Vec::new();
    for uri in track_uris {
        let id_str = uri.split(':').last().unwrap_or(uri);
        if let Ok(tid) = TrackId::from_id(id_str) {
            track_ids.push(tid);
        }
    }

    let playables: Vec<PlayableId> = track_ids
        .iter()
        .map(|tid| PlayableId::Track(tid.clone()))
        .collect();

    if !playables.is_empty() {
        spotify
            .playlist_add_items(p_id, playables, None)
            .await
            .context("Failed to add tracks to playlist")?;
    }

    Ok(())
}

fn playlist_item_to_track(
    item: rspotify::model::PlaylistItem,
    position: u32,
) -> Option<PlaylistTrack> {
    use rspotify::model::PlayableItem;

    let added_at = item.added_at.map(|dt| dt.to_rfc3339());
    let playable = item.item?;
    let PlayableItem::Track(t) = playable else {
        return None;
    };
    let artist = t
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let id = t.id.as_ref()?;

    Some(PlaylistTrack {
        position,
        name: t.name,
        artist,
        album: t.album.name,
        album_image_url: best_image_url(&t.album.images, 160),
        album_thumbnail_url: best_image_url(&t.album.images, 36),
        added_at,
        duration_ms: t.duration.num_milliseconds() as u32,
        spotify_uri: id.uri(),
    })
}

fn best_image_url(images: &[rspotify::model::Image], target_size: u32) -> Option<String> {
    images
        .iter()
        .filter_map(|image| {
            let width = image.width.unwrap_or(target_size);
            let height = image.height.unwrap_or(width);
            let size = width.max(height);
            let penalty = size.abs_diff(target_size);
            Some((penalty, size < target_size, image.url.clone()))
        })
        .min_by_key(|(penalty, is_too_small, _)| (*is_too_small, *penalty))
        .map(|(_, _, url)| url)
}

fn retry_after_from_text(error_text: &str) -> Option<Duration> {
    let lower = error_text.to_ascii_lowercase();
    let marker = "retry-after";
    let idx = lower.find(marker)?;
    let after_marker = &error_text[idx + marker.len()..];
    let seconds = after_marker
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.max(1)))
}

/// Remove tracks from a playlist.
pub async fn remove_from_playlist(
    spotify: &AuthCodeSpotify,
    playlist_id: &str,
    track_uris: &[String],
) -> Result<()> {
    use rspotify::model::{PlayableId, PlaylistId, TrackId};

    let p_id = PlaylistId::from_id(playlist_id).context("Invalid playlist ID")?;

    let mut track_ids = Vec::new();
    for uri in track_uris {
        let id_str = uri.split(':').last().unwrap_or(uri);
        if let Ok(tid) = TrackId::from_id(id_str) {
            track_ids.push(tid);
        }
    }

    let playables: Vec<PlayableId> = track_ids
        .iter()
        .map(|tid| PlayableId::Track(tid.clone()))
        .collect();

    if !playables.is_empty() {
        spotify
            .playlist_remove_all_occurrences_of_items(p_id, playables, None)
            .await
            .context("Failed to remove tracks from playlist")?;
    }

    Ok(())
}
