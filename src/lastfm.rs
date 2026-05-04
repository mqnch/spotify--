/// Last.fm REST API client.
///
/// Provides async methods for `track.search`, `artist.getInfo`,
/// `artist.getTopTracks`, and `artist.getTopAlbums`. Uses `reqwest` + `serde`
/// for JSON deserialization.

use anyhow::{Context, Result};
use serde::Deserialize;

const BASE_URL: &str = "https://ws.audioscrobbler.com/2.0/";

// ───────────────────────────────────────────────────────────────────
// Client
// ───────────────────────────────────────────────────────────────────

pub struct LastFmClient {
    api_key: String,
    http: reqwest::Client,
}

impl LastFmClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            http: reqwest::Client::new(),
        }
    }

    // ── track.search ─────────────────────────────────────────────────

    /// Search for tracks by name, optionally narrowed by artist.
    pub async fn search_tracks(
        &self,
        query: &str,
        artist: Option<&str>,
        limit: u32,
    ) -> Result<Vec<TrackMatch>> {
        let mut params = vec![
            ("method", "track.search".to_string()),
            ("track", query.to_string()),
            ("api_key", self.api_key.clone()),
            ("format", "json".to_string()),
            ("limit", limit.to_string()),
            ("autocorrect", "1".to_string()),
        ];
        if let Some(a) = artist {
            params.push(("artist", a.to_string()));
        }

        let resp = self
            .http
            .get(BASE_URL)
            .query(&params)
            .send()
            .await
            .context("Last.fm track.search request failed")?;

        let body: TrackSearchResponse = resp
            .json()
            .await
            .context("Failed to parse track.search response")?;

        Ok(body
            .results
            .trackmatches
            .track
            .unwrap_or_default())
    }

    // ── artist.getInfo ───────────────────────────────────────────────

    /// Get metadata for an artist (bio, stats, similar artists, tags).
    pub async fn artist_info(&self, artist: &str) -> Result<ArtistInfo> {
        let params = [
            ("method", "artist.getInfo"),
            ("artist", artist),
            ("api_key", &self.api_key),
            ("format", "json"),
            ("autocorrect", "1"),
        ];

        let resp = self
            .http
            .get(BASE_URL)
            .query(&params)
            .send()
            .await
            .context("Last.fm artist.getInfo request failed")?;

        let body: ArtistInfoResponse = resp
            .json()
            .await
            .context("Failed to parse artist.getInfo response")?;

        Ok(body.artist)
    }

    // ── artist.getTopTracks ──────────────────────────────────────────

    /// Get the top tracks for an artist, ordered by popularity.
    pub async fn artist_top_tracks(
        &self,
        artist: &str,
        limit: u32,
    ) -> Result<Vec<TopTrack>> {
        let limit_str = limit.to_string();
        let params = [
            ("method", "artist.getTopTracks"),
            ("artist", artist),
            ("api_key", &self.api_key),
            ("format", "json"),
            ("limit", &limit_str),
            ("autocorrect", "1"),
        ];

        let resp = self
            .http
            .get(BASE_URL)
            .query(&params)
            .send()
            .await
            .context("Last.fm artist.getTopTracks request failed")?;

        let body: TopTracksResponse = resp
            .json()
            .await
            .context("Failed to parse artist.getTopTracks response")?;

        Ok(body.toptracks.track.unwrap_or_default())
    }

    // ── artist.getTopAlbums ─────────────────────────────────────────

    /// Top albums for an artist (Last.fm charts).
    pub async fn artist_top_albums(&self, artist: &str, limit: u32) -> Result<Vec<TopAlbum>> {
        let limit_str = limit.to_string();
        let params = [
            ("method", "artist.getTopAlbums"),
            ("artist", artist),
            ("api_key", &self.api_key),
            ("format", "json"),
            ("limit", &limit_str),
            ("autocorrect", "1"),
        ];

        let resp = self
            .http
            .get(BASE_URL)
            .query(&params)
            .send()
            .await
            .context("Last.fm artist.getTopAlbums request failed")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse artist.getTopAlbums response")?;

        let album_val = body
            .pointer("/topalbums/album")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let mut albums: Vec<TopAlbum> = match album_val {
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect(),
            serde_json::Value::Object(_) => serde_json::from_value(album_val)
                .map(|a: TopAlbum| vec![a])
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        albums.truncate(limit as usize);
        Ok(albums)
    }
}

