mod app_settings;
mod auth;
mod config;
mod downloads;
mod gui;
mod lastfm;
mod metadata;
mod player;
mod playlist_cache;
mod spotify_api;
mod telemetry;

use anyhow::Result;
use librespot::playback::player::PlayerEvent;
use rspotify::prelude::*;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn main() -> Result<()> {
    // Initialize logger (set RUST_LOG=info or debug for more output).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("onyx=info")).init();

    println!();
    println!("  ╔═══════════════════════════╗");
    println!("  ║       ♫  O N Y X  ♫       ║");
    println!("  ╠═══════════════════════════╣");
    println!("  ║ Minimalist Spotify Client ║");
    println!("  ╚═══════════════════════════╝");

    // Initialize tokio runtime
    let rt = tokio::runtime::Runtime::new()?;
    let cache_only = spotify_api::cache_only_mode();
    if cache_only {
        println!("  ONYX_CACHE_ONLY=1: Spotify Web API and audio session startup are disabled.");
    }

    let (spotify, audio, db, playback_state, app_config, user_settings) = rt.block_on(async {
        // ── Step 1: Ensure API keys are configured ───────────────────────
        let app_config = config::AppConfig::ensure_configured()?;
        let user_settings = app_settings::UserSettings::load();

        // ── Step 2: Authenticate with Spotify Web API (rspotify) ─────────
        let spotify = auth::authenticate(&app_config).await?;

        // Verify: print the authenticated user.
        if cache_only {
            println!("  Logged in as: cached user (offline UI mode)");
        } else {
            let user = spotify.current_user().await?;
            println!(
                "  Logged in as: {}",
                user.display_name.as_deref().unwrap_or("(unknown)")
            );
        }

        // ── Step 3: Start the audio engine (librespot) ───────────────────
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PlayerEvent>();
        let audio = if cache_only {
            player::AudioEngine::offline()
        } else {
            player::AudioEngine::start(event_tx, user_settings.equalizer.clone()).await?
        };

        // ── Step 4: Event listener (Telemetry Engine) ────────────────────
        let db = Arc::new(Mutex::new(telemetry::TelemetryDb::new()?));
        let playback_state = Arc::new(Mutex::new(gui::PlaybackState::default()));

        let db_clone = Arc::clone(&db);
        let state_clone = Arc::clone(&playback_state);
        let spotify_clone = spotify.clone();

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match &event {
                    PlayerEvent::Playing {
                        track_id,
                        position_ms,
                        ..
                    } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = true;
                            st.position_ms = *position_ms;
                            st.position_anchor_ms = *position_ms;
                            st.position_updated_at = Some(std::time::Instant::now());
                        }
                        log::info!("▶ Playing {:?} at {}ms", track_id, position_ms);
                    }
                    PlayerEvent::Paused {
                        track_id,
                        position_ms,
                        ..
                    } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = false;
                            st.position_ms = *position_ms;
                            st.position_anchor_ms = *position_ms;
                            st.position_updated_at = None;
                        }
                        log::info!("⏸ Paused {:?} at {}ms", track_id, position_ms);
                    }
                    PlayerEvent::EndOfTrack { track_id, .. } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = false;
                            st.position_updated_at = None;
                            st.end_count = st.end_count.saturating_add(1);
                        }
                        log::info!("⏹ End of track {:?}", track_id);

                        if cache_only {
                            continue;
                        }

                        // Phase 4: Scrobble logic
                        let id_str = track_id.to_base62().unwrap_or_default();
                        if let Ok(rspotify_id) = rspotify::model::TrackId::from_id(id_str.as_str())
                        {
                            match spotify_clone.track(rspotify_id, None).await {
                                Ok(track_obj) => {
                                    let artist_name = track_obj
                                        .artists
                                        .first()
                                        .map(|a| a.name.clone())
                                        .unwrap_or_default();
                                    let scrobble = telemetry::Scrobble {
                                        track_name: track_obj.name.clone(),
                                        artist_name: artist_name.clone(),
                                        album_name: track_obj.album.name.clone(),
                                        duration_ms: track_obj.duration.num_milliseconds() as u32,
                                        spotify_uri: track_obj
                                            .id
                                            .map(|id| id.uri())
                                            .unwrap_or_default(),
                                    };
                                    if let Ok(db_lock) = db_clone.lock() {
                                        if let Err(e) = db_lock.record_scrobble(&scrobble) {
                                            log::error!("Failed to record scrobble: {}", e);
                                        } else {
                                            log::info!(
                                                "📝 Scrobbled: {} - {}",
                                                scrobble.artist_name,
                                                scrobble.track_name
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!(
                                        "Failed to fetch track metadata for scrobble: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                    PlayerEvent::VolumeChanged { volume } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.volume = *volume;
                        }
                        log::info!("🔊 Volume → {}", volume);
                    }
                    PlayerEvent::TrackChanged { audio_item } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.track_name = audio_item.name.clone();
                            st.position_ms = 0;
                            st.position_anchor_ms = 0;
                            st.position_updated_at = Some(std::time::Instant::now());
                        }
                        log::info!(
                            "🎵 Track changed → {} - {}",
                            audio_item.track_id,
                            audio_item.name
                        );
                    }
                    PlayerEvent::Stopped { .. } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = false;
                            st.track_name.clear();
                            st.artist_name.clear();
                            st.artwork_url = None;
                            st.spotify_uri = None;
                            st.position_ms = 0;
                            st.position_anchor_ms = 0;
                            st.position_updated_at = None;
                            st.duration_ms = 0;
                        }
                        log::info!("⏹ Stopped");
                    }
                    _ => {
                        log::debug!("Player event: {:?}", event);
                    }
                }
            }
        });

        // // ── Step 5: Demo — Hybrid Metadata Pipeline ──────────────────────
        // demo_metadata_pipeline(&app_config, &spotify, &audio).await?;

        Ok::<
            (
                rspotify::AuthCodeSpotify,
                player::AudioHandle,
                Arc<Mutex<telemetry::TelemetryDb>>,
                Arc<Mutex<gui::PlaybackState>>,
                config::AppConfig,
                app_settings::UserSettings,
            ),
            anyhow::Error,
        >((
            spotify,
            audio,
            db,
            playback_state,
            app_config,
            user_settings,
        ))
    })?;

    let audio_cmd_tx = audio.cmd_tx.clone();

    // ── Step 6: GUI Construction (egui) ─────────────────────────────
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title_shown(false)
            .with_titlebar_shown(false)
            .with_titlebar_buttons_shown(false)
            .with_fullsize_content_view(true),
        ..Default::default()
    };
    let rt_handle = rt.handle().clone();
    eframe::run_native(
        "Onyx",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(gui::OnyxApp::new(
                cc,
                rt_handle,
                spotify.clone(),
                audio_cmd_tx,
                playback_state.clone(),
                db.clone(),
                app_config.clone(),
                user_settings.clone(),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))?;

    audio.cmd_tx.send(player::AudioCmd::Shutdown)?;
    println!("\n  Goodbye.");

    Ok(())
}

