/// Hybrid metadata pipeline — maps Last.fm results to Spotify URIs.
///
/// The `IdMatcher` takes `(artist, track)` string pairs from Last.fm and
/// resolves them to playable Spotify URIs using hyper-targeted `rspotify`
/// search queries (`track:"X" artist:"Y"`).

use anyhow::Result;
use rspotify::{
    model::{SearchType, SearchResult},
    prelude::*,
    AuthCodeSpotify,
};
use tokio::task::JoinSet;

// ───────────────────────────────────────────────────────────────────
// Resolved result
// ───────────────────────────────────────────────────────────────────

/// A Last.fm track that has been resolved (or failed to resolve)
/// to a Spotify URI.
#[derive(Debug, Clone)]
pub struct ResolvedTrack {
    pub artist: String,
    pub track: String,
    /// `None` if no Spotify match was found.
    pub spotify_uri: Option<String>,
}

// ───────────────────────────────────────────────────────────────────
// IdMatcher
// ───────────────────────────────────────────────────────────────────

/// Concurrently maps Last.fm `(artist, track)` pairs → Spotify URIs.
pub struct IdMatcher {
    spotify: AuthCodeSpotify,
}

/// Maximum number of concurrent Spotify search requests to avoid
/// rate-limiting (Spotify allows ~30 req/s per user token).
const MAX_CONCURRENT: usize = 5;

impl IdMatcher {
    pub fn new(spotify: AuthCodeSpotify) -> Self {
        Self { spotify }
    }

    /// Resolve a single `(artist, track)` pair to a Spotify URI.
    ///
    /// Returns `Ok(Some(uri))` on match, `Ok(None)` if no match found,
    /// or `Err` on API failure.
    pub async fn resolve_uri(
        &self,
        artist: &str,
        track: &str,
    ) -> Result<Option<String>> {
        // Build a targeted search query: track:"Never Gonna Give You Up" artist:"Rick Astley"
        let query = format!("track:\"{}\" artist:\"{}\"", track, artist);

        let result = self
            .spotify
            .search(&query, SearchType::Track, None, None, Some(1), None)
            .await;

        match result {
            Ok(SearchResult::Tracks(page)) => {
                if let Some(first) = page.items.first() {
                    // rspotify TrackId → full URI string
                    Ok(Some(first.id.as_ref().map_or_else(
                        || String::new(),
                        |id| id.uri(),
                    )))
                } else {
                    Ok(None)
                }
            }
            Ok(_) => Ok(None), // shouldn't happen for Track search
            Err(e) => {
                log::warn!("Spotify search failed for '{}': {}", query, e);
                Ok(None) // treat search errors as "not found" to avoid blocking the batch
            }
        }
    }

    /// Resolve a batch of `(artist, track)` pairs concurrently.
    ///
    /// Uses bounded concurrency ([`MAX_CONCURRENT`]) to stay within
    /// Spotify's rate limits.
    pub async fn resolve_batch(
        &self,
        items: Vec<(String, String)>,
    ) -> Vec<ResolvedTrack> {
        let mut results: Vec<ResolvedTrack> = Vec::with_capacity(items.len());
        let mut pending = items.into_iter().peekable();

        while pending.peek().is_some() {
            let mut join_set = JoinSet::new();
            let chunk: Vec<_> = pending.by_ref().take(MAX_CONCURRENT).collect();

            for (artist, track) in chunk {
                let spotify = self.spotify.clone();
                let a = artist.clone();
                let t = track.clone();

                join_set.spawn(async move {
                    let matcher = IdMatcher::new(spotify);
                    let uri = matcher.resolve_uri(&a, &t).await.unwrap_or(None);
                    ResolvedTrack {
                        artist,
                        track,
                        spotify_uri: uri,
                    }
                });
            }

            while let Some(result) = join_set.join_next().await {
                if let Ok(resolved) = result {
                    results.push(resolved);
                }
            }
        }

        results
    }
}
