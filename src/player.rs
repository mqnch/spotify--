/// Headless audio engine powered by librespot.
///
/// Provides a thread-safe control interface for playback (play, pause,
/// skip, seek, volume) and an event channel for position tracking and
/// track-change notifications.
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
// Shared state for (re)starting the live engine
// ───────────────────────────────────────────────────────────────────

/// Data needed to spawn or reconnect the librespot session (same event channel + EQ + client id).
#[derive(Clone)]
pub struct AudioWarmState {
    pub event_tx: mpsc::UnboundedSender<PlayerEvent>,
    pub equalizer: Arc<Mutex<EqualizerSettings>>,
    pub client_id: String,
}

// ───────────────────────────────────────────────────────────────────
// AudioEngine — owns the librespot Session + Player
// ───────────────────────────────────────────────────────────────────

/// A handle returned to the caller after [`AudioEngine::start_live`] or [`AudioEngine::offline`].
/// Send [`AudioCmd`]s through [`Self::send`]. Use [`Self::reconnect_live_session`] after refreshing
/// the Spotify Web API access token (e.g. session drop).
#[derive(Clone)]
pub struct AudioHandle {
    cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<AudioCmd>>>>,
    pub warm: Arc<AudioWarmState>,
}

impl AudioHandle {
    pub fn send(&self, cmd: AudioCmd) -> anyhow::Result<()> {
        let guard = self
            .cmd_tx
            .lock()
            .map_err(|e| anyhow::anyhow!("audio cmd mutex poisoned: {}", e))?;
        let tx = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("audio engine is not running"))?;
        tx.send(cmd)
            .map_err(|e| anyhow::anyhow!("failed to send audio command: {}", e))
    }

    /// Stop the current engine (if any) and start a new librespot session with a fresh token.
    ///
    /// Only swaps the command channel after the new engine is ready, so a failed reconnect
    /// never leaves [`Self::send`] with no sender.
    pub async fn reconnect_live_session(&self, access_token: &str) -> Result<()> {
        let new_tx = spawn_live_engine(
            self.warm.clone(),
            Some(access_token.to_string()),
        )
        .await?;

        let old_tx = {
            let mut guard = self.cmd_tx.lock().unwrap();
            let old = guard.take();
            *guard = Some(new_tx);
            old
        };

        if let Some(tx) = old_tx {
            let _ = tx.send(AudioCmd::Shutdown);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        Ok(())
    }
}

pub struct AudioEngine;

impl AudioEngine {
    /// No playback; commands are accepted but playback-related ops are no-ops.
    /// `SetEqualizer` still updates [`AudioWarmState::equalizer`] so a later live session picks up the latest settings.
    pub fn offline(warm: Arc<AudioWarmState>) -> AudioHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AudioCmd>();
        let warm_loop = warm.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    AudioCmd::Shutdown => break,
                    AudioCmd::SetEqualizer(settings) => {
                        if let Ok(mut g) = warm_loop.equalizer.lock() {
                            *g = settings;
                        }
                    }
                    _ => {}
                }
            }
        });
        AudioHandle {
            cmd_tx: Arc::new(Mutex::new(Some(cmd_tx))),
            warm,
        }
    }

    /// Spin up librespot using a Spotify Web API access token (same OAuth app as rspotify),
    /// or cached librespot credentials when `spotify_access_token` is `None`.
    ///
    /// When an access token is supplied and there is no cached librespot credential yet,
    /// librespot performs a single sign-on without opening a second browser OAuth flow.
    pub async fn start_live(
        warm: Arc<AudioWarmState>,
        spotify_access_token: Option<String>,
    ) -> Result<AudioHandle> {
        let cmd_tx = spawn_live_engine(warm.clone(), spotify_access_token).await?;
        Ok(AudioHandle {
            cmd_tx: Arc::new(Mutex::new(Some(cmd_tx))),
            warm,
        })
    }
}

