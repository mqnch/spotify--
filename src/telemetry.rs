/// Local Telemetry Engine.
///
/// Stores listening history locally in an embedded SQLite database.
/// Provides methods to record a play (scrobble) and retrieve statistics
/// (top tracks, top artists).
use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Utc};
use rusqlite::{Connection, params};
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
pub struct RankedItem {
    pub name: String,
    pub plays: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsMetric {
    Plays,
    Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsDateRange {
    AllTime,
    Year(i32),
    Month { year: i32, month: u32 },
}

#[derive(Debug, Clone, Default)]
pub struct ListeningStats {
    pub total_plays: u32,
    pub total_listening_time_ms: u64,
    pub top_artists: Vec<RankedItem>,
    pub top_tracks: Vec<RankedItem>,
    pub top_albums: Vec<RankedItem>,
    pub available_years: Vec<i32>,
    pub available_months: Vec<u32>,
}

impl StatsDateRange {
    fn bounds(self) -> Option<(String, Option<String>)> {
        match self {
            Self::AllTime => None,
            Self::Year(year) => Some((
                format!("{year:04}-01-01"),
                Some(format!("{:04}-01-01", year + 1)),
            )),
            Self::Month { year, month } => {
                let start_date = NaiveDate::from_ymd_opt(year, month, 1)?;
                let end_date = if month == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1)?
                } else {
                    NaiveDate::from_ymd_opt(year, month + 1, 1)?
                };
                Some((
                    start_date.format("%Y-%m-%d").to_string(),
                    Some(end_date.format("%Y-%m-%d").to_string()),
                ))
            }
        }
    }
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

        // Execute manually to avoid requiring &mut Connection for a Transaction
        self.conn.execute("BEGIN TRANSACTION", [])?;