/// Demonstrates the Phase 3 hybrid metadata pipeline:
/// 1. Fetch user playlists from Spotify
/// 2. Query Last.fm for an artist's top tracks
/// 3. Resolve those tracks to Spotify URIs via IdMatcher
/// 4. Play the first resolved track
async fn demo_metadata_pipeline(
    config: &config::AppConfig,
    spotify: &rspotify::AuthCodeSpotify,
    audio: &player::AudioHandle,
) -> Result<()> {
    println!();
    println!("  ─── Metadata Pipeline Demo ───");

    // 1. User playlists
    println!();
    match spotify_api::user_playlists(spotify).await {
        Ok(playlists) => {
            println!("  📂 Your playlists ({} total):", playlists.len());
            for p in playlists.iter().take(5) {
                println!("     • {} ({} tracks)", p.name, p.track_count);
            }
            if playlists.len() > 5 {
                println!("     … and {} more", playlists.len() - 5);
            }
        }
        Err(e) => {
            log::warn!("Failed to fetch playlists: {}", e);
        }
    }

    // 1.5 User saved tracks (Liked Songs)
    println!();
    match spotify_api::user_saved_tracks(spotify).await {
        Ok(tracks) => {
            println!("  ❤ Your Liked Songs ({} total):", tracks.len());
            for t in tracks.iter().take(5) {
                println!("     • {} - {}", t.artist, t.name);
            }
            if tracks.len() > 5 {
                println!("     … and {} more", tracks.len() - 5);
            }
        }
        Err(e) => {
            log::warn!("Failed to fetch saved tracks: {}", e);
        }
    }

    // 2. Last.fm — artist top tracks
    let lastfm = lastfm::LastFmClient::new(&config.lastfm_api_key);
    let artist = "Radiohead";
    println!();
    println!("  🎸 Last.fm top tracks for \"{}\":", artist);

    let top_tracks = lastfm.artist_top_tracks(artist, 10).await?;
    for (i, t) in top_tracks.iter().enumerate() {
        println!("     {}. {} (♫ {} plays)", i + 1, t.name, t.playcount);
    }

    // 3. IdMatcher — resolve to Spotify URIs
    let matcher = metadata::IdMatcher::new(spotify.clone());

    let items: Vec<(String, String)> = top_tracks
        .iter()
        .map(|t| (t.artist.name.clone(), t.name.clone()))
        .collect();

    println!();
    println!("  🔗 Resolving to Spotify URIs…");
    let resolved = matcher.resolve_batch(items).await;

    let mut first_uri: Option<String> = None;
    for r in &resolved {
        match &r.spotify_uri {
            Some(uri) => {
                println!("     ✓ {} → {}", r.track, uri);
                if first_uri.is_none() {
                    first_uri = Some(uri.clone());
                }
            }
            None => {
                println!("     ✗ {} → (no match)", r.track);
            }
        }
    }

    // 4. Play the first resolved track
    if let Some(uri) = first_uri {
        println!();
        println!("  ▶ Playing first resolved track…");
        println!("  (Press Ctrl+C to stop)");
        audio.cmd_tx.send(player::AudioCmd::Load {
            uri,
            start_playing: true,
            position_ms: 0,
        })?;
    } else {
        println!("  ⚠ No tracks resolved — nothing to play.");
    }

    Ok(())
}
