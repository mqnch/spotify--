/// Headless audio engine powered by librespot.
///
/// Provides a thread-safe control interface for playback (play, pause,
/// skip, seek, volume) and an event channel for position tracking and
/// track-change notifications.
use std::sync::{Arc, Mutex};

use crate::app_settings::{EQ_BANDS, EqualizerSettings};
use anyhow::{Context, Result};
use librespot::core::{
    authentication::Credentials as LibrespotCredentials, cache::Cache, config::SessionConfig,
    session::Session, spotify_uri::SpotifyUri,
};
use librespot::playback::{NUM_CHANNELS, SAMPLE_RATE};
use librespot::playback::{
    audio_backend::{self, Sink, SinkResult},
    config::PlayerConfig,
    convert::Converter,
    decoder::AudioPacket,
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
    Preload {
        uri: String,
    },
    Play,
    Pause,
    Stop,
    Seek {
        position_ms: u32,
    },
    SetVolume {
        volume_u16: u16,
    },
    SetEqualizer(EqualizerSettings),
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
    pub fn offline() -> AudioHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AudioCmd>();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, AudioCmd::Shutdown) {
                    break;
                }
            }
        });
        AudioHandle { cmd_tx }
    }

    /// Spin up the librespot session + player and return a command handle.
    ///
    /// Authentication uses librespot's built-in OAuth flow (opens a
    /// browser). Cached credentials are reused on subsequent launches.
    ///
    /// The `event_tx` sender is fed every [`PlayerEvent`] so that other
    /// subsystems (telemetry, GUI) can react to playback state changes.
    pub async fn start(
        event_tx: mpsc::UnboundedSender<PlayerEvent>,
        equalizer: EqualizerSettings,
    ) -> Result<AudioHandle> {
        // ── 1. Librespot session ─────────────────────────────────────
        let session_config = SessionConfig::default();

        // Cache credentials + audio files under .cache/onyx
        let cache_dir = dirs_or_default();
        let cache = Cache::new(
            Some(cache_dir.clone()),       // credentials
            None,                          // volume (we handle this ourselves)
            Some(cache_dir.join("audio")), // audio file cache
            None,                          // size limit
        )
        .ok();

        let session = Session::new(session_config, cache);

        // Try cached credentials first, fall back to OAuth.
        let credentials = load_or_authenticate(&session).await?;

        session
            .connect(credentials, true)
            .await
            .context("Failed to connect librespot session")?;

        println!(
            "✓ librespot session connected (user: {}).",
            session.username()
        );

        // ── 2. Mixer (software volume) ───────────────────────────────
        let mixer_config = MixerConfig::default();
        let mixer_factory = mixer::find(None) // default = "softvol"
            .expect("no mixer backend found");
        let mixer = mixer_factory(mixer_config).context("Failed to create mixer")?;

        let volume_getter = mixer.get_soft_volume();

        // ── 3. Player ────────────────────────────────────────────────
        let player_config = PlayerConfig::default();

        let backend = audio_backend::find(None)
            .expect("No audio backend found (rodio should be compiled in)");
        let equalizer_runtime = Arc::new(Mutex::new(EqualizerRuntime::new(equalizer)));
        let sink_equalizer = Arc::clone(&equalizer_runtime);

        let player = Player::new(player_config, session.clone(), volume_getter, move || {
            Box::new(EqualizerSink::new(
                backend(None, Default::default()),
                Arc::clone(&sink_equalizer),
            ))
        });

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
            let equalizer_runtime = Arc::clone(&equalizer_runtime);
            // mixer is Box<dyn Mixer>, just move it into the task.
            let mixer_for_vol = mixer;
            async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        AudioCmd::Load {
                            uri,
                            start_playing,
                            position_ms,
                        } => match SpotifyUri::from_uri(&uri) {
                            Ok(spotify_uri) => {
                                player.load(spotify_uri, start_playing, position_ms);
                            }
                            Err(e) => {
                                log::error!("Invalid Spotify URI '{}': {}", uri, e);
                            }
                        },
                        AudioCmd::Preload { uri } => match SpotifyUri::from_uri(&uri) {
                            Ok(spotify_uri) => player.preload(spotify_uri),
                            Err(e) => {
                                log::error!("Invalid Spotify URI '{}': {}", uri, e);
                            }
                        },
                        AudioCmd::Play => player.play(),
                        AudioCmd::Pause => player.pause(),
                        AudioCmd::Stop => player.stop(),
                        AudioCmd::Seek { position_ms } => player.seek(position_ms),
                        AudioCmd::SetVolume { volume_u16 } => {
                            use librespot::playback::mixer::Mixer;
                            mixer_for_vol.set_volume(volume_u16);
                            player.emit_volume_changed_event(volume_u16);
                        }
                        AudioCmd::SetEqualizer(settings) => {
                            if let Ok(mut runtime) = equalizer_runtime.lock() {
                                runtime.set_settings(settings);
                            }
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

struct EqualizerSink {
    inner: Box<dyn Sink>,
    runtime: Arc<Mutex<EqualizerRuntime>>,
}

impl EqualizerSink {
    fn new(inner: Box<dyn Sink>, runtime: Arc<Mutex<EqualizerRuntime>>) -> Self {
        Self { inner, runtime }
    }
}

impl Sink for EqualizerSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        match packet {
            AudioPacket::Samples(mut samples) => {
                if let Ok(mut runtime) = self.runtime.lock() {
                    runtime.process(&mut samples);
                }
                self.inner.write(AudioPacket::Samples(samples), converter)
            }
            AudioPacket::Raw(samples) => self.inner.write(AudioPacket::Raw(samples), converter),
        }
    }
}

