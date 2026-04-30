/// Local Telemetry Engine.
///
/// Stores listening history locally in an embedded SQLite database.
/// Provides methods to record a play (scrobble) and retrieve statistics
/// (top tracks, top artists).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use zip::ZipArchive;

// ───────────────────────────────────────────────────────────────────
// Data Models
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Scrobble {
    pub track_name: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration_ms: u32,
    pub spotify_uri: String,
}

#[derive(Debug, Clone)]
pub struct TopItem {
    pub name: String,
    pub count: u32,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum SpotifyHistoryEntry {
    Standard {
        #[serde(rename = "endTime")]
        end_time: String,
        #[serde(rename = "artistName")]
        artist_name: String,
        #[serde(rename = "trackName")]
        track_name: String,
        #[serde(rename = "msPlayed")]
        ms_played: u32,
    },
    Extended {
        ts: String,
        master_metadata_album_artist_name: Option<String>,
        master_metadata_track_name: Option<String>,
        master_metadata_album_album_name: Option<String>,
        spotify_track_uri: Option<String>,
        ms_played: u32,
    },
}

// ───────────────────────────────────────────────────────────────────
// Telemetry Database
// ───────────────────────────────────────────────────────────────────

pub struct TelemetryDb {
    conn: Connection,
}

impl TelemetryDb {
    /// Open the telemetry database. Creates the directory and file if needed.
    pub fn new() -> Result<Self> {
        let db_path = get_db_path();

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create config directory for telemetry")?;
        }

        let conn = Connection::open(&db_path).context("Failed to open SQLite database")?;

        let mut db = Self { conn };
        db.init_schema()?;

        Ok(db)
    }

    /// Creates the tables if they don't exist.
    fn init_schema(&mut self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS listening_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                track_name TEXT NOT NULL,
                artist_name TEXT NOT NULL,
                album_name TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                spotify_uri TEXT NOT NULL
            )",
            [],
        )?;

        // Index on artist name for faster top artist aggregation
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_artist_name ON listening_history(artist_name)",
            [],
        )?;

        // Index on track name for faster top track aggregation
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_track_name ON listening_history(track_name)",
            [],
        )?;

        Ok(())
    }

    /// Record a track play.
    pub fn record_scrobble(&self, scrobble: &Scrobble) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            "INSERT INTO listening_history (timestamp, track_name, artist_name, album_name, duration_ms, spotify_uri)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now,
                scrobble.track_name,
                scrobble.artist_name,
                scrobble.album_name,
                scrobble.duration_ms,
                scrobble.spotify_uri,
            ],
        )?;

        Ok(())
    }

    /// Import scrobbles from a Spotify data export ZIP file.
    pub fn import_spotify_history_zip(&self, path: &str) -> Result<usize> {
        let file = File::open(path).context("Failed to open zip file")?;
        let mut archive = ZipArchive::new(file).context("Failed to parse zip archive")?;

        let mut count = 0;
        
        // Execute manually to avoid requiring &mut Connection for a Transaction
        self.conn.execute("BEGIN TRANSACTION", [])?;

        for i in 0..archive.len() {
            let mut file = match archive.by_index(i) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if !file.is_file() {
                continue;
            }
            
            let name = file.name().to_string();
            if name.ends_with(".json") && (name.contains("StreamingHistory") || name.contains("Streaming_History")) {
                let mut content = String::new();
                if file.read_to_string(&mut content).is_err() {
                    continue;
                }

                let entries: Vec<SpotifyHistoryEntry> = match serde_json::from_str(&content) {
                    Ok(e) => e,
                    Err(_) => continue, // skip files that don't match our schema
                };

                for entry in entries {
                    let (track_name, artist_name, album_name, duration_ms, spotify_uri, ts) = match entry {
                        SpotifyHistoryEntry::Standard { end_time, artist_name, track_name, ms_played } => {
                            (track_name, artist_name, String::new(), ms_played, String::new(), end_time)
                        }
                        SpotifyHistoryEntry::Extended { ts, master_metadata_album_artist_name, master_metadata_track_name, master_metadata_album_album_name, spotify_track_uri, ms_played } => {
                            let track = master_metadata_track_name.unwrap_or_default();
                            let artist = master_metadata_album_artist_name.unwrap_or_default();
                            let album = master_metadata_album_album_name.unwrap_or_default();
                            let uri = spotify_track_uri.unwrap_or_default();
                            (track, artist, album, ms_played, uri, ts)
                        }
                    };

                    if duration_ms < 30_000 || track_name.is_empty() || artist_name.is_empty() {
                        continue; // Skip short plays and podcasts
                    }

                    if let Err(_) = self.conn.execute(
                        "INSERT INTO listening_history (timestamp, track_name, artist_name, album_name, duration_ms, spotify_uri)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![ts, track_name, artist_name, album_name, duration_ms, spotify_uri],
                    ) {
                        // ignore individual insert errors inside bulk operation
                    } else {
                        count += 1;
                    }
                }
            }
        }

        self.conn.execute("COMMIT", [])?;

        Ok(count)
    }

    // ───────────────────────────────────────────────────────────────────
    // Aggregation Queries
    // ───────────────────────────────────────────────────────────────────

    /// Get total play count.
    pub fn total_plays(&self) -> Result<u32> {
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM listening_history",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get total listening time in milliseconds.
    pub fn total_listening_time_ms(&self) -> Result<u64> {
        let ms: Option<i64> = self.conn.query_row(
            "SELECT SUM(duration_ms) FROM listening_history",
            [],
            |row| row.get(0),
        )?;
        Ok(ms.unwrap_or(0) as u64)
    }

    /// Get top artists by play count.
    pub fn top_artists(&self, limit: u32) -> Result<Vec<TopItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT artist_name, COUNT(*) as count 
             FROM listening_history 
             GROUP BY artist_name 
             ORDER BY count DESC 
             LIMIT ?1"
        )?;

        let iter = stmt.query_map([limit], |row| {
            Ok(TopItem {
                name: row.get(0)?,
                count: row.get(1)?,
            })
        })?;

        let mut artists = Vec::new();
        for item in iter {
            artists.push(item?);
        }

        Ok(artists)
    }

    /// Get top tracks by play count.
    pub fn top_tracks(&self, limit: u32) -> Result<Vec<TopItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_name, COUNT(*) as count 
             FROM listening_history 
             GROUP BY track_name 
             ORDER BY count DESC 
             LIMIT ?1"
        )?;

        let iter = stmt.query_map([limit], |row| {
            Ok(TopItem {
                name: row.get(0)?,
                count: row.get(1)?,
            })
        })?;

        let mut tracks = Vec::new();
        for item in iter {
            tracks.push(item?);
        }

        Ok(tracks)
    }
}

// ───────────────────────────────────────────────────────────────────
// Internal Helpers
// ───────────────────────────────────────────────────────────────────

/// Get the path to the sqlite database file.
fn get_db_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("onyx")
        .join("telemetry.sqlite")
}