/// Pick the largest Last.fm image URL (by declared size).
pub fn best_lastfm_image_url(images: &[LfImage]) -> Option<String> {
    let rank = |s: &str| match s {
        "mega" => 6,
        "extralarge" => 5,
        "large" => 4,
        "medium" => 3,
        "small" => 2,
        _ => 1,
    };
    images
        .iter()
        .filter(|im| !im.url.is_empty())
        .max_by_key(|im| rank(im.size.as_str()))
        .map(|im| im.url.clone())
}

// ───────────────────────────────────────────────────────────────────
// Response models — shaped to match Last.fm's nested JSON
// ───────────────────────────────────────────────────────────────────

// -- track.search --

#[derive(Debug, Deserialize)]
struct TrackSearchResponse {
    results: TrackSearchResults,
}

#[derive(Debug, Deserialize)]
struct TrackSearchResults {
    trackmatches: TrackMatches,
}

// Last.fm nests as `{ "trackmatches": { "track": [...] } }`
#[derive(Debug, Deserialize)]
struct TrackMatches {
    track: Option<Vec<TrackMatch>>,
}

/// A single track result from `track.search`.
#[derive(Debug, Clone, Deserialize)]
pub struct TrackMatch {
    pub name: String,
    pub artist: String,
    pub url: String,
    #[serde(default)]
    pub listeners: String,
}

// -- artist.getInfo --

#[derive(Debug, Deserialize)]
struct ArtistInfoResponse {
    artist: ArtistInfo,
}

/// Artist metadata from `artist.getInfo`.
#[derive(Debug, Clone, Deserialize)]
pub struct ArtistInfo {
    pub name: String,
    pub url: String,
    pub stats: Option<ArtistStats>,
    pub bio: Option<ArtistBio>,
    pub tags: Option<ArtistTags>,
    pub similar: Option<SimilarArtists>,
}

/// One-line audience string from Last.fm `stats.listeners` (when Spotify shows 0 followers).
pub fn format_listener_line(info: &ArtistInfo) -> Option<String> {
    let n = info
        .stats
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .and_then(|l| l.replace(',', "").parse::<u64>().ok())?;
    Some(format!("{n} listeners (Last.fm)"))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistStats {
    pub listeners: Option<String>,
    pub playcount: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistBio {
    pub summary: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistTags {
    pub tag: Option<Vec<Tag>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tag {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimilarArtists {
    pub artist: Option<Vec<SimilarArtist>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimilarArtist {
    pub name: String,
    pub url: String,
}

// -- artist.getTopTracks --

#[derive(Debug, Deserialize)]
struct TopTracksResponse {
    toptracks: TopTracksInner,
}

#[derive(Debug, Deserialize)]
struct TopTracksInner {
    track: Option<Vec<TopTrack>>,
}

/// A top track from `artist.getTopTracks`.
#[derive(Debug, Clone, Deserialize)]
pub struct TopTrack {
    pub name: String,
    #[serde(default)]
    pub playcount: String,
    #[serde(default)]
    pub listeners: String,
    pub artist: TopTrackArtist,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TopTrackArtist {
    pub name: String,
}

// -- artist.getTopAlbums --

#[derive(Debug, Clone, Deserialize)]
pub struct TopAlbum {
    pub name: String,
    #[serde(default)]
    pub playcount: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub image: Option<Vec<LfImage>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LfImage {
    #[serde(rename = "#text")]
    pub url: String,
    #[serde(default)]
    pub size: String,
}