        let result = self.import_spotify_history_archive(&mut archive);
        match result {
            Ok(count) => {
                self.conn.execute("COMMIT", [])?;
                Ok(count)
            }
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(err)
            }
        }
    }

    fn import_spotify_history_archive<R: std::io::Read + std::io::Seek>(
        &self,
        archive: &mut ZipArchive<R>,
    ) -> Result<usize> {
        let mut count = 0;
        let mut matched_history_files = 0;

        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .with_context(|| format!("Failed to read zip entry {}", i))?;
            if !file.is_file() {
                continue;
            }

            let name = file.name().to_string();
            if name.ends_with(".json")
                && (name.contains("StreamingHistory") || name.contains("Streaming_History"))
            {
                matched_history_files += 1;

                let mut content = String::new();
                file.read_to_string(&mut content)
                    .with_context(|| format!("Failed to read Spotify history file {}", name))?;

                let entries: Vec<SpotifyHistoryEntry> = serde_json::from_str(&content)
                    .with_context(|| format!("Failed to parse Spotify history file {}", name))?;

                for entry in entries {
                    let (track_name, artist_name, album_name, duration_ms, spotify_uri, ts) =
                        match entry {
                            SpotifyHistoryEntry::Standard {
                                end_time,
                                artist_name,
                                track_name,
                                ms_played,
                            } => (
                                track_name,
                                artist_name,
                                String::new(),
                                ms_played,
                                String::new(),
                                end_time,
                            ),
                            SpotifyHistoryEntry::Extended {
                                ts,
                                master_metadata_album_artist_name,
                                master_metadata_track_name,
                                master_metadata_album_album_name,
                                spotify_track_uri,
                                ms_played,
                            } => {
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

                    count += self.conn.execute(
                        "INSERT INTO listening_history (timestamp, track_name, artist_name, album_name, duration_ms, spotify_uri)
                         SELECT ?1, ?2, ?3, ?4, ?5, ?6
                         WHERE NOT EXISTS (
                            SELECT 1 FROM listening_history
                            WHERE timestamp = ?1
                              AND track_name = ?2
                              AND artist_name = ?3
                              AND spotify_uri = ?6
                         )",
                        params![ts, track_name, artist_name, album_name, duration_ms, spotify_uri],
                    )?;
                }
            }
        }

        if matched_history_files == 0 {
            anyhow::bail!("No Spotify streaming history JSON files found in zip");
        }
        Ok(count)
    }

    // ───────────────────────────────────────────────────────────────────
    // Aggregation Queries
    // ───────────────────────────────────────────────────────────────────

    /// Get total play count.
    pub fn total_plays(&self) -> Result<u32> {
        self.total_plays_for_range(StatsDateRange::AllTime)
    }

    /// Get total listening time in milliseconds.
    pub fn total_listening_time_ms(&self) -> Result<u64> {
        self.total_listening_time_ms_for_range(StatsDateRange::AllTime)
    }

    /// Get top artists by play count.
    pub fn top_artists(&self, limit: u32) -> Result<Vec<RankedItem>> {
        self.top_artists_for_range(StatsDateRange::AllTime, limit, StatsMetric::Plays)
    }

    /// Get top tracks by play count.
    pub fn top_tracks(&self, limit: u32) -> Result<Vec<RankedItem>> {
        self.top_tracks_for_range(StatsDateRange::AllTime, limit, StatsMetric::Plays)
    }

    /// Get top albums by play count.
    pub fn top_albums(&self, limit: u32) -> Result<Vec<RankedItem>> {
        self.top_albums_for_range(StatsDateRange::AllTime, limit, StatsMetric::Plays)
    }

    pub fn available_years(&self) -> Result<Vec<i32>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT CAST(substr(timestamp, 1, 4) AS INTEGER) as year
             FROM listening_history
             WHERE length(timestamp) >= 4
             ORDER BY year DESC",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut years = Vec::new();
        for year in rows {
            years.push(year?);
        }
        Ok(years)
    }

    pub fn available_months(&self, year: i32) -> Result<Vec<u32>> {
        let prefix = format!("{year:04}-");
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT CAST(substr(timestamp, 6, 2) AS INTEGER) as month
             FROM listening_history
             WHERE timestamp >= ?1 AND timestamp < ?2
             ORDER BY month ASC",
        )?;
        let rows = stmt.query_map(params![prefix, format!("{:04}-", year + 1)], |row| {
            row.get(0)
        })?;
        let mut months = Vec::new();
        for month in rows {
            months.push(month?);
        }
        Ok(months)
    }

    /// Get all dashboard listening stats in one pass from the GUI perspective.
    pub fn listening_stats(&self, limit: u32) -> Result<ListeningStats> {
        self.listening_stats_for_range(
            StatsDateRange::AllTime,
            limit,
            StatsMetric::Plays,
            StatsMetric::Plays,
        )
    }

    pub fn listening_stats_for_range(
        &self,
        range: StatsDateRange,
        limit: u32,
        track_metric: StatsMetric,
        artist_metric: StatsMetric,
    ) -> Result<ListeningStats> {
        let selected_year = match range {
            StatsDateRange::Year(year) | StatsDateRange::Month { year, .. } => year,
            StatsDateRange::AllTime => Utc::now().year(),
        };

        Ok(ListeningStats {
            total_plays: self.total_plays_for_range(range)?,
            total_listening_time_ms: self.total_listening_time_ms_for_range(range)?,
            top_artists: self.top_artists_for_range(range, limit, artist_metric)?,
            top_tracks: self.top_tracks_for_range(range, limit, track_metric)?,
            top_albums: self.top_albums_for_range(range, limit, StatsMetric::Plays)?,
            available_years: self.available_years()?,
            available_months: self.available_months(selected_year)?,
        })
    }

    fn total_plays_for_range(&self, range: StatsDateRange) -> Result<u32> {
        let count: u32 = match range.bounds() {
            Some((start, Some(end))) => self.conn.query_row(
                "SELECT COUNT(*) FROM listening_history WHERE timestamp >= ?1 AND timestamp < ?2",
                params![start, end],
                |row| row.get(0),
            )?,
            Some((start, None)) => self.conn.query_row(
                "SELECT COUNT(*) FROM listening_history WHERE timestamp >= ?1",
                params![start],
                |row| row.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT COUNT(*) FROM listening_history", [], |row| {
                    row.get(0)
                })?,
        };
        Ok(count)
    }

    fn total_listening_time_ms_for_range(&self, range: StatsDateRange) -> Result<u64> {
        let ms: Option<i64> = match range.bounds() {
            Some((start, Some(end))) => self.conn.query_row(
                "SELECT SUM(duration_ms) FROM listening_history WHERE timestamp >= ?1 AND timestamp < ?2",
                params![start, end],
                |row| row.get(0),
            )?,
            Some((start, None)) => self.conn.query_row(
                "SELECT SUM(duration_ms) FROM listening_history WHERE timestamp >= ?1",
                params![start],
                |row| row.get(0),
            )?,
            None => self.conn.query_row(
                "SELECT SUM(duration_ms) FROM listening_history",
                [],
                |row| row.get(0),
            )?,
        };
        Ok(ms.unwrap_or(0) as u64)
    }

    fn top_artists_for_range(
        &self,
        range: StatsDateRange,
        limit: u32,
        metric: StatsMetric,
    ) -> Result<Vec<RankedItem>> {
        self.rank_by_field("artist_name", "artist_name != ''", range, limit, metric)
    }

    fn top_tracks_for_range(
        &self,
        range: StatsDateRange,
        limit: u32,
        metric: StatsMetric,
    ) -> Result<Vec<RankedItem>> {
        self.rank_by_field("track_name", "track_name != ''", range, limit, metric)
    }

    fn top_albums_for_range(
        &self,
        range: StatsDateRange,
        limit: u32,
        metric: StatsMetric,
    ) -> Result<Vec<RankedItem>> {
        self.rank_by_field("album_name", "album_name != ''", range, limit, metric)
    }

    fn rank_by_field(
        &self,
        field: &str,
        base_where: &str,
        range: StatsDateRange,
        limit: u32,
        metric: StatsMetric,
    ) -> Result<Vec<RankedItem>> {
        let order_field = match metric {
            StatsMetric::Plays => "plays",
            StatsMetric::Time => "duration_ms",
        };
        let query = match range.bounds() {
            Some((_, Some(_))) => format!(
                "SELECT {field}, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as duration_ms
                 FROM listening_history
                 WHERE {base_where} AND timestamp >= ?1 AND timestamp < ?2
                 GROUP BY {field}
                 ORDER BY {order_field} DESC, plays DESC, {field} ASC
                 LIMIT ?3"
            ),
            Some((_, None)) => format!(
                "SELECT {field}, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as duration_ms
                 FROM listening_history
                 WHERE {base_where} AND timestamp >= ?1
                 GROUP BY {field}
                 ORDER BY {order_field} DESC, plays DESC, {field} ASC
                 LIMIT ?2"
            ),
            None => format!(
                "SELECT {field}, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as duration_ms
                 FROM listening_history
                 WHERE {base_where}
                 GROUP BY {field}
                 ORDER BY {order_field} DESC, plays DESC, {field} ASC
                 LIMIT ?1"
            ),
        };

        let map_row = |row: &rusqlite::Row<'_>| {
            let duration_ms: i64 = row.get(2)?;
            Ok(RankedItem {
                name: row.get(0)?,
                plays: row.get(1)?,
                duration_ms: duration_ms.max(0) as u64,
            })
        };

        let mut stmt = self.conn.prepare(&query)?;
        let rows = match range.bounds() {
            Some((start, Some(end))) => stmt.query_map(params![start, end, limit], map_row)?,
            Some((start, None)) => stmt.query_map(params![start, limit], map_row)?,
            None => stmt.query_map(params![limit], map_row)?,
        };

        let mut items = Vec::new();
        for item in rows {
            items.push(item?);
        }
        Ok(items)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn in_memory_db() -> TelemetryDb {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let mut db = TelemetryDb { conn };
        db.init_schema().expect("initialize schema");
        db
    }

    fn insert_history(
        db: &TelemetryDb,
        timestamp: &str,
        track: &str,
        artist: &str,
        album: &str,
        duration_ms: u32,
    ) {
        db.conn
            .execute(
                "INSERT INTO listening_history (timestamp, track_name, artist_name, album_name, duration_ms, spotify_uri)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![timestamp, track, artist, album, duration_ms, ""],
            )
            .expect("insert history row");
    }

    #[test]
    fn listening_stats_include_totals_and_top_items() {
        let db = in_memory_db();
        db.record_scrobble(&Scrobble {
            track_name: "Nude".to_string(),
            artist_name: "Radiohead".to_string(),
            album_name: "In Rainbows".to_string(),
            duration_ms: 240_000,
            spotify_uri: "spotify:track:1".to_string(),
        })
        .expect("record first scrobble");
        db.record_scrobble(&Scrobble {
            track_name: "Weird Fishes".to_string(),
            artist_name: "Radiohead".to_string(),
            album_name: "In Rainbows".to_string(),
            duration_ms: 300_000,
            spotify_uri: "spotify:track:2".to_string(),
        })
        .expect("record second scrobble");

        let stats = db.listening_stats(10).expect("load stats");

        assert_eq!(stats.total_plays, 2);
        assert_eq!(stats.total_listening_time_ms, 540_000);
        assert_eq!(stats.top_artists[0].name, "Radiohead");
        assert_eq!(stats.top_artists[0].plays, 2);
        assert_eq!(stats.top_albums[0].name, "In Rainbows");
    }

    #[test]
    fn listening_stats_filter_by_year_and_month() {
        let db = in_memory_db();
        insert_history(&db, "2025-12-31 23:00", "Old Song", "Past", "Old", 120_000);
        insert_history(
            &db,
            "2026-01-10 12:00",
            "January Song",
            "Now",
            "New",
            180_000,
        );
        insert_history(&db, "2026-04-10 12:00", "April Song", "Now", "New", 240_000);

        let year_stats = db
            .listening_stats_for_range(
                StatsDateRange::Year(2026),
                10,
                StatsMetric::Plays,
                StatsMetric::Plays,
            )
            .expect("year stats");
        let month_stats = db
            .listening_stats_for_range(
                StatsDateRange::Month {
                    year: 2026,
                    month: 4,
                },
                10,
                StatsMetric::Plays,
                StatsMetric::Plays,
            )
            .expect("month stats");

        assert_eq!(year_stats.total_plays, 2);
        assert_eq!(year_stats.total_listening_time_ms, 420_000);
        assert_eq!(month_stats.total_plays, 1);
        assert_eq!(month_stats.top_tracks[0].name, "April Song");
        assert_eq!(month_stats.available_years, vec![2026, 2025]);
        assert!(month_stats.available_months.contains(&4));
    }

    #[test]
    fn listening_stats_can_rank_by_time() {
        let db = in_memory_db();
        insert_history(
            &db,
            "2026-04-10 12:00",
            "Short Repeat",
            "Artist A",
            "A",
            60_000,
        );
        insert_history(
            &db,
            "2026-04-11 12:00",
            "Short Repeat",
            "Artist A",
            "A",
            60_000,
        );
        insert_history(
            &db,
            "2026-04-12 12:00",
            "Long Track",
            "Artist B",
            "B",
            300_000,
        );

        let by_plays = db
            .listening_stats_for_range(
                StatsDateRange::AllTime,
                10,
                StatsMetric::Plays,
                StatsMetric::Plays,
            )
            .expect("plays stats");
        let by_time = db
            .listening_stats_for_range(
                StatsDateRange::AllTime,
                10,
                StatsMetric::Time,
                StatsMetric::Time,
            )
            .expect("time stats");

        assert_eq!(by_plays.top_tracks[0].name, "Short Repeat");
        assert_eq!(by_time.top_tracks[0].name, "Long Track");
        assert_eq!(by_time.top_tracks[0].duration_ms, 300_000);
    }

    #[test]
    fn spotify_zip_import_does_not_duplicate_repeated_imports() {
        let db = in_memory_db();
        let path = std::env::temp_dir().join(format!(
            "onyx-history-test-{}.zip",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        {
            let file = File::create(&path).expect("create zip");
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "Spotify Account Data/StreamingHistory_music_0.json",
                SimpleFileOptions::default(),
            )
            .expect("start history file");
            zip.write_all(
                br#"[{
                    "endTime": "2026-04-30 12:00",
                    "artistName": "Radiohead",
                    "trackName": "Nude",
                    "msPlayed": 240000
                }]"#,
            )
            .expect("write history file");
            zip.finish().expect("finish zip");
        }

        let first_import = db
            .import_spotify_history_zip(&path.display().to_string())
            .expect("first import");
        let second_import = db
            .import_spotify_history_zip(&path.display().to_string())
            .expect("second import");
        let stats = db.listening_stats(10).expect("load stats");

        let _ = std::fs::remove_file(path);

        assert_eq!(first_import, 1);
        assert_eq!(second_import, 0);
        assert_eq!(stats.total_plays, 1);
        assert_eq!(stats.total_listening_time_ms, 240_000);
    }
}
