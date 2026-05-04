mod app_settings;
mod artist_cache;
mod auth;
mod config;
mod downloads;
mod gui;
mod lastfm;
mod metadata;
mod player;
mod playlist_cache;
mod spotify_api;
mod spotify_token_store;
mod telemetry;

use anyhow::{Context, Result};
use librespot::playback::player::PlayerEvent;
use rspotify::prelude::*;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

async fn spotify_access_token(spotify: &rspotify::AuthCodeSpotify) -> Option<String> {
    auth::access_token(spotify).await
}

fn main() -> Result<()> {
    // Default: onyx at info; librespot at warn so load/decode/sink errors are visible (they are easy to miss with `onyx=info` alone).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "onyx=info,librespot_core=warn,librespot_playback=warn,librespot_metadata=warn",
    ))
    .init();

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

    let (spotify_opt, audio, db, playback_state, app_config, user_settings) = rt.block_on(async {
        // ── Step 1: Ensure API keys are configured ───────────────────────
        let app_config = config::AppConfig::ensure_configured()?;
        let user_settings = app_settings::UserSettings::load();

        // ── Step 2: Restore Spotify Web API session from keyring (optional) ─
        let spotify_opt: Option<rspotify::AuthCodeSpotify> = if cache_only {
            match auth::restore_spotify_session(&app_config).await? {
                Some(s) => Some(s),
                None => {
                    return Err(anyhow::anyhow!(
                        "ONYX_CACHE_ONLY=1 requires a valid Spotify token in the keyring"
                    ));
                }
            }
        } else {
            auth::restore_spotify_session(&app_config).await?
        };

        if let Some(ref sp) = spotify_opt {
            if cache_only {
                println!("  Logged in as: cached user (offline UI mode)");
            } else {
                let user = sp.current_user().await?;
                println!(
                    "  Logged in as: {}",
                    user.display_name.as_deref().unwrap_or("(unknown)")
                );
            }
        } else if !cache_only {
            println!("  Spotify: not signed in — use Connect to Spotify in the app window.");
        }

        // ── Step 3: Audio engine (librespot) ─────────────────────────────
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PlayerEvent>();

        let warm = Arc::new(player::AudioWarmState {
            event_tx: event_tx.clone(),
            equalizer: Arc::new(Mutex::new(user_settings.equalizer.clone())),
            client_id: app_config.spotify_client_id.clone(),
        });

        let audio = if cache_only {
            player::AudioEngine::offline(Arc::clone(&warm))
        } else if let Some(ref sp) = spotify_opt {
            let token = spotify_access_token(sp).await;
            player::AudioEngine::start_live(Arc::clone(&warm), token)
                .await
                .context("Failed to start audio engine")?
        } else {
            player::AudioEngine::offline(Arc::clone(&warm))
        };

        // ── Step 4: Event listener (Telemetry Engine) ────────────────────
        let db = Arc::new(Mutex::new(telemetry::TelemetryDb::new()?));
        let playback_state = Arc::new(Mutex::new(gui::PlaybackState::default()));

        let db_clone = Arc::clone(&db);
        let state_clone = Arc::clone(&playback_state);
        let spotify_for_events = spotify_opt.clone();
        let audio_for_reconnect = audio.clone();
        let reconnect_busy = Arc::new(Mutex::new(false));

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

                        let Some(ref spotify_clone) = spotify_for_events else {
                            continue;
                        };

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
                            st.duration_ms = audio_item.duration_ms;
                        }
                        log::info!(
                            "🎵 Track changed → {} - {}",
                            audio_item.track_id,
                            audio_item.name
                        );

                        if cache_only {
                            continue;
                        }
                        let Some(ref spotify_meta) = spotify_for_events else {
                            continue;
                        };
                        let id_str = audio_item.track_id.to_base62().unwrap_or_default();
                        if id_str.is_empty() {
                            continue;
                        }
                        let spotify = spotify_meta.clone();
                        let state_meta = state_clone.clone();
                        tokio::spawn(async move {
                            let Ok(tid) = rspotify::model::TrackId::from_id(id_str.as_str()) else {
                                return;
                            };
                            match spotify.track(tid, None).await {
                                Ok(track_obj) => {
                                    let artist_name = track_obj
                                        .artists
                                        .first()
                                        .map(|a| a.name.clone())
                                        .unwrap_or_default();
                                    let artist_id = track_obj
                                        .artists
                                        .first()
                                        .and_then(|a| a.id.as_ref())
                                        .map(|id| id.to_string());
                                    let (artwork_url, _) =
                                        crate::spotify_api::full_track_artwork_urls(&track_obj);
                                    if let Ok(mut st) = state_meta.lock() {
                                        st.artist_name = artist_name;
                                        st.artist_id = artist_id;
                                        st.artwork_url = artwork_url;
                                        st.spotify_uri =
                                            track_obj.id.as_ref().map(|id| id.uri());
                                        st.duration_ms =
                                            track_obj.duration.num_milliseconds() as u32;
                                    }
                                }
                                Err(e) => {
                                    log::debug!(
                                        "Web API track metadata after TrackChanged failed: {}",
                                        e
                                    );
                                }
                            }
                        });
                    }
                    PlayerEvent::Stopped { .. } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = false;
                            st.track_name.clear();
                            st.artist_name.clear();
                            st.artist_id = None;
                            st.artwork_url = None;
                            st.spotify_uri = None;
                            st.position_ms = 0;
                            st.position_anchor_ms = 0;
                            st.position_updated_at = None;
                            st.duration_ms = 0;
                        }
                        log::info!("⏹ Stopped");
                    }
                    PlayerEvent::Loading {
                        track_id,
                        position_ms,
                        ..
                    } => {
                        log::info!(
                            "Loading track {:?} (start position {} ms)",
                            track_id,
                            position_ms
                        );
                    }
                    PlayerEvent::Unavailable { track_id, .. } => {
                        if let Ok(mut st) = state_clone.lock() {
                            st.is_playing = false;
                            st.position_updated_at = None;
                        }
                        // librespot loads tracks via Spotify's internal extended-metadata API; HTTP 400
                        // often came from using a custom Session `client_id` (must stay librespot default).
                        // Other causes: Premium, regional restrictions, or Developer app access.
                        log::warn!(
                            "Track unavailable: {:?}. If this persists, confirm Spotify Premium, try \
                             another track, and check RUST_LOG=librespot_core=debug for SpClient errors.",
                            track_id
                        );
                    }
                    PlayerEvent::SessionDisconnected { .. } if !cache_only => {
                        let Some(ref sp) = spotify_for_events else {
                            continue;
                        };
                        {
                            let mut g = reconnect_busy.lock().unwrap();
                            if *g {
                                continue;
                            }
                            *g = true;
                        }
                        log::warn!(
                            "librespot session disconnected; attempting one reconnect with refreshed token"
                        );
                        let sp = sp.clone();
                        let audio = audio_for_reconnect.clone();
                        let busy = Arc::clone(&reconnect_busy);
                        let state_for_vol = state_clone.clone();
                        tokio::spawn(async move {
                            struct BusyReset(Arc<Mutex<bool>>);
                            impl Drop for BusyReset {
                                fn drop(&mut self) {
                                    if let Ok(mut g) = self.0.lock() {
                                        *g = false;
                                    }
                                }
                            }
                            let _busy_guard = BusyReset(busy);

                            if let Err(e) = sp.refresh_token().await {
                                log::error!("Token refresh before audio reconnect failed: {}", e);
                                return;
                            }
                            let Some(tok) = spotify_access_token(&sp).await else {
                                log::error!("No access token after refresh for audio reconnect");
                                return;
                            };
                            match audio.reconnect_live_session(&tok).await {
                                Ok(()) => {
                                    let vol = state_for_vol
                                        .lock()
                                        .map(|g| g.volume)
                                        .unwrap_or(u16::MAX);
                                    if let Err(e) =
                                        audio.send(player::AudioCmd::SetVolume { volume_u16: vol })
                                    {
                                        log::warn!("Volume sync after reconnect failed: {}", e);
                                    }
                                }
                                Err(e) => log::error!("librespot reconnect failed: {}", e),
                            }
                        });
                    }
                    _ => {
                        log::debug!("Player event: {:?}", event);
                    }
                }
            }
        });

        Ok::<
            (
                Option<rspotify::AuthCodeSpotify>,
                player::AudioHandle,
                Arc<Mutex<telemetry::TelemetryDb>>,
                Arc<Mutex<gui::PlaybackState>>,
                config::AppConfig,
                app_settings::UserSettings,
            ),
            anyhow::Error,
        >((
            spotify_opt,
            audio,
            db,
            playback_state,
            app_config,
            user_settings,
        ))
    })?;

    let audio_handle = audio.clone();

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
                spotify_opt.clone(),
                audio_handle.clone(),
                playback_state.clone(),
                db.clone(),
                app_config.clone(),
                user_settings.clone(),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))?;

    let _ = audio.send(player::AudioCmd::Shutdown);
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
        audio.send(player::AudioCmd::Load {
            uri,
            start_playing: true,
            position_ms: 0,
        })?;
    } else {
        println!("  ⚠ No tracks resolved — nothing to play.");
    }

    Ok(())
}
