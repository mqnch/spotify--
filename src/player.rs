/// Headless audio engine powered by librespot.
///
/// Provides a thread-safe control interface for playback (play, pause,
/// skip, seek, volume) and an event channel for position tracking and
/// track-change notifications.

use std::sync::Arc;

use anyhow::{Context, Result};
use librespot::core::{
    authentication::Credentials as LibrespotCredentials,
    cache::Cache,
    config::SessionConfig,
    session::Session,
    spotify_uri::SpotifyUri,
};
use librespot::playback::{
    audio_backend,
    config::PlayerConfig,
    mixer::{self, MixerConfig},
    player::{Player, PlayerEvent, PlayerEventChannel},
};
use tokio::sync::mpsc;

// ───────────────────────────────────────────────────────────────────
// Public command enum — the GUI / caller sends these to control audio
// ───────────────────────────────────────────────────────────────────

/// Commands that can be sent to the audio engine.
#[derive(Debug)]
pub enum AudioCmd {
    /// Load and (optionally) start playing a track by Spotify URI string,
    /// e.g. `"spotify:track:4iV5W9uYEdYURa79A93R3L"`.
    Load {
        uri: String,
        start_playing: bool,
        position_ms: u32,
    },
    Play,
    Pause,
    Stop,
    Seek { position_ms: u32 },
    SetVolume { volume_u16: u16 },
    Shutdown,
}

// ───────────────────────────────────────────────────────────────────
// AudioEngine — owns the librespot Session + Player
// ───────────────────────────────────────────────────────────────────

/// A handle returned to the caller after [`AudioEngine::start`].
/// Send [`AudioCmd`]s through `cmd_tx` to control playback.
pub struct AudioHandle {
    pub cmd_tx: mpsc::UnboundedSender<AudioCmd>,
}

pub struct AudioEngine;

impl AudioEngine {
    /// Spin up the librespot session + player and return a command handle.
    ///
    /// Authentication uses librespot's built-in OAuth flow (opens a
    /// browser). Cached credentials are reused on subsequent launches.
    ///
    /// The `event_tx` sender is fed every [`PlayerEvent`] so that other
    /// subsystems (telemetry, GUI) can react to playback state changes.
    pub async fn start(
        event_tx: mpsc::UnboundedSender<PlayerEvent>,
    ) -> Result<AudioHandle> {
        // ── 1. Librespot session ─────────────────────────────────────
        let session_config = SessionConfig::default();

        // Cache credentials + audio files under .cache/onyx
        let cache_dir = dirs_or_default();
        let cache = Cache::new(
            Some(cache_dir.clone()),     // credentials
            None,                        // volume (we handle this ourselves)
            Some(cache_dir.join("audio")), // audio file cache
            None,                        // size limit
        )
        .ok();

        let session = Session::new(session_config, cache);

        // Try cached credentials first, fall back to OAuth.
        let credentials = load_or_authenticate(&session).await?;

        session
            .connect(credentials, true)
            .await
            .context("Failed to connect librespot session")?;

        println!("✓ librespot session connected (user: {}).", session.username());

        // ── 2. Mixer (software volume) ───────────────────────────────
        let mixer_config = MixerConfig::default();
        let mixer_factory = mixer::find(None) // default = "softvol"
            .expect("no mixer backend found");
        let mixer = mixer_factory(mixer_config)
            .context("Failed to create mixer")?;

        let volume_getter = mixer.get_soft_volume();

        // ── 3. Player ────────────────────────────────────────────────
        let player_config = PlayerConfig::default();

        let backend = audio_backend::find(None)
            .expect("No audio backend found (rodio should be compiled in)");

        let player = Player::new(
            player_config,
            session.clone(),
            volume_getter,
            move || backend(None, Default::default()),
        );

        // ── 4. Forward player events ─────────────────────────────────
        let mut event_channel: PlayerEventChannel = player.get_player_event_channel();

        tokio::spawn({
            let event_tx = event_tx.clone();
            async move {
                while let Some(event) = event_channel.recv().await {
                    if event_tx.send(event).is_err() {
                        break; // receiver dropped
                    }
                }
            }
        });

        // ── 5. Command loop ──────────────────────────────────────────
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AudioCmd>();

        tokio::spawn({
            let player = Arc::clone(&player);
            // mixer is Box<dyn Mixer>, just move it into the task.
            let mixer_for_vol = mixer;
            async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        AudioCmd::Load {
                            uri,
                            start_playing,
                            position_ms,
                        } => {
                            match SpotifyUri::from_uri(&uri) {
                                Ok(spotify_uri) => {
                                    player.load(spotify_uri, start_playing, position_ms);
                                }
                                Err(e) => {
                                    log::error!("Invalid Spotify URI '{}': {}", uri, e);
                                }
                            }
                        }
                        AudioCmd::Play => player.play(),
                        AudioCmd::Pause => player.pause(),
                        AudioCmd::Stop => player.stop(),
                        AudioCmd::Seek { position_ms } => player.seek(position_ms),
                        AudioCmd::SetVolume { volume_u16 } => {
                            use librespot::playback::mixer::Mixer;
                            mixer_for_vol.set_volume(volume_u16);
                            player.emit_volume_changed_event(volume_u16);
                        }
                        AudioCmd::Shutdown => {
                            player.stop();
                            break;
                        }
                    }
                }
            }
        });

        Ok(AudioHandle { cmd_tx })
    }
}

// ───────────────────────────────────────────────────────────────────
// Internal helpers
// ───────────────────────────────────────────────────────────────────

/// Determine a cache directory for librespot credentials + audio.
fn dirs_or_default() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("onyx")
}

/// Try to use credentials from the librespot cache.
/// If none exist, run the built-in OAuth flow (opens a browser).
async fn load_or_authenticate(session: &Session) -> Result<LibrespotCredentials> {
    // Check if the session's cache has stored credentials.
    if let Some(cache) = session.cache() {
        if let Some(creds) = cache.credentials() {
            log::info!("Using cached librespot credentials.");
            return Ok(creds);
        }
    }

    // No cached credentials — run interactive OAuth via librespot-oauth.
    println!();
    println!("  librespot needs to authenticate with Spotify.");
    println!("  A browser window will open for you to log in.");
    println!();

    // Scopes needed for streaming playback.
    let scopes: Vec<&str> = vec![
        "streaming",
        "user-read-playback-state",
        "user-modify-playback-state",
        "user-read-currently-playing",
        "user-read-private",
    ];

    let oauth_token = librespot::oauth::get_access_token(
        &session.config().client_id,
        &format!("http://127.0.0.1:{}/login", 8898),
        scopes,
    )
    .context("librespot OAuth failed")?;

    Ok(LibrespotCredentials::with_access_token(oauth_token.access_token))
}