struct EqualizerRuntime {
    settings: EqualizerSettings,
    filters: Vec<[Biquad; NUM_CHANNELS as usize]>,
}

impl EqualizerRuntime {
    fn new(settings: EqualizerSettings) -> Self {
        let filters = build_filters(&settings);
        Self { settings, filters }
    }

    fn set_settings(&mut self, settings: EqualizerSettings) {
        self.settings = settings;
        self.filters = build_filters(&self.settings);
    }

    fn process(&mut self, samples: &mut [f64]) {
        if !self.settings.enabled {
            return;
        }

        let preamp = db_to_gain(self.settings.preamp_db);
        for frame in samples.chunks_mut(NUM_CHANNELS as usize) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let mut value = *sample * preamp;
                for band in &mut self.filters {
                    value = band[channel].process(value);
                }
                *sample = value.clamp(-1.0, 1.0);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn peaking_eq(frequency_hz: f32, gain_db: f32, q: f64) -> Self {
        if gain_db.abs() < 0.01 {
            return Self::identity();
        }

        let frequency_hz = frequency_hz as f64;
        let gain_db = gain_db as f64;
        let a = 10.0_f64.powf(gain_db / 40.0);
        let omega = 2.0 * std::f64::consts::PI * frequency_hz / SAMPLE_RATE as f64;
        let alpha = omega.sin() / (2.0 * q);
        let cos = omega.cos();

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, sample: f64) -> f64 {
        let output = self.b0 * sample + self.z1;
        self.z1 = self.b1 * sample - self.a1 * output + self.z2;
        self.z2 = self.b2 * sample - self.a2 * output;
        output
    }
}

fn build_filters(settings: &EqualizerSettings) -> Vec<[Biquad; NUM_CHANNELS as usize]> {
    EQ_BANDS
        .iter()
        .zip(settings.bands_db.iter())
        .map(|(band, gain_db)| {
            let filter = Biquad::peaking_eq(band.frequency_hz, *gain_db, 1.0);
            [filter; NUM_CHANNELS as usize]
        })
        .collect()
}

fn db_to_gain(db: f32) -> f64 {
    10.0_f64.powf(db as f64 / 20.0)
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

    Ok(LibrespotCredentials::with_access_token(
        oauth_token.access_token,
    ))
}
