//! On-disk cache for artist page payloads (profile, popular tracks, albums).

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::spotify_api::{ArtistAlbumSummary, ArtistProfile, PlaylistTrack};

/// How long cached artist pages are considered fresh without background refresh.
pub const ARTIST_CACHE_TTL: Duration = Duration::hours(12);

#[derive(Clone, Serialize, Deserialize)]
pub struct CachedArtistPage {
    pub profile: ArtistProfile,
    pub popular_tracks: Vec<PlaylistTrack>,
    pub albums: Vec<ArtistAlbumSummary>,
    pub fetched_at: String,
    #[serde(default)]
    pub listener_display: Option<String>,
}

impl CachedArtistPage {
    pub fn with_timestamp(
        profile: ArtistProfile,
        popular_tracks: Vec<PlaylistTrack>,
        albums: Vec<ArtistAlbumSummary>,
        listener_display: Option<String>,
    ) -> Self {
        Self {
            profile,
            popular_tracks,
            albums,
            fetched_at: Utc::now().to_rfc3339(),
            listener_display,
        }
    }

    pub fn fetched_at_parsed(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.fetched_at)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    }
}

pub fn is_fresh(page: &CachedArtistPage) -> bool {
    page.fetched_at_parsed()
        .is_some_and(|t| Utc::now() - t < ARTIST_CACHE_TTL)
}

pub struct ArtistCache {
    conn: Connection,
}

impl ArtistCache {
    pub fn new() -> Result<Self> {
        let db_path = cache_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create artist cache directory")?;
        }
        let conn = Connection::open(&db_path).context("Failed to open artist cache database")?;
        let mut cache = Self { conn };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&mut self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS artist_page (
                artist_id TEXT PRIMARY KEY,
                payload TEXT NOT NULL,
                fetched_at TEXT NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn load(&self, artist_id: &str) -> Result<Option<CachedArtistPage>> {
        let json: std::result::Result<String, rusqlite::Error> = self.conn.query_row(
            "SELECT payload FROM artist_page WHERE artist_id = ?1",
            [artist_id],
            |row| row.get(0),
        );
        let json = match json {
            Ok(s) => s,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let page: CachedArtistPage =
            serde_json::from_str(&json).context("deserialize artist cache")?;
        Ok(Some(page))
    }

    pub fn save(&mut self, artist_id: &str, page: &CachedArtistPage) -> Result<()> {
        let json = serde_json::to_string(page).context("serialize artist cache")?;
        self.conn
            .execute(
                "INSERT INTO artist_page (artist_id, payload, fetched_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(artist_id) DO UPDATE SET
                    payload = excluded.payload,
                    fetched_at = excluded.fetched_at",
                params![artist_id, json, page.fetched_at.as_str()],
            )
            .context("artist_page upsert")?;
        Ok(())
    }
}

fn cache_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("onyx")
        .join("artist_cache.sqlite")
}
