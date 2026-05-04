/// Spotify Web API helpers — playlist and library access.
///
/// Thin wrappers around `rspotify` for fetching user playlists and
/// their contents.
///
/// Token expiry: `AuthCodeSpotify` is configured with `token_refreshing: true`
/// (see `auth::create_spotify_client`). `BaseClient::auth_headers` refreshes
/// the access token via the refresh token when needed — callers do not need
/// to wrap each call with manual expiry checks.
use anyhow::{Context, Result, anyhow};
use rspotify::{
    AuthCodeSpotify,
    clients::OAuthClient,
    model::{FullTrack, Market, TrackId},
    prelude::*,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

/// Synthetic playlist id for the user's saved tracks (Spotify "Liked Songs").
pub const ONYX_LIKED_SONGS_ID: &str = "onyx:liked-songs";

/// Prefix for synthetic queue ids when playing from an artist's popular tracks.
pub const ONYX_ARTIST_QUEUE_PREFIX: &str = "onyx:artist:";

#[inline]
pub fn artist_queue_playlist_id(artist_id: &str) -> String {
    format!("{ONYX_ARTIST_QUEUE_PREFIX}{artist_id}")
}

#[inline]
pub fn is_artist_queue_playlist(id: &str) -> bool {
    id.starts_with(ONYX_ARTIST_QUEUE_PREFIX)
}

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistTrack {
    pub position: u32,
    pub name: String,
    pub artist: String,
    /// Spotify artist id for the first credited artist (for navigation).
    pub artist_id: Option<String>,
    pub album: String,
    pub album_image_url: Option<String>,
    pub album_thumbnail_url: Option<String>,
    pub added_at: Option<String>,
    pub duration_ms: u32,
    pub spotify_uri: String,
}

/// Artist header data for the artist page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistProfile {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub thumbnail_url: Option<String>,
    pub followers: u32,
}

/// One row in the discography section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistAlbumSummary {
    pub id: String,
    pub name: String,
    pub album_type_label: String,
    pub release_year: String,
    pub thumbnail_url: Option<String>,
}

/// Sidebar row for Liked Songs (not a real Spotify playlist id).
pub fn liked_songs_summary(track_count: u32) -> PlaylistSummary {
    PlaylistSummary {
        id: ONYX_LIKED_SONGS_ID.to_string(),
        name: "Liked Songs".to_string(),
        track_count,
        image_url: None,
        thumbnail_url: None,
        owner_name: Some("You".to_string()),
        public_label: "Playlist".to_string(),
        snapshot_id: None,
    }
}

