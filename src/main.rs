mod auth;
mod config;
mod lastfm;
mod metadata;
mod player;
mod spotify_api;

use anyhow::Result;
use librespot::playback::player::PlayerEvent;
use rspotify::prelude::*;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logger (set RUST_LOG=info or debug for more output).
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("onyx=info"),
    )
    .init();

    println!();
    println!("  ╔═══════════════════════════╗");
    println!("  ║       ♫  O N Y X  ♫       ║");
    println!("  ╠═══════════════════════════╣");
    println!("  ║  Minimalist Spotify Client ║");
    println!("  ╚═══════════════════════════╝");

    // ── Step 1: Ensure API keys are configured ───────────────────────
    let app_config = config::AppConfig::ensure_configured()?;

    // ── Step 2: Authenticate with Spotify Web API (rspotify) ─────────
    let spotify = auth::authenticate(&app_config).await?;

    // Verify: print the authenticated user.
    let user = spotify.current_user().await?;
    println!(
        "  Logged in as: {}",
        user.display_name.as_deref().unwrap_or("(unknown)")
    );

    // ── Step 3: Start the audio engine (librespot) ───────────────────
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PlayerEvent>();
    let audio = player::AudioEngine::start(event_tx).await?;

    // ── Step 4: Event listener (placeholder — will feed telemetry) ───
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match &event {
                PlayerEvent::Playing {
                    track_id,
                    position_ms,
                    ..
                } => {
                    log::info!("▶ Playing {:?} at {}ms", track_id, position_ms);
                }
                PlayerEvent::Paused {
                    track_id,
                    position_ms,
                    ..
                } => {
                    log::info!("⏸ Paused {:?} at {}ms", track_id, position_ms);
                }
                PlayerEvent::EndOfTrack { track_id, .. } => {
                    log::info!("⏹ End of track {:?}", track_id);
                }
                PlayerEvent::VolumeChanged { volume } => {
                    log::info!("🔊 Volume → {}", volume);
                }
                PlayerEvent::TrackChanged { audio_item } => {
                    log::info!(
                        "🎵 Track changed → {} - {}",
                        audio_item.track_id,
                        audio_item.name
                    );
                }
                _ => {
                    log::debug!("Player event: {:?}", event);
                }
            }
        }
    });

    // ── Step 5: Demo — Hybrid Metadata Pipeline ──────────────────────
    demo_metadata_pipeline(&app_config, &spotify, &audio).await?;

    // Keep the process alive until Ctrl+C.
    tokio::signal::ctrl_c().await?;

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
