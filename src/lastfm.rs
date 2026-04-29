/// Last.fm REST API client.
///
/// Provides async methods for `track.search`, `artist.getInfo`, and
/// `artist.getTopTracks`.  Uses `reqwest` + `serde` for zero-cost JSON
/// deserialization.

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