async fn spawn_live_engine(
    warm: Arc<AudioWarmState>,
    spotify_access_token: Option<String>,
) -> Result<mpsc::UnboundedSender<AudioCmd>> {
    // SpClient (extended-metadata, CDN, etc.) must use librespot's default client ID (keymaster on
    // desktop). Overriding this with your Spotify *Developer Dashboard* app ID breaks track load
    // with HTTP 400/403 even for Premium — OAuth tokens are still minted with your app via
    // rspotify; see librespot PR #1309 / discussion on SessionConfig::client_id vs OAuth client_id.
    let session_config = SessionConfig::default();

    let cache_dir = dirs_or_default();
    let cache = Cache::new(
        Some(cache_dir.clone()),
        None,
        Some(cache_dir.join("audio")),
        None,
    )
    .ok();

    let session = Session::new(session_config, cache);

    let credentials = load_or_authenticate(
        &session,
        spotify_access_token.as_deref(),
        warm.client_id.as_str(),
    )
    .await?;

    session
        .connect(credentials, true)
        .await
        .context("Failed to connect librespot session")?;

    println!(
        "✓ librespot session connected (user: {}).",
        session.username()
    );

    let mixer_config = MixerConfig::default();
    let mixer_factory = mixer::find(None).expect("no mixer backend found");
    let mixer = mixer_factory(mixer_config).context("Failed to create mixer")?;
    // SoftMixer starts at ~50% mapped gain; GUI defaults to full until the user moves the slider.
    mixer.set_volume(u16::MAX);

    let volume_getter = mixer.get_soft_volume();

    let player_config = PlayerConfig::default();

    let backend = audio_backend::find(None).expect("No audio backend found (rodio should be compiled in)");
    let eq_settings = warm.equalizer.lock().unwrap().clone();
    let equalizer_runtime = Arc::new(Mutex::new(EqualizerRuntime::new(eq_settings)));
    let sink_equalizer = Arc::clone(&equalizer_runtime);

    let player = Player::new(player_config, session.clone(), volume_getter, move || {
        Box::new(EqualizerSink::new(
            backend(None, Default::default()),
            Arc::clone(&sink_equalizer),
        ))
    });

    let mut event_channel: PlayerEventChannel = player.get_player_event_channel();

    let event_tx = warm.event_tx.clone();
    tokio::spawn(async move {
        while let Some(event) = event_channel.recv().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AudioCmd>();
    let equalizer_shared = warm.equalizer.clone();

    tokio::spawn({
        let player = Arc::clone(&player);
        let equalizer_runtime = Arc::clone(&equalizer_runtime);
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
                            log::info!(
                                "Audio engine load: {} (play={}, position_ms={})",
                                uri,
                                start_playing,
                                position_ms
                            );
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
                        mixer_for_vol.set_volume(volume_u16);
                        player.emit_volume_changed_event(volume_u16);
                    }
                    AudioCmd::SetEqualizer(settings) => {
                        if let Ok(mut shared) = equalizer_shared.lock() {
                            *shared = settings.clone();
                        }
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

    Ok(cmd_tx)
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

fn dirs_or_default() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("onyx")
}

/// Prefer the current Web API access token when provided (same session as rspotify), then
/// cached librespot credentials, then interactive OAuth.
///
/// Checking the token **before** disk cache avoids stale `credentials.json` (e.g. after
/// re-auth or scope changes) blocking playback while the Web API still works.
///
/// `webapp_client_id` is your Spotify Developer app client ID, used only for librespot's browser
/// OAuth helper — not for [`SessionConfig::client_id`], which must stay at librespot's default.
async fn load_or_authenticate(
    session: &Session,
    spotify_access_token: Option<&str>,
    webapp_client_id: &str,
) -> Result<LibrespotCredentials> {
    if let Some(token) = spotify_access_token.filter(|t| !t.is_empty()) {
        log::info!("Connecting librespot with Spotify Web API access token.");
        return Ok(LibrespotCredentials::with_access_token(token.to_string()));
    }

    if let Some(cache) = session.cache() {
        if let Some(creds) = cache.credentials() {
            log::info!("Using cached librespot credentials.");
            return Ok(creds);
        }
    }

    println!();
    println!("  librespot needs to authenticate with Spotify.");
    println!("  A browser window will open for you to log in.");
    println!();

    let scopes: Vec<&str> = vec![
        "streaming",
        "user-read-playback-state",
        "user-modify-playback-state",
        "user-read-currently-playing",
        "user-read-private",
    ];

    let oauth_token = librespot::oauth::get_access_token(
        webapp_client_id.trim(),
        &format!("http://127.0.0.1:{}/login", 8898),
        scopes,
    )
    .context("librespot OAuth failed")?;

    Ok(LibrespotCredentials::with_access_token(
        oauth_token.access_token,
    ))
}
