/// Spotify Web API helpers — playlist and library access.
///
/// Thin wrappers around `rspotify` for fetching user playlists and
/// their contents.

use anyhow::{Context, Result};
use rspotify::{prelude::*, AuthCodeSpotify};

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
}

/// A track within a playlist.
#[derive(Debug, Clone)]
pub struct PlaylistTrack {
    pub name: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub spotify_uri: String,
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
            image_url: p.images.first().map(|img| img.url.clone()),
        })
        .collect();

    Ok(summaries)
}

/// Fetch all tracks from a specific playlist.
pub async fn playlist_tracks(
    spotify: &AuthCodeSpotify,
    playlist_id: &str,
) -> Result<Vec<PlaylistTrack>> {
    use futures_util::TryStreamExt;
    use rspotify::model::{PlayableItem, PlaylistId};

    let id = PlaylistId::from_id(playlist_id)
        .context("Invalid playlist ID")?;

    let stream = spotify.playlist_items(id, None, None);
    let items: Vec<_> = stream
        .try_collect()
        .await
        .context("Failed to fetch playlist tracks")?;

    let tracks = items
        .into_iter()
        .filter_map(|item| {
            let playable = item.item?;
            match playable {
                PlayableItem::Track(t) => {
                    let artist = t
                        .artists
                        .first()
                        .map(|a| a.name.clone())
                        .unwrap_or_default();
                    let uri = t.id.as_ref()?.uri();
                    Some(PlaylistTrack {
                        name: t.name,
                        artist,
                        album: t.album.name,
                        duration_ms: t.duration.num_milliseconds() as u32,
                        spotify_uri: uri,
                    })
                }
                _ => None, // anti-bloat: skip podcasts & unknown
            }
        })
        .collect();

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
                name: t.name,
                artist,
                album: t.album.name,
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
    use rspotify::model::UserId;

    let user = spotify.current_user().await.context("Failed to fetch current user")?;
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
    
    let playables: Vec<PlayableId> = track_ids.iter().map(|tid| PlayableId::Track(tid.clone())).collect();

    if !playables.is_empty() {
        spotify
            .playlist_add_items(p_id, playables, None)
            .await
            .context("Failed to add tracks to playlist")?;
    }

    Ok(())
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
    
    let playables: Vec<PlayableId> = track_ids.iter().map(|tid| PlayableId::Track(tid.clone())).collect();

    if !playables.is_empty() {
        spotify
            .playlist_remove_all_occurrences_of_items(p_id, playables, None)
            .await
            .context("Failed to remove tracks from playlist")?;
    }

    Ok(())
}
