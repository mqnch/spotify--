use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};
use std::path::PathBuf;

use crate::spotify_api::{PlaylistSummary, PlaylistTrack};

pub struct PlaylistCache {
    conn: Connection,
}

pub struct CachedPlaylistTracks {
    pub tracks: Vec<PlaylistTrack>,
    pub complete: bool,
    pub snapshot_id: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
}

impl PlaylistCache {
    pub fn new() -> Result<Self> {
        let db_path = get_cache_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create playlist cache directory")?;
        }

        let conn = Connection::open(&db_path).context("Failed to open playlist cache")?;
        let mut cache = Self { conn };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&mut self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS playlist_cache (
                playlist_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                owner_name TEXT,
                track_count INTEGER NOT NULL,
                image_url TEXT,
                thumbnail_url TEXT,
                public_label TEXT NOT NULL,
                snapshot_id TEXT,
                fetched_at TEXT NOT NULL,
                complete INTEGER NOT NULL
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS playlist_tracks (
                playlist_id TEXT NOT NULL,
                position INTEGER NOT NULL,
                name TEXT NOT NULL,
                artist TEXT NOT NULL,
                album TEXT NOT NULL,
                album_image_url TEXT,
                album_thumbnail_url TEXT,
                added_at TEXT,
                duration_ms INTEGER NOT NULL,
                spotify_uri TEXT NOT NULL,
                PRIMARY KEY (playlist_id, position)
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist
             ON playlist_tracks(playlist_id, position)",
            [],
        )?;
        self.conn
            .execute(
                "ALTER TABLE playlist_cache ADD COLUMN thumbnail_url TEXT",
                [],
            )
            .ok();
        self.conn
            .execute("ALTER TABLE playlist_cache ADD COLUMN snapshot_id TEXT", [])
            .ok();
        self.conn
            .execute(
                "ALTER TABLE playlist_tracks ADD COLUMN album_thumbnail_url TEXT",
                [],
            )
            .ok();

        Ok(())
    }

    pub fn load_tracks(&self, playlist_id: &str) -> Result<Option<CachedPlaylistTracks>> {
        let metadata = match self.conn.query_row(
            "SELECT complete, snapshot_id, fetched_at FROM playlist_cache WHERE playlist_id = ?1",
            [playlist_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? != 0,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        ) {
            Ok(value) => value,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let (complete, snapshot_id, fetched_at) = metadata;
        let fetched_at = fetched_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));

        let mut stmt = self.conn.prepare(
            "SELECT position, name, artist, album, album_image_url, album_thumbnail_url, added_at, duration_ms, spotify_uri
             FROM playlist_tracks
             WHERE playlist_id = ?1
             ORDER BY position ASC",
        )?;

        let rows = stmt.query_map([playlist_id], |row| {
            Ok(PlaylistTrack {
                position: row.get::<_, i64>(0)? as u32,
                name: row.get(1)?,
                artist: row.get(2)?,
                album: row.get(3)?,
                album_image_url: row.get(4)?,
                album_thumbnail_url: row.get(5)?,
                added_at: row.get(6)?,
                duration_ms: row.get::<_, i64>(7)? as u32,
                spotify_uri: row.get(8)?,
            })
        })?;

        let mut tracks = Vec::new();
        for row in rows {
            tracks.push(row?);
        }

        Ok(Some(CachedPlaylistTracks {
            tracks,
            complete,
            snapshot_id,
            fetched_at,
        }))
    }

    pub fn load_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT playlist_id, name, track_count, image_url, thumbnail_url, owner_name, public_label, snapshot_id
             FROM playlist_cache
             ORDER BY fetched_at DESC, name ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(PlaylistSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                track_count: row.get::<_, i64>(2)? as u32,
                image_url: row.get(3)?,
                thumbnail_url: row.get(4)?,
                owner_name: row.get(5)?,
                public_label: row.get(6)?,
                snapshot_id: row.get(7)?,
            })
        })?;

        let mut playlists = Vec::new();
        for row in rows {
            playlists.push(row?);
        }

        Ok(playlists)
    }

    pub fn save_playlist(&self, playlist: &PlaylistSummary, complete: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO playlist_cache
                (playlist_id, name, owner_name, track_count, image_url, thumbnail_url, public_label, snapshot_id, fetched_at, complete)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(playlist_id) DO UPDATE SET
                name = excluded.name,
                owner_name = excluded.owner_name,
                track_count = excluded.track_count,
                image_url = excluded.image_url,
                thumbnail_url = excluded.thumbnail_url,
                public_label = excluded.public_label,
                snapshot_id = excluded.snapshot_id,
                fetched_at = excluded.fetched_at,
                complete = CASE
                    WHEN excluded.complete = 1 THEN 1
                    ELSE playlist_cache.complete
                END",
            params![
                playlist.id,
                playlist.name,
                playlist.owner_name,
                playlist.track_count,
                playlist.image_url,
                playlist.thumbnail_url,
                playlist.public_label,
                playlist.snapshot_id,
                Utc::now().to_rfc3339(),
                if complete { 1 } else { 0 },
            ],
        )?;

        Ok(())
    }

    pub fn cache_is_fresh(fetched_at: Option<DateTime<Utc>>) -> bool {
        fetched_at.is_some_and(|fetched_at| Utc::now() - fetched_at < Duration::minutes(30))
    }

    pub fn save_track_batch(&mut self, playlist_id: &str, tracks: &[PlaylistTrack]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO playlist_tracks
                    (playlist_id, position, name, artist, album, album_image_url, album_thumbnail_url, added_at, duration_ms, spotify_uri)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(playlist_id, position) DO UPDATE SET
                    name = excluded.name,
                    artist = excluded.artist,
                    album = excluded.album,
                    album_image_url = excluded.album_image_url,
                    album_thumbnail_url = excluded.album_thumbnail_url,
                    added_at = excluded.added_at,
                    duration_ms = excluded.duration_ms,
                    spotify_uri = excluded.spotify_uri",
            )?;

            for track in tracks {
                stmt.execute(params![
                    playlist_id,
                    track.position,
                    track.name,
                    track.artist,
                    track.album,
                    track.album_image_url,
                    track.album_thumbnail_url,
                    track.added_at,
                    track.duration_ms,
                    track.spotify_uri,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn finish_refresh(
        &self,
        playlist: &PlaylistSummary,
        loaded_track_count: usize,
    ) -> Result<()> {
        self.save_playlist(playlist, true)?;
        self.conn.execute(
            "DELETE FROM playlist_tracks
             WHERE playlist_id = ?1 AND position >= ?2",
            params![playlist.id, loaded_track_count as u32],
        )?;
        Ok(())
    }
}

fn get_cache_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("onyx")
        .join("playlist_cache.sqlite")
}