#[inline]
pub fn is_liked_songs_playlist(id: &str) -> bool {
    id == ONYX_LIKED_SONGS_ID
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

/// Total count of saved tracks (one lightweight `me/tracks` page request).
pub async fn user_saved_tracks_total(spotify: &AuthCodeSpotify) -> Result<u32> {
    let page = spotify
        .current_user_saved_tracks_manual(None, Some(1), Some(0))
        .await
        .context("Failed to fetch saved tracks count")?;
    Ok(page.total)
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
        .enumerate()
        .filter_map(|(position, item)| {
            let t = item.track;
            let artist = t
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let artist_id = t
                .artists
                .first()
                .and_then(|a| a.id.as_ref())
                .map(|id| id.to_string());
            let uri = t.id.as_ref()?.uri();
            Some(PlaylistTrack {
                position: position as u32,
                name: t.name,
                artist,
                artist_id,
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

/// Map a full track to [`PlaylistTrack`] (e.g. artist top tracks).
pub fn full_track_to_playlist_track(
    t: rspotify::model::FullTrack,
    position: u32,
    primary_artist_id_override: Option<&str>,
) -> Option<PlaylistTrack> {
    let id = t.id.as_ref()?;
    let artist = t
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let artist_id = primary_artist_id_override
        .map(|s| s.to_string())
        .or_else(|| {
            t.artists
                .first()
                .and_then(|a| a.id.as_ref())
                .map(|aid| aid.to_string())
        });
    Some(PlaylistTrack {
        position,
        name: t.name,
        artist,
        artist_id,
        album: t.album.name,
        album_image_url: best_image_url(&t.album.images, 160),
        album_thumbnail_url: best_image_url(&t.album.images, 36),
        added_at: None,
        duration_ms: t.duration.num_milliseconds() as u32,
        spotify_uri: id.uri(),
    })
}

/// Fetch full tracks for Spotify track URIs in the same order as `uris` (sparse skips).
pub async fn fetch_full_tracks_for_uris_ordered(
    spotify: &AuthCodeSpotify,
    uris: &[String],
    market: Option<Market>,
) -> Result<Vec<Option<FullTrack>>> {
    let mut out: Vec<Option<FullTrack>> = vec![None; uris.len()];
    let mut indexed: Vec<(usize, TrackId<'static>)> = Vec::new();
    for (i, uri) in uris.iter().enumerate() {
        if uri.is_empty() {
            continue;
        }
        let tid = match TrackId::from_uri(uri.as_str()) {
            Ok(t) => t.into_static(),
            Err(_) => continue,
        };
        indexed.push((i, tid));
    }

    for chunk in indexed.chunks(50) {
        let idxs: Vec<usize> = chunk.iter().map(|(i, _)| *i).collect();
        let ids: Vec<TrackId<'static>> = chunk.iter().map(|(_, t)| t.clone()).collect();
        #[allow(deprecated)]
        let got = spotify
            .tracks(ids.iter().map(|t| t.clone()), market)
            .await;
        match got {
            Ok(tracks) => {
                for (k, ft) in tracks.into_iter().enumerate() {
                    if let Some(&ix) = idxs.get(k) {
                        out[ix] = Some(ft);
                    }
                }
            }
            Err(e) => {
                log::debug!("get several tracks failed ({e:#}); fetching individually");
                for (k, tid) in ids.into_iter().enumerate() {
                    let ix = idxs[k];
                    match spotify.track(tid, market).await {
                        Ok(ft) => out[ix] = Some(ft),
                        Err(e2) => log::debug!("track {ix}: {e2:#}"),
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Last.fm [`crate::metadata::ResolvedTrack`] rows enriched with Spotify track metadata.
pub async fn popular_tracks_from_resolved(
    spotify: &AuthCodeSpotify,
    resolved: Vec<crate::metadata::ResolvedTrack>,
    primary_artist_id: &str,
    market: Option<Market>,
) -> Result<Vec<PlaylistTrack>> {
    let uris: Vec<String> = resolved
        .iter()
        .map(|r| r.spotify_uri.clone().unwrap_or_default())
        .collect();
    let fetched = fetch_full_tracks_for_uris_ordered(spotify, &uris, market).await?;

    let mut out = Vec::with_capacity(resolved.len());
    for (i, res) in resolved.into_iter().enumerate() {
        let pos = i as u32;
        let row = if let Some(Some(ft)) = fetched.get(i) {
            full_track_to_playlist_track(ft.clone(), pos, Some(primary_artist_id))
        } else {
            None
        };
        out.push(row.unwrap_or_else(|| PlaylistTrack {
            position: pos,
            name: res.track.clone(),
            artist: res.artist.clone(),
            artist_id: Some(primary_artist_id.to_string()),
            album: String::new(),
            album_image_url: None,
            album_thumbnail_url: None,
            added_at: None,
            duration_ms: 0,
            spotify_uri: res.spotify_uri.unwrap_or_default(),
        }));
    }
    Ok(out)
}

/// Load artist profile (name, images, follower count).
pub async fn fetch_artist_profile(
    spotify: &AuthCodeSpotify,
    artist_id: &str,
) -> Result<ArtistProfile> {
    use rspotify::model::ArtistId;

    let id_str = artist_id.split(':').last().unwrap_or(artist_id);
    let aid = ArtistId::from_id(id_str).context("Invalid artist ID")?;
    #[allow(deprecated)]
    let a = spotify
        .artist(aid)
        .await
        .context("Failed to fetch artist")?;
    #[allow(deprecated)]
    let followers = a.followers.total;
    Ok(ArtistProfile {
        id: a.id.to_string(),
        name: a.name,
        image_url: best_image_url(&a.images, 320),
        thumbnail_url: best_image_url(&a.images, 64),
        followers,
    })
}

/// `market` query for catalog endpoints (`/artists/{id}/top-tracks`, `/artists/{id}/albums`).
/// `Market::FromToken` often fails for dev apps or when `country` is absent; prefer explicit ISO.
pub async fn catalog_market(spotify: &AuthCodeSpotify) -> Market {
    use rspotify::model::Country;

    match spotify.current_user().await {
        Ok(user) => {
            #[allow(deprecated)]
            if let Some(c) = user.country {
                return Market::Country(c);
            }
        }
        Err(e) => log::debug!("current_user for catalog market: {:#}", e),
    }
    Market::Country(Country::UnitedStates)
}

/// Top tracks for an artist (Spotify catalog; market from user token).
pub async fn fetch_artist_top_tracks(
    spotify: &AuthCodeSpotify,
    artist_id: &str,
    market: Market,
) -> Result<Vec<PlaylistTrack>> {
    use rspotify::model::ArtistId;

    let id_str = artist_id.split(':').last().unwrap_or(artist_id);
    let aid = ArtistId::from_id(id_str).context("Invalid artist ID")?;
    // Spotify still serves this endpoint; rspotify deprecates after Web API churn.
    #[allow(deprecated)]
    let tracks = spotify
        .artist_top_tracks(aid, Some(market))
        .await
        .context("Failed to fetch artist top tracks")?;
    Ok(tracks
        .into_iter()
        .enumerate()
        .filter_map(|(i, t)| full_track_to_playlist_track(t, i as u32, Some(id_str)))
        .collect())
}

/// Albums, singles, compilations, and appearances — deduped by album id.
pub async fn fetch_artist_albums(
    spotify: &AuthCodeSpotify,
    artist_id: &str,
    market: Market,
) -> Result<Vec<ArtistAlbumSummary>> {
    use futures_util::TryStreamExt;
    use rspotify::model::{AlbumType, ArtistId};

    let id_str = artist_id.split(':').last().unwrap_or(artist_id);
    let aid = ArtistId::from_id(id_str).context("Invalid artist ID")?;
    let groups = [
        AlbumType::Album,
        AlbumType::Single,
        AlbumType::Compilation,
        AlbumType::AppearsOn,
    ];
    let stream = spotify.artist_albums(aid, groups, Some(market));
    let items: Vec<rspotify::model::SimplifiedAlbum> = stream
        .try_collect()
        .await
        .context("Failed to fetch artist albums")?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for a in items {
        let Some(album_id) = a.id.as_ref() else {
            continue;
        };
        let key = album_id.to_string();
        if !seen.insert(key) {
            continue;
        }
        let release_year = a
            .release_date
            .as_deref()
            .and_then(|d| d.get(0..4))
            .unwrap_or("—")
            .to_string();
        let album_type_label = a
            .album_type
            .clone()
            .unwrap_or_else(|| "album".to_string())
            .replace('_', " ");
        out.push(ArtistAlbumSummary {
            id: album_id.to_string(),
            name: a.name,
            album_type_label,
            release_year,
            thumbnail_url: best_image_url(&a.images, 64),
        });
    }
    Ok(out)
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
    let artist_id = t
        .artists
        .first()
        .and_then(|a| a.id.as_ref())
        .map(|id| id.to_string());
    let id = t.id.as_ref()?;

    Some(PlaylistTrack {
        position,
        name: t.name,
        artist,
        artist_id,
        album: t.album.name,
        album_image_url: best_image_url(&t.album.images, 160),
        album_thumbnail_url: best_image_url(&t.album.images, 36),
        added_at,
        duration_ms: t.duration.num_milliseconds() as u32,
        spotify_uri: id.uri(),
    })
}

/// Large + thumbnail URLs from a full track's album art.
pub fn full_track_artwork_urls(t: &rspotify::model::FullTrack) -> (Option<String>, Option<String>) {
    (
        best_image_url(&t.album.images, 160),
        best_image_url(&t.album.images, 36),
    )
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
