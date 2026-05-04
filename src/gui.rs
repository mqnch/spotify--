use crate::app_settings::{EQ_BANDS, EqualizerSettings, PlaylistOrderingSettings, UserSettings};
use crate::config::AppConfig;
use crate::downloads::{DOWNLOAD_DOWNLOADED, DOWNLOAD_DOWNLOADING, DownloadStatuses};
use crate::player::{AudioCmd, AudioHandle};
use crate::spotify_api::{
    ArtistAlbumSummary, ArtistProfile, PlaylistSummary, PlaylistTrack, artist_queue_playlist_id,
};
use crate::telemetry::{ListeningStats, RankedItem, StatsDateRange, StatsMetric, TelemetryDb};
use chrono::{DateTime, Datelike, Utc};
use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use rspotify::AuthCodeSpotify;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub track_name: String,
    pub artist_name: String,
    pub artist_id: Option<String>,
    pub artwork_url: Option<String>,
    pub spotify_uri: Option<String>,
    pub position_ms: u32,
    pub position_anchor_ms: u32,
    pub position_updated_at: Option<Instant>,
    pub duration_ms: u32,
    pub volume: u16,
    pub end_count: u64,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            is_playing: false,
            track_name: String::new(),
            artist_name: String::new(),
            artist_id: None,
            artwork_url: None,
            spotify_uri: None,
            position_ms: 0,
            position_anchor_ms: 0,
            position_updated_at: None,
            duration_ms: 0,
            volume: u16::MAX,
            end_count: 0,
        }
    }
}

#[derive(Clone)]
enum PlaylistStatus {
    Idle,
    Loading,
    Refreshing,
    Cached,
    Loaded,
    RateLimited(String),
    Error(String),
}

#[derive(Clone)]
struct PlaylistLoadState {
    playlist_id: Option<String>,
    generation: u64,
    tracks: Vec<PlaylistTrack>,
    status: PlaylistStatus,
    complete: bool,
}

#[derive(Clone)]
enum ArtistPageStatus {
    Idle,
    Loading,
    Loaded,
    Error(String),
}

#[derive(Clone)]
struct ArtistLoadState {
    artist_id: Option<String>,
    generation: u64,
    name_hint: String,
    profile: Option<ArtistProfile>,
    popular_tracks: Vec<PlaylistTrack>,
    albums: Vec<ArtistAlbumSummary>,
    status: ArtistPageStatus,
    /// When set (e.g. Last.fm), shown instead of Spotify follower formatting.
    listener_display: Option<String>,
    /// Show up to 10 popular rows instead of 5.
    popular_show_all: bool,
}

impl Default for ArtistLoadState {
    fn default() -> Self {
        Self {
            artist_id: None,
            generation: 0,
            name_hint: String::new(),
            profile: None,
            popular_tracks: Vec::new(),
            albums: Vec::new(),
            status: ArtistPageStatus::Idle,
            listener_display: None,
            popular_show_all: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainView {
    Dashboard,
    Playlist,
    Artist,
    Settings,
}

#[derive(Clone, Copy)]
enum IconKind {
    Home,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatsRangeMode {
    AllTime,
    Year,
    Month,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RankingKind {
    Tracks,
    Artists,
}

/// Central panel frame uses zero margin; this is the only horizontal inset so we do not double up
/// with scroll padding (which previously produced thick “bars” on top and sides).
const CENTRAL_CONTENT_INSET: f32 = 24.0;
/// Listening stats 2×2 grid: equal row and column gutter (px).
const STATS_GRID_GAP: f32 = 12.0;
/// Spotify-style accent for sliders, toggles, and primary actions.
const ACCENT_GREEN: egui::Color32 = egui::Color32::from_rgb(30, 215, 96);
/// Maps linear slider position to `u16` volume with more usable range in the lower half of the bar.
const VOLUME_SLIDER_EXP: f32 = 0.5;
/// Slider-space step for keyboard volume (↑ / ↓).
const VOLUME_KEYBOARD_STEP_T: f32 = 0.06;
/// Two arrow key presses within this window trigger previous/next track.
const ARROW_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

#[inline]
fn volume_u16_to_slider_t(volume: u16) -> f32 {
    if volume == 0 {
        0.0
    } else {
        let n = volume as f32 / u16::MAX as f32;
        n.powf(1.0 / VOLUME_SLIDER_EXP)
    }
}

#[inline]
fn volume_slider_t_to_u16(t: f32) -> u16 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        0
    } else {
        let curved = t.powf(VOLUME_SLIDER_EXP);
        ((curved * u16::MAX as f32).round() as u32).min(u16::MAX as u32) as u16
    }
}

#[inline]
fn volume_step_slider(volume: u16, delta_t: f32) -> u16 {
    let t = volume_u16_to_slider_t(volume) + delta_t;
    volume_slider_t_to_u16(t)
}

fn consume_arrow_double_tap(last: &mut Option<Instant>, key_pressed: bool) -> bool {
    if !key_pressed {
        return false;
    }
    let now = Instant::now();
    let trigger = last.is_some_and(|t| now.duration_since(t) <= ARROW_DOUBLE_TAP_WINDOW);
    if trigger {
        *last = None;
        true
    } else {
        *last = Some(now);
        false
    }
}

#[derive(Clone, Copy)]
struct TrackTableLayout {
    index: f32,
    title: f32,
    album: f32,
    added: f32,
    duration: f32,
    gap: f32,
}

impl Default for PlaylistLoadState {
    fn default() -> Self {
        Self {
            playlist_id: None,
            generation: 0,
            tracks: Vec::new(),
            status: PlaylistStatus::Idle,
            complete: false,
        }
    }
}

impl TrackTableLayout {
    fn for_width(width: f32) -> Self {
        let index = 28.0;
        let duration = 58.0;
        let added = 120.0;
        let album = (width * 0.24).clamp(140.0, 220.0);
        let gap = 12.0;
        let title = (width - index - album - added - duration - gap * 4.0).max(160.0);
        Self {
            index,
            title,
            album,
            added,
            duration,
            gap,
        }
    }

    fn rects(self, rect: egui::Rect) -> TrackTableRects {
        let mut left = rect.left();
        let index = egui::Rect::from_min_size(
            egui::pos2(left, rect.top()),
            egui::vec2(self.index, rect.height()),
        );
        left = index.right() + self.gap;
        let title = egui::Rect::from_min_size(
            egui::pos2(left, rect.top()),
            egui::vec2(self.title, rect.height()),
        );
        left = title.right() + self.gap;
        let album = egui::Rect::from_min_size(
            egui::pos2(left, rect.top()),
            egui::vec2(self.album, rect.height()),
        );
        left = album.right() + self.gap;
        let added = egui::Rect::from_min_size(
            egui::pos2(left, rect.top()),
            egui::vec2(self.added, rect.height()),
        );
        let duration = egui::Rect::from_min_size(
            egui::pos2(rect.right() - self.duration, rect.top()),
            egui::vec2(self.duration, rect.height()),
        );
        TrackTableRects {
            index,
            title,
            album,
            added,
            duration,
        }
    }
}

#[derive(Clone, Copy)]
struct TrackTableRects {
    index: egui::Rect,
    title: egui::Rect,
    album: egui::Rect,
    added: egui::Rect,
    duration: egui::Rect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackSortColumn {
    Index,
    Title,
    Album,
    DateAdded,
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackSortDirection {
    Asc,
    Desc,
}

const MAX_RECENT_PLAYLISTS: usize = 50;

pub struct OnyxApp {
    pub spotify: Option<AuthCodeSpotify>,
    pub audio_handle: AudioHandle,
    pub playback_state: Arc<Mutex<PlaybackState>>,
    pub db: Arc<Mutex<TelemetryDb>>,

    /// OAuth / playlist fetch from the Connect UI (None until completed).
    spotify_connect_result: Arc<Mutex<Option<Result<AuthCodeSpotify, String>>>>,
    spotify_login_busy: bool,
    spotify_login_error: Option<String>,

    listening_stats: ListeningStats,
    stats_status: Option<String>,
    stats_range_mode: StatsRangeMode,
    selected_stats_year: i32,
    selected_stats_month: u32,
    track_stats_metric: StatsMetric,
    artist_stats_metric: StatsMetric,
    track_stats_limit: u32,
    artist_stats_limit: u32,
    main_view: MainView,
    app_config: AppConfig,
    config_draft: AppConfig,
    user_settings: UserSettings,
    settings_status: Option<String>,

    // Phase 5 Additions
    pub rt: tokio::runtime::Handle,
    playlists: Arc<Mutex<Vec<PlaylistSummary>>>,
    playlists_status: Arc<Mutex<String>>,
    download_statuses: DownloadStatuses,
    download_tasks: HashMap<String, JoinHandle<()>>,
    selected_playlist: Option<PlaylistSummary>,
    playlist_state: Arc<Mutex<PlaylistLoadState>>,
    playlist_colors: Arc<Mutex<HashMap<String, Option<[u8; 3]>>>>,
    playlist_generation: u64,
    playlist_task: Option<JoinHandle<()>>,
    queue: Vec<PlaylistTrack>,
    queue_playlist_id: Option<String>,
    queue_index: Option<usize>,
    pending_autoplay_playlist_id: Option<String>,
    pending_queue_index: Option<usize>,
    last_queue_load_at: Option<Instant>,
    observed_end_count: u64,
    stats_refresh_due_at: Option<Instant>,
    last_sent_volume: u16,
    previous_volume: u16,
    arrow_left_last_tap: Option<Instant>,
    arrow_right_last_tap: Option<Instant>,

    // Playback state toggles
    shuffle: bool,
    repeat: bool,

    /// None = preserve API / playlist order for this view.
    track_sort: Option<(TrackSortColumn, TrackSortDirection)>,

    artist_state: Arc<Mutex<ArtistLoadState>>,
    artist_generation: u64,
    artist_task: Option<JoinHandle<()>>,
    /// Original track order for the current queue (before shuffle), for restoring when shuffle is toggled off.
    queue_original_tracks: Vec<PlaylistTrack>,
}

impl OnyxApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        spotify: Option<AuthCodeSpotify>,
        audio_handle: AudioHandle,
        playback_state: Arc<Mutex<PlaybackState>>,
        db: Arc<Mutex<TelemetryDb>>,
        app_config: AppConfig,
        user_settings: UserSettings,
    ) -> Self {
        // Spotify-like Visuals
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(18, 18, 18); // Default background
        visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(179, 179, 179); // Gray text
        visuals.selection.bg_fill = egui::Color32::from_rgb(30, 215, 96); // Spotify Green
        cc.egui_ctx.set_visuals(visuals);
        configure_ui_fonts(&cc.egui_ctx);

        let current_year = Utc::now().year();
        let current_month = Utc::now().month();
        let (listening_stats, stats_status) = {
            if let Ok(db_lock) = db.lock() {
                match db_lock.listening_stats_for_range(
                    StatsDateRange::AllTime,
                    10,
                    StatsMetric::Plays,
                    StatsMetric::Plays,
                ) {
                    Ok(stats) => (stats, None),
                    Err(e) => (
                        ListeningStats::default(),
                        Some(format!("Failed to load listening stats: {}", e)),
                    ),
                }
            } else {
                (
                    ListeningStats::default(),
                    Some("Failed to access listening stats database.".to_string()),
                )
            }
        };

        let mut cached_playlists = crate::playlist_cache::PlaylistCache::new()
            .and_then(|cache| cache.load_playlists())
            .unwrap_or_else(|e| {
                log::warn!("Failed to load cached playlists: {}", e);
                Vec::new()
            });
        if !cached_playlists
            .iter()
            .any(|p| crate::spotify_api::is_liked_songs_playlist(&p.id))
        {
            cached_playlists.insert(0, crate::spotify_api::liked_songs_summary(0));
        }
        let cache_only = crate::spotify_api::cache_only_mode();
        let playlists_status_text = if cache_only && cached_playlists.is_empty() {
            "Cache-only mode: no cached playlists yet.".to_string()
        } else if cache_only {
            "Cache-only mode: Spotify API disabled.".to_string()
        } else if spotify.is_none() {
            "Sign in to Spotify to load your library.".to_string()
        } else if cached_playlists.is_empty() {
            "Loading playlists...".to_string()
        } else {
            "Refreshing playlists...".to_string()
        };
        let playlists = Arc::new(Mutex::new(cached_playlists));
        let playlists_status = Arc::new(Mutex::new(playlists_status_text));
        let download_statuses = Arc::new(Mutex::new(
            crate::playlist_cache::PlaylistCache::new()
                .and_then(|cache| cache.load_download_statuses())
                .map(|statuses| {
                    statuses
                        .into_iter()
                        .map(|status| (status.playlist_id.clone(), status))
                        .collect()
                })
                .unwrap_or_else(|e| {
                    log::warn!("Failed to load download statuses: {}", e);
                    HashMap::new()
                }),
        ));
        let playlists_clone = playlists.clone();
        let playlists_status_clone = playlists_status.clone();
        let ctx_clone = cc.egui_ctx.clone();

        egui_extras::install_image_loaders(&cc.egui_ctx);

        if !cache_only {
            if let Some(spotify_clone) = spotify.clone() {
                rt.spawn(async move {
                match crate::spotify_api::user_playlists(&spotify_clone).await {
                    Ok(mut pl) => {
                        let liked_total =
                            crate::spotify_api::user_saved_tracks_total(&spotify_clone)
                                .await
                                .unwrap_or_else(|e| {
                                    log::warn!("Failed to fetch liked songs count: {}", e);
                                    0
                                });
                        pl.insert(0, crate::spotify_api::liked_songs_summary(liked_total));
                        let count = pl.len();
                        if let Ok(cache) = crate::playlist_cache::PlaylistCache::new() {
                            for playlist in &pl {
                                if let Err(e) = cache.save_playlist(playlist, false) {
                                    log::warn!("Failed to cache playlist metadata: {}", e);
                                }
                            }
                        }
                        if let Ok(mut lock) = playlists_clone.lock() {
                            *lock = pl;
                        }
                        if let Ok(mut status) = playlists_status_clone.lock() {
                            if count == 0 {
                                *status = "No playlists found.".to_string();
                            } else {
                                status.clear();
                            }
                        }
                        log::info!("Loaded {} playlists", count);
                    }
                    Err(e) => {
                        log::error!("Failed to fetch user playlists: {:#}", e);
                        let rate_limit_message = crate::spotify_api::rate_limit_status_message(&e);
                        let cached_count = playlists_clone
                            .lock()
                            .map(|playlists| playlists.len())
                            .unwrap_or_default();
                        if let Ok(mut status) = playlists_status_clone.lock() {
                            *status = if let Some(message) = rate_limit_message {
                                if cached_count > 0 {
                                    format!("Showing cached playlists. {}", message)
                                } else {
                                    message
                                }
                            } else if cached_count > 0 {
                                format!("Showing cached playlists. Spotify request failed: {:#}", e)
                            } else {
                                format!("Failed to load playlists: {:#}", e)
                            };
                        }
                    }
                }
                ctx_clone.request_repaint();
            });
            }
        }

        Self {
            spotify,
            audio_handle,
            spotify_connect_result: Arc::new(Mutex::new(None)),
            spotify_login_busy: false,
            spotify_login_error: None,
            playback_state,
            db,
            listening_stats,
            stats_status,
            stats_range_mode: StatsRangeMode::AllTime,
            selected_stats_year: current_year,
            selected_stats_month: current_month,
            track_stats_metric: StatsMetric::Plays,
            artist_stats_metric: StatsMetric::Plays,
            track_stats_limit: 10,
            artist_stats_limit: 10,
            main_view: MainView::Dashboard,
            app_config: app_config.clone(),
            config_draft: app_config,
            user_settings,
            settings_status: None,
            rt,
            playlists,
            playlists_status,
            download_statuses,
            download_tasks: HashMap::new(),
            selected_playlist: None,
            playlist_state: Arc::new(Mutex::new(PlaylistLoadState::default())),
            playlist_colors: Arc::new(Mutex::new(HashMap::new())),
            playlist_generation: 0,
            playlist_task: None,
            queue: Vec::new(),
            queue_playlist_id: None,
            queue_index: None,
            pending_autoplay_playlist_id: None,
            pending_queue_index: None,
            last_queue_load_at: None,
            observed_end_count: 0,
            stats_refresh_due_at: None,
            last_sent_volume: u16::MAX,
            previous_volume: u16::MAX,
            arrow_left_last_tap: None,
            arrow_right_last_tap: None,
            shuffle: false,
            repeat: false,
            track_sort: None,
            artist_state: Arc::new(Mutex::new(ArtistLoadState::default())),
            artist_generation: 0,
            artist_task: None,
            queue_original_tracks: Vec::new(),
        }
    }

    fn poll_spotify_connect_result(&mut self, ctx: &egui::Context) {
        let taken = self
            .spotify_connect_result
            .lock()
            .ok()
            .and_then(|mut g| g.take());
        let Some(result) = taken else {
            return;
        };
        self.spotify_login_busy = false;
        match result {
            Ok(client) => {
                self.spotify = Some(client.clone());
                self.spotify_login_error = None;
                self.spawn_playlist_refresh(&client, ctx);
                let audio = self.audio_handle.clone();
                let ctx2 = ctx.clone();
                let client2 = client.clone();
                self.rt.spawn(async move {
                    if let Some(tok) = crate::auth::access_token(&client2).await {
                        if let Err(e) = audio.reconnect_live_session(&tok).await {
                            log::error!("Live audio after Spotify login failed: {}", e);
                        }
                    }
                    ctx2.request_repaint();
                });
            }
            Err(e) => {
                self.spotify_login_error = Some(e);
            }
        }
    }

    fn spawn_playlist_refresh(&self, spotify: &AuthCodeSpotify, ctx: &egui::Context) {
        let spotify_clone = spotify.clone();
        let playlists_clone = self.playlists.clone();
        let playlists_status_clone = self.playlists_status.clone();
        let ctx_clone = ctx.clone();
        self.rt.spawn(async move {
            match crate::spotify_api::user_playlists(&spotify_clone).await {
                Ok(mut pl) => {
                    let liked_total = crate::spotify_api::user_saved_tracks_total(&spotify_clone)
                        .await
                        .unwrap_or_else(|e| {
                            log::warn!("Failed to fetch liked songs count: {}", e);
                            0
                        });
                    pl.insert(0, crate::spotify_api::liked_songs_summary(liked_total));
                    let count = pl.len();
                    if let Ok(cache) = crate::playlist_cache::PlaylistCache::new() {
                        for playlist in &pl {
                            if let Err(e) = cache.save_playlist(playlist, false) {
                                log::warn!("Failed to cache playlist metadata: {}", e);
                            }
                        }
                    }
                    if let Ok(mut lock) = playlists_clone.lock() {
                        *lock = pl;
                    }
                    if let Ok(mut status) = playlists_status_clone.lock() {
                        if count == 0 {
                            *status = "No playlists found.".to_string();
                        } else {
                            status.clear();
                        }
                    }
                    log::info!("Loaded {} playlists", count);
                }
                Err(e) => {
                    log::error!("Failed to fetch user playlists: {:#}", e);
                    let rate_limit_message = crate::spotify_api::rate_limit_status_message(&e);
                    let cached_count = playlists_clone
                        .lock()
                        .map(|playlists| playlists.len())
                        .unwrap_or_default();
                    if let Ok(mut status) = playlists_status_clone.lock() {
                        *status = if let Some(message) = rate_limit_message {
                            if cached_count > 0 {
                                format!("Showing cached playlists. {}", message)
                            } else {
                                message
                            }
                        } else if cached_count > 0 {
                            format!("Showing cached playlists. Spotify request failed: {:#}", e)
                        } else {
                            format!("Failed to load playlists: {:#}", e)
                        };
                    }
                }
            }
            ctx_clone.request_repaint();
        });
    }

    fn render_spotify_connect_gate(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading(
                egui::RichText::new("Connect to Spotify")
                    .color(egui::Color32::WHITE)
                    .size(22.0),
            );
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(
                    "Your saved session is missing or was revoked. Sign in again — your API keys stay in the system keyring.",
                )
                .color(egui::Color32::from_rgb(179, 179, 179))
                .size(14.0),
            );
            if let Some(err) = &self.spotify_login_error {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(err)
                        .color(egui::Color32::from_rgb(255, 120, 120))
                        .size(13.0),
                );
            }
            ui.add_space(28.0);
            let can_click = !self.spotify_login_busy;
            if ui
                .add_enabled(
                    can_click,
                    egui::Button::new(egui::RichText::new("Sign in with Spotify").size(15.0)),
                )
                .clicked()
            {
                self.spotify_login_busy = true;
                self.spotify_login_error = None;
                let pending = self.spotify_connect_result.clone();
                let cfg = self.app_config.clone();
                self.rt.spawn(async move {
                    let spotify = crate::auth::create_spotify_client(&cfg);
                    let out = match crate::auth::authenticate_interactive(&spotify).await {
                        Ok(()) => Ok(spotify),
                        Err(e) => Err(e.to_string()),
                    };
                    if let Ok(mut g) = pending.lock() {
                        *g = Some(out);
                    }
                });
            }
            if self.spotify_login_busy {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Complete login in your browser…")
                            .color(egui::Color32::from_rgb(179, 179, 179)),
                    );
                });
                ctx.request_repaint();
            }
        });
    }

    fn artist_color_cache_key(artist_id: &str) -> String {
        let id = artist_id.split(':').last().unwrap_or(artist_id);
        format!("artist:{id}")
    }

    fn open_artist_page(&mut self, artist_id: String, name_hint: String, ctx: &egui::Context) {
        if let Some(t) = self.artist_task.take() {
            t.abort();
        }
        self.artist_generation += 1;
        let generation = self.artist_generation;
        self.main_view = MainView::Artist;
        self.track_sort = None;

        let id_clean = artist_id
            .split(':')
            .last()
            .unwrap_or(artist_id.as_str())
            .to_string();

        if let Ok(mut s) = self.artist_state.lock() {
            s.artist_id = Some(id_clean.clone());
            s.generation = generation;
            s.name_hint = name_hint;
            s.profile = None;
            s.popular_tracks.clear();
            s.albums.clear();
            s.listener_display = None;
            s.popular_show_all = false;
            s.status = ArtistPageStatus::Loading;
        }

        let cache_only = crate::spotify_api::cache_only_mode();
        if cache_only {
            if let Ok(mut s) = self.artist_state.lock() {
                if s.generation == generation {
                    s.status = ArtistPageStatus::Error(
                        "Cache-only mode: artist pages need the Spotify API.".to_string(),
                    );
                }
            }
            ctx.request_repaint();
            return;
        }

        let Some(spotify) = self.spotify.clone() else {
            if let Ok(mut s) = self.artist_state.lock() {
                if s.generation == generation {
                    s.status = ArtistPageStatus::Error(
                        "Sign in to Spotify to load this artist.".to_string(),
                    );
                }
            }
            ctx.request_repaint();
            return;
        };

        if let Ok(cache) = crate::artist_cache::ArtistCache::new() {
            if let Ok(Some(page)) = cache.load(id_clean.as_str()) {
                let fresh = crate::artist_cache::is_fresh(&page);
                if let Ok(mut s) = self.artist_state.lock() {
                    if s.generation == generation {
                        s.profile = Some(page.profile.clone());
                        s.popular_tracks = page.popular_tracks.clone();
                        s.albums = page.albums.clone();
                        s.listener_display = page.listener_display.clone();
                        s.popular_show_all = false;
                        s.status = ArtistPageStatus::Loaded;
                    }
                }
                ctx.request_repaint();
                if fresh {
                    return;
                }
            }
        }

        let state = self.artist_state.clone();
        let ctx2 = ctx.clone();
        let aid = id_clean.clone();
        let lastfm_key = self.app_config.lastfm_api_key.trim().to_string();
        self.artist_task = Some(self.rt.spawn(async move {
            let profile = match crate::spotify_api::fetch_artist_profile(&spotify, &aid).await {
                Ok(p) => p,
                Err(e) => {
                    if let Ok(mut lock) = state.lock() {
                        if lock.generation == generation {
                            lock.status = ArtistPageStatus::Error(e.to_string());
                        }
                    }
                    ctx2.request_repaint();
                    return;
                }
            };

            let listener_display = if profile.followers == 0 && !lastfm_key.is_empty() {
                let lf = crate::lastfm::LastFmClient::new(&lastfm_key);
                lf.artist_info(&profile.name)
                    .await
                    .ok()
                    .and_then(|i| crate::lastfm::format_listener_line(&i))
            } else {
                None
            };

            let market = crate::spotify_api::catalog_market(&spotify).await;

            let (popular, albums) = if !lastfm_key.is_empty() {
                let lf = crate::lastfm::LastFmClient::new(&lastfm_key);
                let top = match lf.artist_top_tracks(&profile.name, 10).await {
                    Ok(t) => t,
                    Err(e) => {
                        log::warn!("Last.fm artist top tracks: {e:#}");
                        Vec::new()
                    }
                };
                let pairs: Vec<(String, String)> = top
                    .iter()
                    .map(|t| (t.artist.name.clone(), t.name.clone()))
                    .collect();
                let matcher = crate::metadata::IdMatcher::new(spotify.clone());
                let resolved = matcher.resolve_batch(pairs).await;
                let pop_fut = crate::spotify_api::popular_tracks_from_resolved(
                    &spotify,
                    resolved,
                    &aid,
                    Some(market),
                );
                let alb_fut = crate::spotify_api::fetch_artist_albums(&spotify, &aid, market);
                let (popular_r, albums_r) = tokio::join!(pop_fut, alb_fut);
                let popular = popular_r.unwrap_or_else(|e| {
                    log::warn!("Enrich Last.fm popular tracks: {e:#}");
                    Vec::new()
                });
                let albums = albums_r.unwrap_or_else(|e| {
                    log::warn!("Artist albums: {e:#}");
                    Vec::new()
                });
                (popular, albums)
            } else {
                let popular =
                    match crate::spotify_api::fetch_artist_top_tracks(&spotify, &aid, market).await
                    {
                        Ok(tr) => tr,
                        Err(e) => {
                            log::warn!("Artist top tracks: {e:#}");
                            Vec::new()
                        }
                    };
                let albums =
                    match crate::spotify_api::fetch_artist_albums(&spotify, &aid, market).await {
                        Ok(a) => a,
                        Err(e) => {
                            log::warn!("Artist albums: {e:#}");
                            Vec::new()
                        }
                    };
                (popular, albums)
            };

            if let Ok(mut lock) = state.lock() {
                if lock.generation != generation {
                    return;
                }
                lock.profile = Some(profile.clone());
                lock.popular_tracks = popular.clone();
                lock.albums = albums.clone();
                lock.listener_display = listener_display.clone();
                lock.status = ArtistPageStatus::Loaded;
            }

            let page = crate::artist_cache::CachedArtistPage::with_timestamp(
                profile,
                popular,
                albums,
                listener_display,
            );
            if let Ok(mut c) = crate::artist_cache::ArtistCache::new() {
                if let Err(e) = c.save(&aid, &page) {
                    log::debug!("artist cache save: {e:#}");
                }
            }

            ctx2.request_repaint();
        }));
        ctx.request_repaint();
    }

    fn select_playlist(&mut self, playlist: PlaylistSummary, ctx: &egui::Context) {
        if self
            .selected_playlist
            .as_ref()
            .is_some_and(|selected| selected.id == playlist.id)
        {
            self.main_view = MainView::Playlist;
            ctx.request_repaint();
            return;
        }

        self.track_sort = None;

        if let Some(task) = self.playlist_task.take() {
            task.abort();
        }

        self.playlist_generation += 1;
        let generation = self.playlist_generation;
        self.selected_playlist = Some(playlist.clone());
        self.main_view = MainView::Playlist;
        self.ensure_playlist_color(&playlist, ctx);
        let cache_only = crate::spotify_api::cache_only_mode();
        let is_liked = crate::spotify_api::is_liked_songs_playlist(&playlist.id);

        let cached_tracks = crate::playlist_cache::PlaylistCache::new()
            .ok()
            .and_then(|cache| cache.load_tracks(&playlist.id).ok().flatten());
        let can_use_cache_without_refresh = cached_tracks.as_ref().is_some_and(|cached| {
            if !cached.complete || cached.tracks.is_empty() {
                return false;
            }
            if cache_only {
                return true;
            }
            if cached.tracks.iter().any(|t| t.artist_id.is_none()) {
                return false;
            }
            if is_liked {
                return crate::playlist_cache::PlaylistCache::cache_is_fresh(cached.fetched_at);
            }
            (cached.snapshot_id.is_some()
                && playlist.snapshot_id.is_some()
                && cached.snapshot_id == playlist.snapshot_id)
                || crate::playlist_cache::PlaylistCache::cache_is_fresh(cached.fetched_at)
        });

        if let Ok(mut state) = self.playlist_state.lock() {
            state.playlist_id = Some(playlist.id.clone());
            state.generation = generation;
            state.tracks = cached_tracks
                .as_ref()
                .map(|cached| cached.tracks.clone())
                .unwrap_or_default();
            state.complete = cached_tracks.as_ref().is_some_and(|cached| cached.complete);
            state.status = if cache_only && state.tracks.is_empty() {
                PlaylistStatus::Error(
                    "Cache-only mode: no cached tracks for this playlist.".to_string(),
                )
            } else if state.tracks.is_empty() {
                PlaylistStatus::Loading
            } else if cache_only || can_use_cache_without_refresh {
                PlaylistStatus::Cached
            } else {
                PlaylistStatus::Refreshing
            };
        }

        if cache_only || can_use_cache_without_refresh {
            ctx.request_repaint();
            return;
        }

        let Some(spotify) = self.spotify.clone() else {
            if let Ok(mut state) = self.playlist_state.lock() {
                if state.generation == generation {
                    state.status = PlaylistStatus::Error(
                        "Sign in to Spotify to load this playlist.".to_string(),
                    );
                }
            }
            ctx.request_repaint();
            return;
        };

        let state = self.playlist_state.clone();
        let ctx = ctx.clone();
        let playlist_id = playlist.id.clone();
        let playlist_for_cache = playlist.clone();

        if is_liked {
            self.playlist_task = Some(self.rt.spawn(async move {
                let mut cache = match crate::playlist_cache::PlaylistCache::new() {
                    Ok(cache) => Some(cache),
                    Err(e) => {
                        log::warn!("Playlist cache unavailable: {}", e);
                        None
                    }
                };

                if let Some(cache) = cache.as_ref() {
                    if let Err(e) = cache.save_playlist(&playlist_for_cache, false) {
                        log::warn!("Failed to save playlist cache metadata: {}", e);
                    }
                }

                let fetch_result = crate::spotify_api::user_saved_tracks(&spotify).await;

                match fetch_result {
                    Ok(tracks) => {
                        if let Some(cache) = cache.as_mut() {
                            if let Err(e) = cache.save_track_batch(&playlist_id, &tracks) {
                                log::warn!("Failed to cache liked tracks: {}", e);
                            }
                            if let Err(e) = cache.finish_refresh(&playlist_for_cache, tracks.len()) {
                                log::warn!("Failed to finalize liked songs cache: {}", e);
                            }
                        }

                        if let Ok(mut lock) = state.lock() {
                            if lock.generation == generation
                                && lock.playlist_id.as_deref() == Some(playlist_id.as_str())
                            {
                                lock.tracks = tracks;
                                lock.status = PlaylistStatus::Loaded;
                                lock.complete = true;
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to fetch liked songs: {:#}", e);
                        let rate_limit_message = crate::spotify_api::rate_limit_status_message(&e);
                        if let Ok(mut lock) = state.lock() {
                            if lock.generation == generation
                                && lock.playlist_id.as_deref() == Some(playlist_id.as_str())
                            {
                                lock.status = if let Some(message) = rate_limit_message {
                                    let prefix = if lock.tracks.is_empty() {
                                        "Spotify rate limited track loading."
                                    } else {
                                        "Using cached tracks."
                                    };
                                    PlaylistStatus::RateLimited(format!("{} {}", prefix, message))
                                } else {
                                    PlaylistStatus::Error(e.to_string())
                                };
                            }
                        }
                    }
                }

                ctx.request_repaint();
            }));
            return;
        }

        self.playlist_task = Some(self.rt.spawn(async move {
            let mut cache = match crate::playlist_cache::PlaylistCache::new() {
                Ok(cache) => Some(cache),
                Err(e) => {
                    log::warn!("Playlist cache unavailable: {}", e);
                    None
                }
            };

            if let Some(cache) = cache.as_ref() {
                if let Err(e) = cache.save_playlist(&playlist_for_cache, false) {
                    log::warn!("Failed to save playlist cache metadata: {}", e);
                }
            }

            let fetch_result =
                crate::spotify_api::playlist_tracks_batched(&spotify, &playlist_id, |batch| {
                    if let Some(cache) = cache.as_mut() {
                        if let Err(e) = cache.save_track_batch(&playlist_id, &batch) {
                            log::warn!("Failed to cache playlist track batch: {}", e);
                        }
                    }

                    if let Ok(mut lock) = state.lock() {
                        if lock.generation == generation
                            && lock.playlist_id.as_deref() == Some(playlist_id.as_str())
                        {
                            if lock.tracks.is_empty() {
                                lock.tracks = batch;
                            } else {
                                for track in batch {
                                    if let Some(existing) = lock
                                        .tracks
                                        .iter_mut()
                                        .find(|existing| existing.position == track.position)
                                    {
                                        *existing = track;
                                    } else {
                                        lock.tracks.push(track);
                                    }
                                }
                                lock.tracks.sort_by_key(|track| track.position);
                            }
                            lock.status = PlaylistStatus::Refreshing;
                            lock.complete = false;
                        }
                    }
                    ctx.request_repaint();
                })
                .await;

            match fetch_result {
                Ok(tracks) => {
                    if let Some(cache) = cache.as_ref() {
                        if let Err(e) = cache.finish_refresh(&playlist_for_cache, tracks.len()) {
                            log::warn!("Failed to finalize playlist cache: {}", e);
                        }
                    }

                    if let Ok(mut lock) = state.lock() {
                        if lock.generation == generation
                            && lock.playlist_id.as_deref() == Some(playlist_id.as_str())
                        {
                            lock.tracks = tracks;
                            lock.status = PlaylistStatus::Loaded;
                            lock.complete = true;
                        }
                    }
                }
                Err(e) => {
                    log::error!("Failed to fetch playlist tracks: {:#}", e);
                    let rate_limit_message = crate::spotify_api::rate_limit_status_message(&e);
                    if let Ok(mut lock) = state.lock() {
                        if lock.generation == generation
                            && lock.playlist_id.as_deref() == Some(playlist_id.as_str())
                        {
                            lock.status = if let Some(message) = rate_limit_message {
                                let prefix = if lock.tracks.is_empty() {
                                    "Spotify rate limited track loading."
                                } else {
                                    "Using cached tracks."
                                };
                                PlaylistStatus::RateLimited(format!("{} {}", prefix, message))
                            } else {
                                PlaylistStatus::Error(e.to_string())
                            };
                        }
                    }
                }
            }

            ctx.request_repaint();
        }));
    }

    fn ensure_playlist_color(&mut self, playlist: &PlaylistSummary, ctx: &egui::Context) {
        let fallback = if crate::spotify_api::is_liked_songs_playlist(&playlist.id) {
            Some([105, 95, 245])
        } else {
            None
        };
        let url = playlist
            .image_url
            .clone()
            .or_else(|| playlist.thumbnail_url.clone());
        self.ensure_color_from_cover_url(playlist.id.clone(), url, fallback, ctx);
    }

    /// Sample a header gradient color from cover art (used for playlists and artist pages).
    fn ensure_color_from_cover_url(
        &mut self,
        cache_key: String,
        url: Option<String>,
        no_image_fallback: Option<[u8; 3]>,
        ctx: &egui::Context,
    ) {
        let Some(url) = url else {
            if let Ok(mut colors) = self.playlist_colors.lock() {
                colors.entry(cache_key).or_insert(no_image_fallback);
            }
            return;
        };

        if let Ok(mut colors) = self.playlist_colors.lock() {
            if colors.contains_key(&cache_key) {
                return;
            }
            colors.insert(cache_key.clone(), None);
        } else {
            return;
        }

        let colors = self.playlist_colors.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let color = fetch_playlist_color(url).await;
            if let Ok(mut colors) = colors.lock() {
                colors.insert(cache_key, color);
            }
            ctx.request_repaint();
        });
    }

    fn refresh_listening_stats(&mut self) {
        let range = self.current_stats_range();
        let limit = self.track_stats_limit.max(self.artist_stats_limit).max(10);
        let stats_result = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to access listening stats database."))
            .and_then(|db_lock| {
                db_lock.listening_stats_for_range(
                    range,
                    limit,
                    self.track_stats_metric,
                    self.artist_stats_metric,
                )
            });

        match stats_result {
            Ok(stats) => {
                self.listening_stats = stats;
                self.sync_selected_stats_date();
                self.stats_status = None;
            }
            Err(e) => {
                self.stats_status = Some(format!("Failed to refresh listening stats: {}", e));
            }
        }
    }

    fn current_stats_range(&self) -> StatsDateRange {
        match self.stats_range_mode {
            StatsRangeMode::AllTime => StatsDateRange::AllTime,
            StatsRangeMode::Year => StatsDateRange::Year(self.selected_stats_year),
            StatsRangeMode::Month => StatsDateRange::Month {
                year: self.selected_stats_year,
                month: self.selected_stats_month,
            },
        }
    }

    fn sync_selected_stats_date(&mut self) {
        if self
            .listening_stats
            .available_years
            .contains(&self.selected_stats_year)
        {
            if self.stats_range_mode == StatsRangeMode::Month
                && !self
                    .listening_stats
                    .available_months
                    .contains(&self.selected_stats_month)
            {
                if let Some(month) = self.listening_stats.available_months.first().copied() {
                    self.selected_stats_month = month;
                }
            }
            return;
        }

        if let Some(year) = self.listening_stats.available_years.first().copied() {
            self.selected_stats_year = year;
            if let Some(month) = self.listening_stats.available_months.first().copied() {
                self.selected_stats_month = month;
            }
        }
    }
}

/// Bottom-bar left strip: 56×56 artwork anchor (matches layout in `OnyxApp::update`).
fn bottom_bar_thumb_rect(left_strip: &egui::Rect) -> egui::Rect {
    egui::Rect::from_center_size(
        egui::pos2(left_strip.min.x + 8.0 + 28.0, left_strip.center().y),
        egui::vec2(56.0, 56.0),
    )
}

impl OnyxApp {
    fn handle_playback_keyboard_shortcuts(
        &mut self,
        ctx: &egui::Context,
        state: &PlaybackState,
        display_position_ms: u32,
    ) {
        if ctx.wants_keyboard_input() {
            return;
        }

        let plain_modifiers = ctx.input(|i| {
            !i.modifiers.alt && !i.modifiers.ctrl && !i.modifiers.command
        });
        if !plain_modifiers {
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            if state.is_playing {
                self.update_position_immediately(display_position_ms, false);
                let _ = self.audio_handle.send(AudioCmd::Pause);
            } else {
                self.update_position_immediately(display_position_ms, true);
                let _ = self.audio_handle.send(AudioCmd::Play);
            }
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            let new_vol = volume_step_slider(state.volume, VOLUME_KEYBOARD_STEP_T);
            if new_vol != state.volume {
                if new_vol > 0 {
                    self.previous_volume = new_vol;
                }
                self.set_volume_immediately(new_vol, true);
            }
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            let new_vol = volume_step_slider(state.volume, -VOLUME_KEYBOARD_STEP_T);
            if new_vol != state.volume {
                if new_vol == 0 {
                    self.previous_volume = state.volume;
                } else {
                    self.previous_volume = new_vol;
                }
                self.set_volume_immediately(new_vol, true);
            }
            return;
        }

        let left_pressed = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        if consume_arrow_double_tap(&mut self.arrow_left_last_tap, left_pressed) {
            self.play_previous();
            return;
        }

        let right_pressed = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        if consume_arrow_double_tap(&mut self.arrow_right_last_tap, right_pressed) {
            self.play_next();
        }
    }
}

impl eframe::App for OnyxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_spotify_connect_result(ctx);
        if self.spotify_login_busy {
            ctx.request_repaint();
        }
        let mut state = self.playback_state.lock().unwrap().clone();
        self.advance_queue_after_track_end(&state);
        self.flush_pending_queue_load();
        state = self.playback_state.lock().unwrap().clone();
        let display_position_ms = display_position_ms(&state);

        self.handle_playback_keyboard_shortcuts(ctx, &state, display_position_ms);

        if state.is_playing {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        if self
            .stats_refresh_due_at
            .is_some_and(|refresh_at| Instant::now() >= refresh_at)
        {
            self.stats_refresh_due_at = None;
            self.refresh_listening_stats();
        } else if self.stats_refresh_due_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // BOTTOM BAR (#181818)
        let mut bottom_frame = egui::Frame::default();
        bottom_frame.fill = egui::Color32::from_rgb(24, 24, 24);
        bottom_frame.inner_margin = egui::Margin::same(8);

        egui::TopBottomPanel::bottom("bottom_bar")
            .exact_height(80.0)
            .frame(bottom_frame)
            .show(ctx, |ui| {
                let available = ui.available_rect_before_wrap();
                let w = available.width();
                let w_left = (w * 0.3).round();
                let w_center = (w * 0.4).round();
                let w_right = w - w_left - w_center;

                let left_rect = egui::Rect::from_min_size(
                    available.min,
                    egui::vec2(w_left, available.height()),
                );
                let center_rect = egui::Rect::from_min_size(
                    left_rect.right_top(),
                    egui::vec2(w_center, available.height()),
                );
                let right_rect = egui::Rect::from_min_size(
                    center_rect.right_top(),
                    egui::vec2(w_right, available.height()),
                );

                // Left Section (track art + title / artist)
                ui.allocate_ui_at_rect(left_rect, |ui| {
                    let thumb = bottom_bar_thumb_rect(&left_rect);
                    let text_left = thumb.right() + 12.0;

                    ui.allocate_ui_at_rect(thumb, |ui| {
                        if let Some(url) = &state.artwork_url {
                            ui.add(
                                egui::Image::new(url)
                                    .corner_radius(4_u8)
                                    .fit_to_exact_size(thumb.size()),
                            );
                        } else {
                            let (rect, _) =
                                ui.allocate_exact_size(thumb.size(), egui::Sense::hover());
                            ui.painter().rect_filled(
                                rect,
                                4.0,
                                egui::Color32::from_rgb(40, 40, 40),
                            );
                        }
                    });

                    let text_rect = egui::Rect::from_min_max(
                        egui::pos2(text_left, left_rect.top()),
                        egui::pos2(left_rect.right() - 8.0, left_rect.bottom()),
                    );
                    if text_rect.width() >= 8.0 && text_rect.height() >= 8.0 {
                        ui.allocate_ui_at_rect(text_rect, |ui| {
                            ui.with_layout(
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.add_space(10.0);
                                    if state.track_name.is_empty() {
                                        ui.label(
                                            egui::RichText::new("No track playing")
                                                .color(egui::Color32::WHITE)
                                                .strong(),
                                        );
                                    } else {
                                        ui.label(
                                            egui::RichText::new(&state.track_name)
                                                .color(egui::Color32::WHITE)
                                                .strong(),
                                        );
                                        let artist_h = 16.0_f32;
                                        let artist_top = ui.min_rect().bottom() + 2.0;
                                        let artist_rect = egui::Rect::from_min_max(
                                            egui::pos2(text_rect.left(), artist_top),
                                            egui::pos2(text_rect.right(), artist_top + artist_h),
                                        );
                                        if let Some(aid) = state.artist_id.as_deref() {
                                            let np_artist_id =
                                                ui.id().with(("np_artist", aid, &state.artist_name));
                                            let artist_interact = ui.interact(
                                                artist_rect,
                                                np_artist_id,
                                                egui::Sense::click(),
                                            );
                                            let muted = egui::Color32::from_rgb(179, 179, 179);
                                            let hover_c = egui::Color32::from_rgb(230, 230, 230);
                                            let c = if artist_interact.hovered() {
                                                hover_c
                                            } else {
                                                muted
                                            };
                                            if artist_interact.hovered() {
                                                ui.ctx()
                                                    .set_cursor_icon(egui::CursorIcon::PointingHand);
                                            }
                                            let text = elide_to_width(
                                                &state.artist_name,
                                                artist_rect.width(),
                                                12.0,
                                            );
                                            let font_id = egui::FontId::proportional(12.0);
                                            let galley = ui.painter().layout_no_wrap(
                                                text,
                                                font_id,
                                                c,
                                            );
                                            let pos = egui::pos2(
                                                artist_rect.left(),
                                                artist_rect.center().y - 12.0 * 0.55,
                                            );
                                            ui.painter().galley(pos, galley, c);
                                            if artist_interact.clicked() {
                                                self.open_artist_page(
                                                    aid.to_string(),
                                                    state.artist_name.clone(),
                                                    ctx,
                                                );
                                            }
                                        } else {
                                            ui.allocate_ui_at_rect(artist_rect, |ui| {
                                                ui.set_width(artist_rect.width());
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&state.artist_name)
                                                            .color(egui::Color32::from_rgb(
                                                                179, 179, 179,
                                                            ))
                                                            .size(12.0),
                                                    )
                                                    .truncate()
                                                    .selectable(false),
                                                );
                                            });
                                        }
                                    }
                                },
                            );
                        });
                    }
                });

                // Center Section
                ui.allocate_ui_at_rect(center_rect, |ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        // Nudge transport controls slightly lower for visual centering.
                        ui.add_space(4.0);
                        // Controls Row
                        ui.allocate_ui_with_layout(
                            egui::vec2(center_rect.width(), 30.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let spacing = 13.0;
                                let btn_w = 24.0;
                                let play_w = 30.0;
                                let total_w = 4.0 * btn_w + play_w + 4.0 * spacing;

                                // Push to center
                                let center_space =
                                    ((center_rect.width() - total_w) / 2.0).max(0.0).floor();
                                ui.add_space(center_space);
                                ui.spacing_mut().item_spacing.x = spacing;

                                let shuffle_color = if self.shuffle {
                                    ACCENT_GREEN
                                } else {
                                    egui::Color32::from_rgb(179, 179, 179)
                                };
                                if ui
                                    .add_sized(
                                        [btn_w, btn_w],
                                        egui::Button::new(
                                            egui::RichText::new("🔀")
                                                .size(14.0)
                                                .color(shuffle_color),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .clicked()
                                {
                                    self.toggle_shuffle();
                                }
                                let skip_color = egui::Color32::from_rgb(179, 179, 179);
                                if track_skip_button(ui, btn_w, false, skip_color).clicked() {
                                    self.play_previous();
                                }

                                if play_pause_button(
                                    ui,
                                    play_w,
                                    state.is_playing,
                                    egui::Color32::WHITE,
                                    egui::Color32::BLACK,
                                )
                                .clicked()
                                {
                                    if state.is_playing {
                                        self.update_position_immediately(
                                            display_position_ms,
                                            false,
                                        );
                                        let _ = self.audio_handle.send(AudioCmd::Pause);
                                    } else {
                                        self.update_position_immediately(display_position_ms, true);
                                        let _ = self.audio_handle.send(AudioCmd::Play);
                                    }
                                }

                                if track_skip_button(ui, btn_w, true, skip_color).clicked() {
                                    self.play_next();
                                }
                                let repeat_color = if self.repeat {
                                    ACCENT_GREEN
                                } else {
                                    egui::Color32::from_rgb(179, 179, 179)
                                };
                                if ui
                                    .add_sized(
                                        [btn_w, btn_w],
                                        egui::Button::new(
                                            egui::RichText::new("🔁")
                                                .size(14.0)
                                                .color(repeat_color),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .clicked()
                                {
                                    self.repeat = !self.repeat;
                                }
                            },
                        );

                        ui.add_space(0.0);
                        // Progress Row
                        ui.allocate_ui_with_layout(
                            egui::vec2(center_rect.width(), 18.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let mins = display_position_ms / 60000;
                                let secs = (display_position_ms / 1000) % 60;
                                let time_text = format!("{}:{:02}", mins, secs);

                                let time_w = 30.0;
                                let pb_width =
                                    (center_rect.width() - (time_w * 2.0) - 32.0).max(10.0);

                                let center_space = ((center_rect.width()
                                    - (pb_width + time_w * 2.0 + 16.0))
                                    / 2.0)
                                    .max(0.0)
                                    .floor();
                                ui.add_space(center_space);

                                ui.add_sized(
                                    [time_w, 12.0],
                                    egui::Label::new(
                                        egui::RichText::new(time_text)
                                            .size(11.0)
                                            .color(egui::Color32::from_rgb(179, 179, 179)),
                                    ),
                                );

                                ui.spacing_mut().item_spacing.x = 8.0;

                                let interact_h = 14.0;
                                let bar_h = 4.0;
                                let (track_interact_rect, seek_resp) = ui.allocate_exact_size(
                                    egui::vec2(pb_width, interact_h),
                                    egui::Sense::click_and_drag(),
                                );
                                let seek_resp =
                                    seek_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                                let bar_rect = egui::Rect::from_min_max(
                                    egui::pos2(
                                        track_interact_rect.left(),
                                        track_interact_rect.center().y - bar_h * 0.5,
                                    ),
                                    egui::pos2(
                                        track_interact_rect.right(),
                                        track_interact_rect.center().y + bar_h * 0.5,
                                    ),
                                );
                                let bar_w = bar_rect.width();

                                if seek_resp.clicked() || seek_resp.dragged() {
                                    if let Some(pos) = seek_resp.interact_pointer_pos() {
                                        let x = (pos.x - bar_rect.left()).clamp(0.0, bar_w);
                                        let pct = x / bar_w;
                                        let duration = state.duration_ms.max(1) as f32;
                                        let new_pos = (pct * duration) as u32;
                                        self.update_position_immediately(new_pos, state.is_playing);
                                        let _ = self.audio_handle.send(AudioCmd::Seek {
                                            position_ms: new_pos,
                                        });
                                    }
                                }

                                let pct = if state.duration_ms > 0 {
                                    (display_position_ms as f32 / state.duration_ms as f32)
                                        .clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                let play_x = bar_rect.left() + bar_w * pct;
                                let p = ui.painter();
                                let track_color = if seek_resp.hovered() {
                                    lighten(egui::Color32::from_rgb(83, 83, 83), 22)
                                } else {
                                    egui::Color32::from_rgb(83, 83, 83)
                                };
                                p.rect_filled(bar_rect, 2.0, track_color);

                                if seek_resp.hovered() {
                                    if let Some(hover_pos) = seek_resp.hover_pos() {
                                        let hx =
                                            (hover_pos.x - bar_rect.left()).clamp(0.0, bar_w);
                                        if hx > bar_w * pct {
                                            let hover_x_abs = bar_rect.left() + hx;
                                            let preview_r = egui::Rect::from_min_max(
                                                egui::pos2(play_x, bar_rect.top()),
                                                egui::pos2(hover_x_abs, bar_rect.bottom()),
                                            );
                                            p.rect_filled(
                                                preview_r,
                                                2.0,
                                                egui::Color32::from_rgba_unmultiplied(
                                                    200, 200, 200, 72,
                                                ),
                                            );
                                        }
                                    }
                                }

                                let fill_color = if seek_resp.hovered() {
                                    lighten(ACCENT_GREEN, 18)
                                } else {
                                    ACCENT_GREEN
                                };
                                if pct > 0.0 {
                                    let mut fr = bar_rect;
                                    fr.set_right(play_x.max(bar_rect.left() + 1.0));
                                    p.rect_filled(fr, 2.0, fill_color);
                                }

                                if state.duration_ms > 0 {
                                    p.circle_filled(
                                        egui::pos2(play_x, bar_rect.center().y),
                                        5.0,
                                        egui::Color32::WHITE,
                                    );
                                }

                                if seek_resp.hovered() && state.duration_ms > 0 {
                                    if let Some(hover_pos) = seek_resp.hover_pos() {
                                        let hx =
                                            (hover_pos.x - bar_rect.left()).clamp(0.0, bar_w);
                                        let preview_ms = ((hx / bar_w) * state.duration_ms as f32)
                                            as u32;
                                        let _ = seek_resp.on_hover_text(format_duration(
                                            preview_ms.min(state.duration_ms),
                                        ));
                                    }
                                }

                                let remaining = state
                                    .duration_ms
                                    .saturating_sub(display_position_ms.min(state.duration_ms));
                                ui.add_sized(
                                    [time_w, 12.0],
                                    egui::Label::new(
                                        egui::RichText::new(format!(
                                            "-{}",
                                            format_duration(remaining)
                                        ))
                                        .size(11.0)
                                        .color(egui::Color32::from_rgb(179, 179, 179)),
                                    ),
                                );
                            },
                        );
                    });
                });

                // Right Section
                ui.allocate_ui_at_rect(right_rect, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(24.0);

                        let btn_w = 24.0;

                        // Fullscreen Icon
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_rgb(179, 179, 179)
                        };
                        let p = ui.painter();
                        let m = rect.center() - egui::vec2(6.0, 6.0);
                        let s = 12.0;
                        let stroke = (1.5, color);
                        p.line_segment([m, m + egui::vec2(4.0, 0.0)], stroke);
                        p.line_segment([m, m + egui::vec2(0.0, 4.0)], stroke);
                        p.line_segment(
                            [m + egui::vec2(s - 4.0, 0.0), m + egui::vec2(s, 0.0)],
                            stroke,
                        );
                        p.line_segment([m + egui::vec2(s, 0.0), m + egui::vec2(s, 4.0)], stroke);
                        p.line_segment(
                            [m + egui::vec2(0.0, s - 4.0), m + egui::vec2(0.0, s)],
                            stroke,
                        );
                        p.line_segment([m + egui::vec2(0.0, s), m + egui::vec2(4.0, s)], stroke);
                        p.line_segment([m + egui::vec2(s - 4.0, s), m + egui::vec2(s, s)], stroke);
                        p.line_segment([m + egui::vec2(s, s - 4.0), m + egui::vec2(s, s)], stroke);

                        let vol_w = 80.0;
                        let interact_h = 14.0;
                        let bar_h = 4.0;
                        let (vol_track_interact, vol_resp) = ui.allocate_exact_size(
                            egui::vec2(vol_w, interact_h),
                            egui::Sense::click_and_drag(),
                        );
                        let vol_resp =
                            vol_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let vol_bar_rect = egui::Rect::from_min_max(
                            egui::pos2(
                                vol_track_interact.left(),
                                vol_track_interact.center().y - bar_h * 0.5,
                            ),
                            egui::pos2(
                                vol_track_interact.right(),
                                vol_track_interact.center().y + bar_h * 0.5,
                            ),
                        );
                        let vol_bar_w = vol_bar_rect.width();

                        if vol_resp.dragged() || vol_resp.clicked() {
                            if let Some(pos) = vol_resp.interact_pointer_pos() {
                                let x = (pos.x - vol_bar_rect.left()).clamp(0.0, vol_bar_w);
                                let t = x / vol_bar_w;
                                let new_vol = volume_slider_t_to_u16(t);
                                if new_vol != state.volume {
                                    state.volume = new_vol;
                                    if new_vol > 0 {
                                        self.previous_volume = new_vol;
                                    }
                                    self.set_volume_immediately(new_vol, true);
                                }
                            }
                        }

                        let vp = ui.painter();
                        let vol_track_color = if vol_resp.hovered() {
                            lighten(egui::Color32::from_rgb(83, 83, 83), 22)
                        } else {
                            egui::Color32::from_rgb(83, 83, 83)
                        };
                        vp.rect_filled(vol_bar_rect, 2.0, vol_track_color);
                        let vol_fill_w = vol_bar_w * volume_u16_to_slider_t(state.volume);
                        let vol_fill_color = if vol_resp.hovered() {
                            lighten(ACCENT_GREEN, 18)
                        } else {
                            ACCENT_GREEN
                        };
                        if vol_fill_w > 0.0 {
                            let mut fr = vol_bar_rect;
                            fr.set_right(vol_bar_rect.left() + vol_fill_w);
                            vp.rect_filled(fr, 2.0, vol_fill_color);
                        }
                        let knob_x = vol_bar_rect.left() + vol_fill_w;
                        vp.circle_filled(
                            egui::pos2(knob_x, vol_bar_rect.center().y),
                            5.0,
                            egui::Color32::WHITE,
                        );

                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if resp.clicked() {
                            if state.volume == 0 {
                                let restore = self.previous_volume.max(1);
                                state.volume = restore;
                                self.set_volume_immediately(restore, true);
                            } else {
                                self.previous_volume = state.volume;
                                state.volume = 0;
                                self.set_volume_immediately(0, true);
                            }
                        }
                        paint_volume_icon(ui, rect, state.volume == 0, resp.hovered());

                        // Device Icon
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_rgb(179, 179, 179)
                        };
                        let c = rect.center();
                        let stroke = (1.5, color);
                        ui.painter().rect_stroke(
                            egui::Rect::from_center_size(
                                c - egui::vec2(0.0, 1.0),
                                egui::vec2(14.0, 10.0),
                            ),
                            1.0,
                            stroke,
                            egui::StrokeKind::Middle,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(-4.0, 7.0), c + egui::vec2(4.0, 7.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(0.0, 4.0), c + egui::vec2(0.0, 7.0)],
                            stroke,
                        );

                        // Queue Icon
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_rgb(179, 179, 179)
                        };
                        let c = rect.center();
                        let stroke = (1.5, color);
                        ui.painter().line_segment(
                            [c + egui::vec2(-6.0, -4.0), c + egui::vec2(6.0, -4.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(-6.0, 0.0), c + egui::vec2(6.0, 0.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(-6.0, 4.0), c + egui::vec2(1.0, 4.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.0, 2.0), c + egui::vec2(3.0, 6.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.0, 2.0), c + egui::vec2(7.0, 4.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.0, 6.0), c + egui::vec2(7.0, 4.0)],
                            stroke,
                        );
                    });
                });
            });

        // SIDEBAR (#000000)
        let mut side_frame = egui::Frame::default();
        side_frame.fill = egui::Color32::from_rgb(0, 0, 0);
        side_frame.inner_margin = egui::Margin {
            left: 16,
            right: 0,
            top: 16,
            bottom: 16,
        };

        let _ = egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(280.0)
            .width_range(200.0..=400.0)
            .frame(side_frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let btn_size = 12.0;

                    let (close_rect, close_resp) = ui
                        .allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(
                        close_rect.center(),
                        btn_size / 2.0,
                        egui::Color32::from_rgb(255, 95, 86),
                    );
                    if close_resp.hovered() {
                        let c = close_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(77, 0, 0));
                        ui.painter().line_segment(
                            [c - egui::vec2(3.0, 3.0), c + egui::vec2(3.0, 3.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.0, -3.0), c - egui::vec2(3.0, -3.0)],
                            stroke,
                        );
                    }
                    if close_resp.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }

                    let (min_rect, min_resp) = ui
                        .allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(
                        min_rect.center(),
                        btn_size / 2.0,
                        egui::Color32::from_rgb(255, 189, 46),
                    );
                    if min_resp.hovered() {
                        let c = min_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(153, 87, 0));
                        ui.painter().line_segment(
                            [c - egui::vec2(3.0, 0.0), c + egui::vec2(3.0, 0.0)],
                            stroke,
                        );
                    }
                    if min_resp.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }

                    let (max_rect, max_resp) = ui
                        .allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(
                        max_rect.center(),
                        btn_size / 2.0,
                        egui::Color32::from_rgb(39, 201, 63),
                    );
                    if max_resp.hovered() {
                        let c = max_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(0, 101, 0));
                        ui.painter().line_segment(
                            [c - egui::vec2(0.0, 3.0), c + egui::vec2(0.0, 3.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c - egui::vec2(3.0, 0.0), c + egui::vec2(3.0, 0.0)],
                            stroke,
                        );
                    }
                    if max_resp.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                    }
                });
                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    let home_resp = icon_button(ui, IconKind::Home, 26.0);
                    if home_resp.clicked() {
                        self.main_view = MainView::Dashboard;
                    }
                    ui.heading(
                        egui::RichText::new("Your Library")
                            .color(egui::Color32::from_rgb(179, 179, 179))
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(16.0);
                        let _ = ui.add(
                            egui::Button::new(egui::RichText::new("→").size(16.0)).frame(false),
                        );
                        let _ = ui.add(
                            egui::Button::new(egui::RichText::new("+").size(16.0)).frame(false),
                        );
                    });
                });

                ui.add_space(12.0);

                // Recents row
                ui.horizontal(|ui| {
                    let _ = ui
                        .add(egui::Button::new(egui::RichText::new("🔍").size(14.0)).frame(false));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(16.0); // Prevent touching the split line
                        ui.label(egui::RichText::new("Recents ☰").size(12.0));
                    });
                });

                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        let playlists = { self.playlists.lock().unwrap().clone() };
                        let playlists = self.ordered_playlists(playlists);
                        let status = self
                            .playlists_status
                            .lock()
                            .map(|status| status.clone())
                            .unwrap_or_else(|_| "Loading playlists...".to_string());
                        if !status.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(status)
                                    .color(egui::Color32::from_rgb(179, 179, 179))
                                    .size(12.0),
                            );
                            ui.add_space(8.0);
                        }
                        for p in playlists {
                            let is_selected = self
                                .selected_playlist
                                .as_ref()
                                .is_some_and(|selected| selected.id == p.id);

                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 54.0),
                                egui::Sense::click(),
                            );
                            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            let paint_rect = egui::Rect::from_min_max(
                                rect.min - egui::vec2(6.0, 0.0),
                                rect.max + egui::vec2(0.0, 0.0),
                            );
                            let bg = match (is_selected, resp.hovered()) {
                                (true, true) => Some(egui::Color32::from_rgb(56, 56, 56)),
                                (true, false) => Some(egui::Color32::from_rgb(40, 40, 40)),
                                (false, true) => Some(egui::Color32::from_rgb(28, 28, 28)),
                                (false, false) => None,
                            };
                            if let Some(bg) = bg {
                                ui.painter().rect_filled(paint_rect, 4.0, bg);
                            }

                            let is_pinned = self.is_playlist_pinned(&p.id);
                            let status_text = self.playlist_download_status_text(&p.id);
                            let right_reserved = if is_pinned { 22.0 } else { 8.0 };

                            resp.context_menu(|ui| {
                                let pin_label = if is_pinned { "Unpin" } else { "Pin" };
                                if ui.button(pin_label).clicked() {
                                    self.toggle_playlist_pin(&p.id);
                                    ui.close();
                                    ctx.request_repaint();
                                }
                                ui.separator();
                                self.render_download_menu(ui, ctx, &p);
                            });

                            if resp.double_clicked() {
                                self.select_playlist(p.clone(), ctx);
                                self.start_playlist_when_ready(&p.id);
                            } else if resp.clicked() {
                                self.select_playlist(p.clone(), ctx);
                            }

                            let img_rect = egui::Rect::from_min_size(
                                rect.min + egui::vec2(0.0, 3.0),
                                egui::vec2(48.0, 48.0),
                            );
                            if crate::spotify_api::is_liked_songs_playlist(&p.id) {
                                paint_liked_songs_playlist_artwork(ui.painter(), img_rect, 4.0);
                            } else if let Some(url) = p.thumbnail_url.as_ref().or(p.image_url.as_ref()) {
                                ui.put(
                                    img_rect,
                                    egui::Image::new(url)
                                        .corner_radius(4_u8)
                                        .fit_to_exact_size(egui::vec2(48.0, 48.0)),
                                );
                            } else {
                                ui.painter().rect_filled(
                                    img_rect,
                                    4.0,
                                    egui::Color32::from_rgb(40, 40, 40),
                                );
                            }

                            let text_left = img_rect.right() + 10.0;
                            let name_color = if is_selected {
                                egui::Color32::from_rgb(30, 215, 96)
                            } else {
                                egui::Color32::WHITE
                            };
                            let name_rect = egui::Rect::from_min_size(
                                egui::pos2(text_left, rect.top() + 10.0),
                                egui::vec2(
                                    (rect.right() - text_left - right_reserved).max(20.0),
                                    18.0,
                                ),
                            );
                            let meta_rect = egui::Rect::from_min_size(
                                egui::pos2(text_left, rect.top() + 29.0),
                                egui::vec2(
                                    (rect.right() - text_left - right_reserved).max(20.0),
                                    16.0,
                                ),
                            );
                            paint_left_text(ui, name_rect, &p.name, name_color, 13.0, true);
                            let meta_text =
                                if crate::spotify_api::is_liked_songs_playlist(&p.id) {
                                    if let Some(status_text) = status_text {
                                        format!(
                                            "Playlist • {} liked songs • {}",
                                            p.track_count, status_text
                                        )
                                    } else {
                                        format!("Playlist • {} liked songs", p.track_count)
                                    }
                                } else if let Some(status_text) = status_text {
                                    format!("Playlist • {} tracks • {}", p.track_count, status_text)
                                } else {
                                    format!("Playlist • {} tracks", p.track_count)
                                };
                            paint_left_text(
                                ui,
                                meta_rect,
                                &meta_text,
                                egui::Color32::from_rgb(179, 179, 179),
                                12.0,
                                false,
                            );
                            if is_pinned {
                                paint_pin_indicator(ui, rect);
                            }
                        }
                    });
            });

        // CENTRAL PANEL (#121212)
        let mut central_frame = egui::Frame::default();
        central_frame.fill = egui::Color32::from_rgb(18, 18, 18);
        // Padding lives inside each view (`CENTRAL_CONTENT_INSET`) so it is not stacked with a
        // frame margin (which produced thick bars at top/sides and dead bands at the bottom).
        central_frame.inner_margin = egui::Margin::ZERO;

        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ctx, |ui| {
                let cache_only = crate::spotify_api::cache_only_mode();
                if !cache_only && self.spotify.is_none() {
                    self.render_spotify_connect_gate(ui, ctx);
                    return;
                }
                if self.main_view == MainView::Playlist {
                    if let Some(playlist) = self.selected_playlist.clone() {
                        self.ensure_playlist_color(&playlist, ctx);
                        let playlist_color = self
                            .playlist_colors
                            .lock()
                            .ok()
                            .and_then(|colors| colors.get(&playlist.id).copied().flatten())
                            .map(playlist_gradient_color);
                        paint_playlist_header_gradient(ui, playlist_color);
                    }
                } else if self.main_view == MainView::Artist {
                    let artist_snapshot = self.artist_state.lock().unwrap().clone();
                    if let Some(ref profile) = artist_snapshot.profile {
                        let key = Self::artist_color_cache_key(&profile.id);
                        self.ensure_color_from_cover_url(
                            key.clone(),
                            profile
                                .image_url
                                .clone()
                                .or_else(|| profile.thumbnail_url.clone()),
                            None,
                            ctx,
                        );
                        let artist_color = self
                            .playlist_colors
                            .lock()
                            .ok()
                            .and_then(|colors| colors.get(&key).copied().flatten())
                            .map(playlist_gradient_color);
                        paint_playlist_header_gradient(ui, artist_color);
                    }
                }

                match self.main_view {
                    MainView::Settings => {
                        self.render_central_header(ui);
                        self.render_settings_view(ui);
                    }
                    MainView::Playlist => {
                        if let Some(playlist) = self.selected_playlist.clone() {
                            self.render_central_header(ui);
                            let playlist_state = self.playlist_state.lock().unwrap().clone();
                            self.maybe_run_pending_autoplay(&playlist_state);
                            self.render_playlist_view(ui, &playlist, &playlist_state, &state, ctx);
                        } else {
                            self.render_dashboard_view(ui);
                        }
                    }
                    MainView::Artist => {
                        self.render_central_header(ui);
                        let artist_snapshot = self.artist_state.lock().unwrap().clone();
                        self.render_artist_view(ui, &artist_snapshot, &state, ctx);
                    }
                    MainView::Dashboard => self.render_dashboard_view(ui),
                }
            });
    }
}

impl OnyxApp {
    fn render_central_header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(CENTRAL_CONTENT_INSET);
        let full_w = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.add_space(CENTRAL_CONTENT_INSET);
            let inner = (full_w - 2.0 * CENTRAL_CONTENT_INSET).max(1.0);
            ui.allocate_ui_with_layout(
                egui::vec2(inner, 0.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if icon_button(ui, IconKind::Settings, 28.0).clicked() {
                        self.main_view = MainView::Settings;
                    }
                },
            );
            ui.add_space(CENTRAL_CONTENT_INSET);
        });
        ui.add_space(8.0);
    }

    fn render_playlist_view(
        &mut self,
        ui: &mut egui::Ui,
        playlist: &PlaylistSummary,
        playlist_state: &PlaylistLoadState,
        playback_state: &PlaybackState,
        ctx: &egui::Context,
    ) {
        self.ensure_playlist_color(playlist, ui.ctx());
        let display_tracks =
            ordered_tracks_for_view(&playlist_state.tracks, self.track_sort);
        let total_duration_ms: u64 = playlist_state
            .tracks
            .iter()
            .map(|track| track.duration_ms as u64)
            .sum();

        let full_w = ui.available_width();
        let full_h = ui.available_height();
        ui.allocate_ui_with_layout(
            egui::vec2(full_w, full_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.horizontal_top(|ui| {
                    ui.add_space(CENTRAL_CONTENT_INSET);
                    let inner_w = (full_w - 2.0 * CENTRAL_CONTENT_INSET).max(1.0);
                    let row_h = ui.available_height();
                    ui.allocate_ui_with_layout(
                        egui::vec2(inner_w, row_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(inner_w);

        ui.horizontal(|ui| {
            if crate::spotify_api::is_liked_songs_playlist(&playlist.id) {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(160.0, 160.0), egui::Sense::hover());
                paint_liked_songs_playlist_artwork(ui.painter(), rect, 8.0);
            } else if let Some(url) = &playlist.image_url {
                ui.add(
                    egui::Image::new(url)
                        .corner_radius(8_u8)
                        .fit_to_exact_size(egui::vec2(160.0, 160.0)),
                );
            } else {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(160.0, 160.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 8.0, egui::Color32::from_rgb(40, 40, 40));
            }

            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new(&playlist.public_label)
                        .color(egui::Color32::WHITE)
                        .size(12.0),
                );
                ui.label(
                    egui::RichText::new(&playlist.name)
                        .color(egui::Color32::WHITE)
                        .size(48.0)
                        .strong(),
                );

                let owner = playlist.owner_name.as_deref().unwrap_or("Unknown owner");
                let duration = if total_duration_ms > 0 {
                    format!(" • {}", format_total_duration(total_duration_ms))
                } else {
                    String::new()
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{} • {} songs{}",
                        owner, playlist.track_count, duration
                    ))
                    .color(egui::Color32::from_rgb(179, 179, 179))
                    .size(13.0),
                );

                let status = playlist_status_text(playlist_state, playlist.track_count);
                if !status.is_empty() {
                    ui.label(
                        egui::RichText::new(status)
                            .color(egui::Color32::from_rgb(179, 179, 179))
                            .size(12.0),
                    );
                }
            });
        });

        ui.add_space(24.0);
        let playlist_is_playing = self.playlist_is_current(playlist) && playback_state.is_playing;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if play_pause_button(
                ui,
                48.0,
                playlist_is_playing,
                egui::Color32::from_rgb(30, 215, 96),
                egui::Color32::BLACK,
            )
            .clicked()
            {
                if playlist_is_playing {
                    let pos = display_position_ms(playback_state);
                    self.update_position_immediately(pos, false);
                    let _ = self.audio_handle.send(AudioCmd::Pause);
                } else if self.playlist_is_current(playlist) {
                    let pos = display_position_ms(playback_state);
                    self.update_position_immediately(pos, true);
                    let _ = self.audio_handle.send(AudioCmd::Play);
                } else {
                    self.start_playlist(playlist.id.clone(), display_tracks.clone());
                }
            }

            let shuffle_color = if self.shuffle {
                ACCENT_GREEN
            } else {
                egui::Color32::from_rgb(179, 179, 179)
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("🔀").size(20.0).color(shuffle_color))
                        .frame(false),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                self.toggle_shuffle();
            }
            let _ = ui.add(
                egui::Button::new(
                    egui::RichText::new("•••")
                        .size(20.0)
                        .color(egui::Color32::from_rgb(179, 179, 179)),
                )
                .frame(false),
            );
        });

        ui.add_space(18.0);
        self.render_track_table_header(ui, &playlist.id);

        if display_tracks.is_empty() {
            ui.add_space(16.0);
            let text = match &playlist_state.status {
                PlaylistStatus::Error(err) => format!("Could not load tracks: {}", err),
                PlaylistStatus::RateLimited(err) => err.clone(),
                _ => "Loading tracks...".to_string(),
            };
            ui.label(egui::RichText::new(text).color(egui::Color32::from_rgb(179, 179, 179)));
            return;
        }

        ui.separator();
        let row_height = 48.0;
        let scroll_h = ui.available_height().max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), scroll_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("playlist_tracks_scroll")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, display_tracks.len(), |ui, row_range| {
                        for row in row_range {
                            let track = &display_tracks[row];
                            self.render_track_row(
                                ui,
                                playlist,
                                &display_tracks,
                                row,
                                track,
                                playback_state,
                                row_height,
                                ctx,
                            );
                        }
                    });
            },
        );
                        },
                    );
                    ui.add_space(CENTRAL_CONTENT_INSET);
                });
            },
        );
    }

    fn format_audience_line(listener_display: Option<&str>, spotify_followers: u32) -> String {
        if let Some(s) = listener_display {
            if !s.is_empty() {
                return s.to_string();
            }
        }
        if spotify_followers == 0 {
            return "Spotify follower count unavailable".to_string();
        }
        if spotify_followers >= 1_000_000 {
            format!(
                "{:.1}M followers on Spotify",
                spotify_followers as f64 / 1_000_000.0
            )
        } else if spotify_followers >= 10_000 {
            format!(
                "{:.0}K followers on Spotify",
                spotify_followers as f64 / 1000.0
            )
        } else if spotify_followers >= 1_000 {
            format!(
                "{:.1}K followers on Spotify",
                spotify_followers as f64 / 1000.0
            )
        } else {
            format!("{spotify_followers} followers on Spotify")
        }
    }

    fn render_artist_popular_row(
        &mut self,
        ui: &mut egui::Ui,
        artist_spotify_id: &str,
        tracks: &[PlaylistTrack],
        row: usize,
        track: &PlaylistTrack,
        playback_state: &PlaybackState,
        row_height: f32,
        ctx: &egui::Context,
    ) {
        let (rect, row_resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click(),
        );
        if row_resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
        }

        let content_rect = rect.shrink2(egui::vec2(16.0, 4.0));
        let pad = 12.0;
        let dur_w = 58.0;
        let idx_w = 36.0;
        let img = 36.0;
        let title_left = content_rect.left() + idx_w + pad;
        let title_right = content_rect.right() - dur_w - pad;
        let muted = egui::Color32::from_rgb(179, 179, 179);
        let green = egui::Color32::from_rgb(30, 215, 96);
        let is_current = self.track_is_current(track, playback_state);
        let title_color = if is_current { green } else { egui::Color32::WHITE };
        let index_color = if is_current { green } else { muted };

        let index_rect = egui::Rect::from_min_size(
            egui::pos2(content_rect.left(), content_rect.top()),
            egui::vec2(idx_w, content_rect.height()),
        );
        paint_left_text(
            ui,
            index_rect,
            &format!("{}", row + 1),
            index_color,
            14.0,
            false,
        );

        let image_rect = egui::Rect::from_min_size(
            egui::pos2(title_left, content_rect.center().y - img / 2.0),
            egui::vec2(img, img),
        );
        if let Some(url) = track
            .album_thumbnail_url
            .as_ref()
            .or(track.album_image_url.as_ref())
        {
            ui.put(
                image_rect,
                egui::Image::new(url)
                    .corner_radius(4_u8)
                    .fit_to_exact_size(egui::vec2(img, img)),
            );
        } else {
            ui.painter()
                .rect_filled(image_rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
        }

        let text_top = content_rect.top();
        let text_bottom = content_rect.bottom();
        let text_left = image_rect.right() + 10.0;
        let name_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, text_top),
            egui::pos2(title_right, (text_top + text_bottom) / 2.0),
        );
        let artist_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, (text_top + text_bottom) / 2.0),
            egui::pos2(title_right, text_bottom),
        );
        let on_same_artist_page = track
            .artist_id
            .as_deref()
            .is_some_and(|id| id == artist_spotify_id);
        paint_left_text(ui, name_rect, &track.name, title_color, 14.0, true);
        paint_left_text(ui, artist_rect, &track.artist, muted, 12.0, false);

        if let Some(aid) = track.artist_id.as_deref() {
            if !on_same_artist_page {
                let artist_click = ui.interact(
                    artist_rect,
                    ui.id().with(("artist_link_popular", track.spotify_uri.as_str())),
                    egui::Sense::click(),
                );
                if artist_click.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if artist_click.clicked() {
                    self.open_artist_page(aid.to_string(), track.artist.clone(), ctx);
                    return;
                }
            }
        }

        let dur_rect = egui::Rect::from_min_size(
            egui::pos2(content_rect.right() - dur_w, content_rect.top()),
            egui::vec2(dur_w, content_rect.height()),
        );
        paint_right_text(
            ui,
            dur_rect,
            &format_duration(track.duration_ms),
            muted,
            13.0,
        );

        if row_resp.clicked() {
            let qid = artist_queue_playlist_id(artist_spotify_id);
            self.start_playlist_at(qid, tracks.to_vec(), row);
        }
    }

    fn render_artist_view(
        &mut self,
        ui: &mut egui::Ui,
        artist_state: &ArtistLoadState,
        playback_state: &PlaybackState,
        ctx: &egui::Context,
    ) {
        let full_w = ui.available_width();
        let full_h = ui.available_height();
        let Some(aid) = artist_state.artist_id.as_deref() else {
            ui.label(
                egui::RichText::new("No artist selected.")
                    .color(egui::Color32::from_rgb(179, 179, 179)),
            );
            return;
        };

        ui.allocate_ui_with_layout(
            egui::vec2(full_w, full_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.horizontal_top(|ui| {
                    ui.add_space(CENTRAL_CONTENT_INSET);
                    let inner_w = (full_w - 2.0 * CENTRAL_CONTENT_INSET).max(1.0);
                    let row_h = ui.available_height();
                    ui.allocate_ui_with_layout(
                        egui::vec2(inner_w, row_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(inner_w);

                            match &artist_state.status {
                                ArtistPageStatus::Idle | ArtistPageStatus::Loading => {
                                    let title = if artist_state.name_hint.is_empty() {
                                        "Loading artist…".to_string()
                                    } else {
                                        format!("Loading {}…", artist_state.name_hint)
                                    };
                                    ui.label(
                                        egui::RichText::new(title)
                                            .color(egui::Color32::WHITE)
                                            .size(28.0)
                                            .strong(),
                                    );
                                }
                                ArtistPageStatus::Error(msg) => {
                                    ui.label(
                                        egui::RichText::new(msg)
                                            .color(egui::Color32::from_rgb(255, 160, 120))
                                            .size(14.0),
                                    );
                                }
                                ArtistPageStatus::Loaded => {
                                    if let Some(profile) = &artist_state.profile {
                                        let key = Self::artist_color_cache_key(&profile.id);
                                        self.ensure_color_from_cover_url(
                                            key,
                                            profile
                                                .image_url
                                                .clone()
                                                .or_else(|| profile.thumbnail_url.clone()),
                                            None,
                                            ui.ctx(),
                                        );

                                        ui.horizontal(|ui| {
                                            let size = 160.0_f32;
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(size, size),
                                                egui::Layout::top_down(egui::Align::Center),
                                                |ui| {
                                                    ui.set_width(size);
                                                    if let Some(url) = profile
                                                        .image_url
                                                        .as_ref()
                                                        .or(profile.thumbnail_url.as_ref())
                                                    {
                                                        ui.add(
                                                            egui::Image::new(url)
                                                                .maintain_aspect_ratio(true)
                                                                .bg_fill(egui::Color32::from_rgb(
                                                                    24, 24, 24,
                                                                ))
                                                                .corner_radius((size / 2.0) as u8)
                                                                .fit_to_exact_size(egui::vec2(
                                                                    size, size,
                                                                )),
                                                        );
                                                    } else {
                                                        let (r, _) = ui.allocate_exact_size(
                                                            egui::vec2(size, size),
                                                            egui::Sense::hover(),
                                                        );
                                                        ui.painter().rect_filled(
                                                            r,
                                                            size / 2.0,
                                                            egui::Color32::from_rgb(40, 40, 40),
                                                        );
                                                    }
                                                },
                                            );

                                            ui.add_space(24.0);
                                            ui.vertical(|ui| {
                                                ui.add_space(28.0);
                                                ui.label(
                                                    egui::RichText::new("Artist")
                                                        .color(egui::Color32::WHITE)
                                                        .size(12.0),
                                                );
                                                ui.label(
                                                    egui::RichText::new(&profile.name)
                                                        .color(egui::Color32::WHITE)
                                                        .size(44.0)
                                                        .strong(),
                                                );
                                                ui.label(
                                                    egui::RichText::new(
                                                        Self::format_audience_line(
                                                            artist_state
                                                                .listener_display
                                                                .as_deref(),
                                                            profile.followers,
                                                        ),
                                                    )
                                                    .color(egui::Color32::from_rgb(
                                                        179, 179, 179,
                                                    ))
                                                    .size(14.0),
                                                );
                                            });
                                        });

                                        ui.add_space(24.0);
                                        let qid = artist_queue_playlist_id(aid);
                                        let from_this_artist = self
                                            .queue_playlist_id
                                            .as_deref()
                                            == Some(qid.as_str());
                                        let playlist_is_playing =
                                            from_this_artist && playback_state.is_playing;
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 14.0;
                                            if play_pause_button(
                                                ui,
                                                48.0,
                                                playlist_is_playing,
                                                egui::Color32::from_rgb(30, 215, 96),
                                                egui::Color32::BLACK,
                                            )
                                            .clicked()
                                            {
                                                if playlist_is_playing {
                                                    let pos =
                                                        display_position_ms(playback_state);
                                                    self.update_position_immediately(pos, false);
                                                    let _ = self.audio_handle.send(AudioCmd::Pause);
                                                } else if from_this_artist {
                                                    let pos =
                                                        display_position_ms(playback_state);
                                                    self.update_position_immediately(pos, true);
                                                    let _ = self.audio_handle.send(AudioCmd::Play);
                                                } else {
                                                    self.start_playlist(
                                                        qid.clone(),
                                                        artist_state.popular_tracks.clone(),
                                                    );
                                                }
                                            }

                                            let shuffle_color = if self.shuffle {
                                                ACCENT_GREEN
                                            } else {
                                                egui::Color32::from_rgb(179, 179, 179)
                                            };
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new("🔀")
                                                            .size(20.0)
                                                            .color(shuffle_color),
                                                    )
                                                    .frame(false),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .clicked()
                                            {
                                                self.toggle_shuffle();
                                            }
                                            let _ = ui.add(
                                                egui::Button::new(
                                                    egui::RichText::new("•••")
                                                        .size(20.0)
                                                        .color(egui::Color32::from_rgb(
                                                            179, 179, 179,
                                                        )),
                                                )
                                                .frame(false),
                                            );
                                        });

                                        ui.add_space(28.0);
                                        ui.label(
                                            egui::RichText::new("Popular")
                                                .color(egui::Color32::WHITE)
                                                .size(20.0)
                                                .strong(),
                                        );
                                        ui.add_space(12.0);

                                        if artist_state.popular_tracks.is_empty() {
                                            let msg = if self.app_config.lastfm_api_key.trim().is_empty()
                                            {
                                                "No popular tracks from Spotify. Add a Last.fm API key in Settings for Last.fm charts."
                                            } else {
                                                "No popular tracks returned from Last.fm."
                                            };
                                            ui.label(
                                                egui::RichText::new(msg)
                                                    .color(egui::Color32::from_rgb(179, 179, 179))
                                                    .size(13.0),
                                            );
                                        } else {
                                            let visible = (if artist_state.popular_show_all {
                                                10
                                            } else {
                                                5
                                            })
                                            .min(artist_state.popular_tracks.len());
                                            let row_height = 48.0_f32;
                                            let scroll_h = ui.available_height().max(0.0);
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(ui.available_width(), scroll_h),
                                                egui::Layout::top_down(egui::Align::Min),
                                                |ui| {
                                                    egui::ScrollArea::vertical()
                                                        .id_salt("artist_popular_scroll")
                                                        .auto_shrink([false, false])
                                                        .show_rows(
                                                            ui,
                                                            row_height,
                                                            visible,
                                                            |ui, row_range| {
                                                                for row in row_range {
                                                                    let track =
                                                                        &artist_state.popular_tracks
                                                                            [row];
                                                                    self.render_artist_popular_row(
                                                                        ui,
                                                                        aid,
                                                                        &artist_state.popular_tracks,
                                                                        row,
                                                                        track,
                                                                        playback_state,
                                                                        row_height,
                                                                        ctx,
                                                                    );
                                                                }
                                                            },
                                                        );
                                                    if artist_state.popular_tracks.len() > 5 {
                                                        ui.add_space(6.0);
                                                        let label = if artist_state.popular_show_all
                                                        {
                                                            "Show less"
                                                        } else {
                                                            "Show more"
                                                        };
                                                        if ui
                                                            .link(
                                                                egui::RichText::new(label)
                                                                    .color(egui::Color32::from_rgb(
                                                                        179, 179, 179,
                                                                    ))
                                                                    .size(13.0),
                                                            )
                                                            .clicked()
                                                        {
                                                            if let Ok(mut s) =
                                                                self.artist_state.lock()
                                                            {
                                                                s.popular_show_all =
                                                                    !s.popular_show_all;
                                                            }
                                                        }
                                                    }
                                                },
                                            );
                                        }

                                        ui.add_space(32.0);
                                        ui.label(
                                            egui::RichText::new("Discography")
                                                .color(egui::Color32::WHITE)
                                                .size(20.0)
                                                .strong(),
                                        );
                                        ui.add_space(12.0);
                                        if artist_state.albums.is_empty() {
                                            ui.label(
                                                egui::RichText::new(
                                                    "No releases from Spotify for this artist.",
                                                )
                                                .color(egui::Color32::from_rgb(179, 179, 179))
                                                .size(13.0),
                                            );
                                        } else {
                                            egui::ScrollArea::vertical()
                                                .id_salt("artist_albums_scroll")
                                                .max_height(320.0)
                                                .auto_shrink([false, false])
                                                .show(ui, |ui| {
                                                    for album in &artist_state.albums {
                                                        ui.horizontal(|ui| {
                                                            let sz = 56.0_f32;
                                                            if let Some(url) =
                                                                &album.thumbnail_url
                                                            {
                                                                ui.add(
                                                                    egui::Image::new(url)
                                                                        .corner_radius(4_u8)
                                                                        .fit_to_exact_size(
                                                                            egui::vec2(sz, sz),
                                                                        ),
                                                                );
                                                            } else {
                                                                let (r, _) = ui
                                                                    .allocate_exact_size(
                                                                        egui::vec2(sz, sz),
                                                                        egui::Sense::hover(),
                                                                    );
                                                                ui.painter().rect_filled(
                                                                    r,
                                                                    4.0,
                                                                    egui::Color32::from_rgb(
                                                                        40, 40, 40,
                                                                    ),
                                                                );
                                                            }
                                                            ui.add_space(12.0);
                                                            ui.vertical(|ui| {
                                                                ui.label(
                                                                    egui::RichText::new(
                                                                        &album.name,
                                                                    )
                                                                    .color(egui::Color32::WHITE)
                                                                    .size(15.0)
                                                                    .strong(),
                                                                );
                                                                let meta = format!(
                                                                    "{} • {}",
                                                                    album.album_type_label,
                                                                    album.release_year
                                                                );
                                                                ui.label(
                                                                    egui::RichText::new(meta)
                                                                        .color(
                                                                            egui::Color32::from_rgb(
                                                                                179,
                                                                                179,
                                                                                179,
                                                                            ),
                                                                        )
                                                                        .size(12.0),
                                                                );
                                                            });
                                                        });
                                                        ui.add_space(8.0);
                                                    }
                                                });
                                        }
                                    }
                                }
                            }
                        },
                    );
                    ui.add_space(CENTRAL_CONTENT_INSET);
                });
            },
        );
    }

    fn render_track_table_header(&mut self, ui: &mut egui::Ui, playlist_id: &str) {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
        let rect = rect.shrink2(egui::vec2(16.0, 0.0));
        let columns = TrackTableLayout::for_width(rect.width()).rects(rect);
        let color = egui::Color32::from_rgb(179, 179, 179);

        let header_cell = |ui: &mut egui::Ui,
                           this: &mut Self,
                           col_rect: egui::Rect,
                           label: &str,
                           column: TrackSortColumn,
                           salt: &'static str| {
            let id = ui.id().with(("track_sort_hdr", playlist_id, salt));
            let resp = ui.interact(col_rect, id, egui::Sense::click());
            if resp.clicked() {
                this.track_sort = cycle_track_sort(this.track_sort, column);
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let sorted_here = matches!(this.track_sort, Some((c, _)) if c == column);
            let natural_w = header_label_width(ui, label, 13.0);
            let arrow_block = HEADER_SORT_ARROW_GAP + SORT_HEADER_ARROW_SIZE * 0.95;
            let max_label_w = (col_rect.width() - arrow_block).max(0.0);
            let used_for_arrow = natural_w.min(max_label_w);
            // Reserve arrow space from the column's right edge so short labels (e.g. "Title") are not
            // squeezed into `used_for_arrow` px — that triggered elide_to_width → "Ti...".
            let text_rect = if sorted_here {
                col_rect.with_max_x((col_rect.right() - arrow_block).max(col_rect.left()))
            } else {
                col_rect
            };
            paint_left_text(ui, text_rect, label, color, 13.0, false);
            if let Some((c, dir)) = this.track_sort {
                if c == column {
                    let arrow_center_x = (col_rect.left()
                        + used_for_arrow
                        + HEADER_SORT_ARROW_GAP
                        + sort_triangle_half_width())
                    .min(col_rect.right() - 2.0);
                    let arrow_center =
                        egui::pos2(arrow_center_x, sort_header_triangle_center_y(col_rect, 13.0));
                    paint_sort_triangle_arrow(
                        ui.painter(),
                        arrow_center,
                        matches!(dir, TrackSortDirection::Asc),
                        color,
                        SORT_HEADER_ARROW_SIZE,
                    );
                }
            }
        };

        header_cell(ui, self, columns.index, "#", TrackSortColumn::Index, "idx");
        header_cell(ui, self, columns.title, "Title", TrackSortColumn::Title, "title");
        header_cell(ui, self, columns.album, "Album", TrackSortColumn::Album, "album");
        header_cell(
            ui,
            self,
            columns.added,
            "Date added",
            TrackSortColumn::DateAdded,
            "added",
        );
        let duration_sorted = matches!(
            self.track_sort,
            Some((TrackSortColumn::Duration, _))
        );
        let (duration_text_rect, duration_arrow_x) = if duration_sorted {
            let tw = header_label_width(ui, "Time", 13.0);
            let arrow_block = HEADER_SORT_ARROW_GAP + SORT_HEADER_ARROW_SIZE * 0.95;
            let max_tw = (columns.duration.width() - arrow_block).max(0.0);
            let used = tw.min(max_tw);
            let min_x = (columns.duration.right() - used - arrow_block).max(columns.duration.left());
            let arrow_x = columns.duration.right()
                - used
                - HEADER_SORT_ARROW_GAP
                - sort_triangle_half_width();
            (
                columns.duration.with_min_x(min_x),
                Some(arrow_x.max(columns.duration.left() + sort_triangle_half_width())),
            )
        } else {
            (columns.duration, None)
        };
        let id = ui.id().with(("track_sort_hdr", playlist_id, "dur"));
        let resp = ui.interact(columns.duration, id, egui::Sense::click());
        if resp.clicked() {
            self.track_sort = cycle_track_sort(self.track_sort, TrackSortColumn::Duration);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_right_text(ui, duration_text_rect, "Time", color, 13.0);
        if let (Some((TrackSortColumn::Duration, dir)), Some(ax)) =
            (self.track_sort, duration_arrow_x)
        {
            paint_sort_triangle_arrow(
                ui.painter(),
                egui::pos2(ax, sort_header_triangle_center_y(columns.duration, 13.0)),
                matches!(dir, TrackSortDirection::Asc),
                color,
                SORT_HEADER_ARROW_SIZE,
            );
        }
    }

    fn render_track_row(
        &mut self,
        ui: &mut egui::Ui,
        playlist: &PlaylistSummary,
        tracks: &[PlaylistTrack],
        row: usize,
        track: &PlaylistTrack,
        playback_state: &PlaybackState,
        row_height: f32,
        ctx: &egui::Context,
    ) {
        let (rect, row_hover) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::hover(),
        );
        if row_hover.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
        }

        let content_rect = rect.shrink2(egui::vec2(16.0, 4.0));
        let columns = TrackTableLayout::for_width(content_rect.width()).rects(content_rect);
        let is_current = self.track_is_current(track, playback_state);
        let muted = egui::Color32::from_rgb(179, 179, 179);
        let green = egui::Color32::from_rgb(30, 215, 96);
        let title_color = if is_current {
            green
        } else {
            egui::Color32::WHITE
        };
        let index_color = if is_current { green } else { muted };

        paint_left_text(
            ui,
            columns.index,
            &format!("{}", row + 1),
            index_color,
            14.0,
            false,
        );

        let image_size = 36.0;
        let image_rect = egui::Rect::from_min_size(
            egui::pos2(
                columns.title.left(),
                columns.title.center().y - image_size / 2.0,
            ),
            egui::vec2(image_size, image_size),
        );
        if let Some(url) = track
            .album_thumbnail_url
            .as_ref()
            .or(track.album_image_url.as_ref())
        {
            ui.put(
                image_rect,
                egui::Image::new(url)
                    .corner_radius(4_u8)
                    .fit_to_exact_size(egui::vec2(image_size, image_size)),
            );
        } else {
            ui.painter()
                .rect_filled(image_rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
        }

        let title_text_rect = egui::Rect::from_min_max(
            egui::pos2(image_rect.right() + 10.0, columns.title.top()),
            columns.title.right_bottom(),
        );
        let name_rect = egui::Rect::from_min_size(
            title_text_rect.min,
            egui::vec2(title_text_rect.width(), title_text_rect.height() / 2.0),
        );
        let artist_rect = egui::Rect::from_min_max(
            egui::pos2(title_text_rect.left(), title_text_rect.center().y),
            title_text_rect.right_bottom(),
        );
        paint_left_text(ui, name_rect, &track.name, title_color, 14.0, true);
        paint_left_text(ui, columns.album, &track.album, muted, 13.0, false);
        paint_left_text(
            ui,
            columns.added,
            &format_added_at(track.added_at.as_deref()),
            muted,
            13.0,
            false,
        );
        paint_right_text(
            ui,
            columns.duration,
            &format_duration(track.duration_ms),
            muted,
            13.0,
        );

        let artist_line_h = artist_rect.height();
        let artist_galley_w =
            elided_text_width(ui, &track.artist, artist_rect.width(), 12.0).min(artist_rect.width());
        let artist_hit_rect = egui::Rect::from_min_size(
            artist_rect.left_top(),
            egui::vec2(artist_galley_w.max(1.0), artist_line_h),
        );

        let mut play_rect = columns.index;
        play_rect = play_rect.union(image_rect);
        play_rect = play_rect.union(name_rect);
        play_rect = play_rect.union(columns.album);
        play_rect = play_rect.union(columns.added);
        play_rect = play_rect.union(columns.duration);

        let play_click = ui.interact(
            play_rect,
            ui.id().with(("pl_row_play", playlist.id.as_str(), row)),
            egui::Sense::click(),
        );

        if let Some(aid) = track.artist_id.as_deref() {
            let artist_click = ui.interact(
                artist_hit_rect,
                ui.id().with(("pl_row_artist", playlist.id.as_str(), row)),
                egui::Sense::click(),
            );
            let artist_color = if artist_click.hovered() {
                egui::Color32::from_rgb(230, 230, 230)
            } else {
                muted
            };
            paint_left_text(ui, artist_rect, &track.artist, artist_color, 12.0, false);
            if artist_click.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if artist_click.clicked() {
                self.open_artist_page(aid.to_string(), track.artist.clone(), ctx);
                return;
            }
        } else {
            paint_left_text(ui, artist_rect, &track.artist, muted, 12.0, false);
        }
        if play_click.clicked() {
            self.start_playlist_at(playlist.id.clone(), tracks.to_vec(), row);
        }
    }

    fn render_dashboard_view(&mut self, ui: &mut egui::Ui) {
        const PAD: f32 = CENTRAL_CONTENT_INSET;

        self.render_central_header(ui);

        let scroll_h = ui.available_height().max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), scroll_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("dashboard_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let content_width =
                            (ui.available_width() - 2.0 * PAD).max(280.0);
                        ui.add_space(PAD);
                        ui.horizontal_top(|ui| {
                            ui.add_space(PAD);
                            ui.allocate_ui_with_layout(
                                egui::vec2(content_width, 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_width(content_width);
                                    self.render_dashboard_content(ui);
                                },
                            );
                            ui.add_space(PAD);
                        });
                        ui.add_space(PAD);
                    });
            },
        );
    }

    fn render_dashboard_content(&mut self, ui: &mut egui::Ui) {
        self.render_dashboard_header(ui);
        ui.add_space(12.0);

        if let Some(status) = &self.stats_status {
            ui.label(
                egui::RichText::new(status)
                    .color(egui::Color32::from_rgb(255, 180, 120))
                    .size(12.0),
            );
            ui.add_space(12.0);
        }

        if self.listening_stats.total_plays == 0 {
            const EMPTY_CARD_PAD: i8 = 18;
            let outer = ui.available_width().min(760.0);
            let inner_w = (outer - 2.0 * f32::from(EMPTY_CARD_PAD)).max(1.0);
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(31, 31, 31))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::same(EMPTY_CARD_PAD))
                .show(ui, |ui| {
                    ui.set_width(inner_w);
                    ui.heading(
                        egui::RichText::new("No listening history yet")
                            .color(egui::Color32::WHITE)
                            .size(18.0),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "Import your Spotify data export from Settings, or listen in Onyx to start building stats.",
                        )
                        .color(egui::Color32::from_rgb(179, 179, 179))
                        .size(13.0),
                    );
                    ui.add_space(12.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Open Settings")
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(ACCENT_GREEN),
                        )
                        .clicked()
                    {
                        self.main_view = MainView::Settings;
                    }
                });
            return;
        }

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, STATS_GRID_GAP);
            self.render_summary_cards(ui);
            if ui.available_width() < 760.0 {
                self.render_ranked_card(ui, "Top Tracks", RankingKind::Tracks);
                self.render_ranked_card(ui, "Top Artists", RankingKind::Artists);
            } else {
                let available = ui.available_width();
                let col_width = (available - STATS_GRID_GAP) / 2.0;
                let col_layout = egui::Layout::top_down(egui::Align::Min);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.allocate_ui_with_layout(egui::vec2(col_width, 0.0), col_layout, |ui| {
                        ui.set_width(col_width);
                        self.render_ranked_card(ui, "Top Tracks", RankingKind::Tracks);
                    });
                    ui.add_space(STATS_GRID_GAP);
                    ui.allocate_ui_with_layout(egui::vec2(col_width, 0.0), col_layout, |ui| {
                        ui.set_width(col_width);
                        self.render_ranked_card(ui, "Top Artists", RankingKind::Artists);
                    });
                });
            }
        });

        if !self.listening_stats.top_albums.is_empty() {
            ui.add_space(STATS_GRID_GAP);
            let width = if ui.available_width() < 760.0 {
                ui.available_width()
            } else {
                (ui.available_width() - STATS_GRID_GAP) / 2.0
            };
            ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                render_bar_rankings(
                    ui,
                    "Top Albums",
                    &self.listening_stats.top_albums,
                    StatsMetric::Plays,
                    self.listening_stats.top_albums.len() as u32,
                    false,
                );
            });
        }
    }

    fn render_dashboard_header(&mut self, ui: &mut egui::Ui) {
        ui.heading(
            egui::RichText::new("Listening Stats")
                .color(egui::Color32::WHITE)
                .size(26.0)
                .strong(),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Your imported Spotify history and Onyx listening activity combined.",
            )
            .color(egui::Color32::from_rgb(179, 179, 179))
            .size(13.0),
        );
        ui.add_space(12.0);
        self.render_stats_range_controls(ui);
    }

    fn render_summary_cards(&self, ui: &mut egui::Ui) {
        let available = ui.available_width();
        if available < 560.0 {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = STATS_GRID_GAP;
                summary_card(
                    ui,
                    "Time listened",
                    &format_total_duration(self.listening_stats.total_listening_time_ms),
                    available,
                );
                summary_card(
                    ui,
                    "Tracks played",
                    &self.listening_stats.total_plays.to_string(),
                    available,
                );
            });
        } else {
            let gap = STATS_GRID_GAP;
            let card_width = (available - gap) / 2.0;
            ui.horizontal_top(|ui| {
                summary_card(
                    ui,
                    "Time listened",
                    &format_total_duration(self.listening_stats.total_listening_time_ms),
                    card_width,
                );
                ui.add_space(gap);
                summary_card(
                    ui,
                    "Tracks played",
                    &self.listening_stats.total_plays.to_string(),
                    card_width,
                );
            });
        }
    }

    fn render_stats_range_controls(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            changed |= range_mode_button(
                ui,
                &mut self.stats_range_mode,
                StatsRangeMode::AllTime,
                "All time",
            );
            changed |=
                range_mode_button(ui, &mut self.stats_range_mode, StatsRangeMode::Year, "Year");
            changed |= range_mode_button(
                ui,
                &mut self.stats_range_mode,
                StatsRangeMode::Month,
                "Month",
            );

            ui.add_space(10.0);
            let years = if self.listening_stats.available_years.is_empty() {
                vec![self.selected_stats_year]
            } else {
                self.listening_stats.available_years.clone()
            };
            egui::ComboBox::from_id_salt("stats_year")
                .selected_text(self.selected_stats_year.to_string())
                .width(92.0)
                .show_ui(ui, |ui| {
                    for year in years {
                        if ui
                            .selectable_value(&mut self.selected_stats_year, year, year.to_string())
                            .changed()
                        {
                            changed = true;
                        }
                    }
                });

            if self.stats_range_mode == StatsRangeMode::Month {
                let months = if self.listening_stats.available_months.is_empty() {
                    vec![self.selected_stats_month]
                } else {
                    self.listening_stats.available_months.clone()
                };
                egui::ComboBox::from_id_salt("stats_month")
                    .selected_text(month_name(self.selected_stats_month))
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for month in months {
                            if ui
                                .selectable_value(
                                    &mut self.selected_stats_month,
                                    month,
                                    month_name(month),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                        }
                    });
            }
        });

        if changed {
            self.refresh_listening_stats();
        }
    }

    fn render_ranked_card(&mut self, ui: &mut egui::Ui, title: &str, kind: RankingKind) {
        let (metric, limit, items) = match kind {
            RankingKind::Tracks => (
                self.track_stats_metric,
                self.track_stats_limit,
                self.listening_stats.top_tracks.clone(),
            ),
            RankingKind::Artists => (
                self.artist_stats_metric,
                self.artist_stats_limit,
                self.listening_stats.top_artists.clone(),
            ),
        };

        let response = render_bar_rankings(ui, title, &items, metric, limit, true);
        let mut changed = false;
        match kind {
            RankingKind::Tracks => {
                if response.metric_changed {
                    self.track_stats_metric = toggle_metric(self.track_stats_metric);
                    changed = true;
                }
                if response.show_more {
                    self.track_stats_limit = next_stats_limit(self.track_stats_limit);
                    changed = true;
                }
                if response.show_less {
                    self.track_stats_limit = 10;
                    changed = true;
                }
            }
            RankingKind::Artists => {
                if response.metric_changed {
                    self.artist_stats_metric = toggle_metric(self.artist_stats_metric);
                    changed = true;
                }
                if response.show_more {
                    self.artist_stats_limit = next_stats_limit(self.artist_stats_limit);
                    changed = true;
                }
                if response.show_less {
                    self.artist_stats_limit = 10;
                    changed = true;
                }
            }
        }

        if changed {
            self.refresh_listening_stats();
        }
    }

    fn import_spotify_history_from_picker(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Spotify history zip", &["zip"])
            .set_title("Import Spotify Listening History")
            .pick_file()
        else {
            return;
        };

        let import_result = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to access listening stats database."))
            .and_then(|db_lock| db_lock.import_spotify_history_zip(&path.display().to_string()));

        match import_result {
            Ok(imported) => {
                self.settings_status = Some(format!(
                    "Imported {} new plays from {}.",
                    imported,
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("Spotify history")
                ));
                self.refresh_listening_stats();
            }
            Err(e) => {
                self.settings_status = Some(format!("Failed to import Spotify history: {}", e));
            }
        }
    }

    fn render_history_import_section(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new("Listening History").color(egui::Color32::WHITE));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Import the ZIP from Spotify's privacy data export to combine older listening history with plays tracked in Onyx.",
            )
            .color(egui::Color32::from_rgb(179, 179, 179))
            .size(13.0),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Import Spotify ZIP").color(egui::Color32::WHITE),
                    )
                    .fill(ACCENT_GREEN),
                )
                .clicked()
            {
                self.import_spotify_history_from_picker();
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} plays, {} listened",
                    self.listening_stats.total_plays,
                    format_total_duration(self.listening_stats.total_listening_time_ms)
                ))
                .color(egui::Color32::from_rgb(179, 179, 179))
                .size(12.0),
            );
        });
    }

    fn render_settings_view(&mut self, ui: &mut egui::Ui) {
        let scroll_h = ui.available_height().max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), scroll_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(CENTRAL_CONTENT_INSET);
                        let full_inner = ui.available_width();
                        ui.horizontal_top(|ui| {
                            ui.add_space(CENTRAL_CONTENT_INSET);
                            let inner_w = (full_inner - 2.0 * CENTRAL_CONTENT_INSET).max(1.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(inner_w, 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_width(inner_w);
            ui.heading(
                egui::RichText::new("Settings")
                    .color(egui::Color32::WHITE)
                    .size(26.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Credentials changes are saved immediately, but Spotify auth and playback sessions may need an app restart.")
                    .color(egui::Color32::from_rgb(179, 179, 179))
                    .size(13.0),
            );
            ui.add_space(20.0);

            self.render_history_import_section(ui);
            ui.add_space(28.0);

            ui.heading(egui::RichText::new("API Keys").color(egui::Color32::WHITE));
            ui.add_space(8.0);
            settings_text_field(
                ui,
                "Spotify Client ID",
                &mut self.config_draft.spotify_client_id,
                false,
            );
            settings_text_field(
                ui,
                "Spotify Client Secret",
                &mut self.config_draft.spotify_client_secret,
                true,
            );
            settings_text_field(ui, "Last.fm API Key", &mut self.config_draft.lastfm_api_key, true);
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Save API Keys").color(egui::Color32::WHITE),
                        )
                        .fill(ACCENT_GREEN),
                    )
                    .clicked()
                {
                    match self.config_draft.save() {
                        Ok(()) => {
                            self.app_config = self.config_draft.clone();
                            self.settings_status = Some("API keys saved to keyring.".to_string());
                        }
                        Err(e) => {
                            self.settings_status = Some(format!("Failed to save API keys: {}", e));
                        }
                    }
                }
                if ui.add(egui::Button::new("Reset Unsaved Changes")).clicked() {
                    self.config_draft = self.app_config.clone();
                    self.settings_status = Some("API key edits reset.".to_string());
                }
            });

            if let Some(status) = &self.settings_status {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(status)
                        .color(egui::Color32::from_rgb(179, 179, 179))
                        .size(12.0),
                );
            }

            ui.add_space(28.0);
            ui.heading(egui::RichText::new("Equalizer").color(egui::Color32::WHITE));
            ui.add_space(8.0);

            let equalizer_changed = self.render_equalizer_card(ui);

            if equalizer_changed {
                self.apply_equalizer_settings();
            }
                                },
                            );
                            ui.add_space(CENTRAL_CONTENT_INSET);
                        });
                        ui.add_space(CENTRAL_CONTENT_INSET);
                    });
            },
        );
    }

    fn apply_equalizer_settings(&mut self) {
        let _ = self
            .audio_handle
            .send(AudioCmd::SetEqualizer(self.user_settings.equalizer.clone()));
        match self.user_settings.save() {
            Ok(()) => self.settings_status = Some("Equalizer settings saved.".to_string()),
            Err(e) => {
                self.settings_status = Some(format!("Failed to save equalizer settings: {}", e))
            }
        }
    }

    fn render_equalizer_card(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let card_width = ui.available_width().min(780.0);
        let card_height = 385.0;
        let (card_rect, _) =
            ui.allocate_exact_size(egui::vec2(card_width, card_height), egui::Sense::hover());
        ui.painter()
            .rect_filled(card_rect, 6.0, egui::Color32::from_rgb(31, 31, 31));

        let top_rect = egui::Rect::from_min_max(
            card_rect.min + egui::vec2(20.0, 16.0),
            egui::pos2(card_rect.right() - 20.0, card_rect.top() + 58.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(top_rect), |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Presets")
                        .color(egui::Color32::from_rgb(179, 179, 179))
                        .size(13.0),
                );
                egui::ComboBox::from_id_salt("equalizer_preset")
                    .selected_text(equalizer_preset_name(&self.user_settings.equalizer))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        changed |= equalizer_preset_option(
                            ui,
                            "Flat",
                            &mut self.user_settings.equalizer,
                            EqualizerSettings::preset_flat(),
                        );
                        changed |= equalizer_preset_option(
                            ui,
                            "Bass Booster",
                            &mut self.user_settings.equalizer,
                            EqualizerSettings::preset_bass_boost(),
                        );
                        changed |= equalizer_preset_option(
                            ui,
                            "Treble Booster",
                            &mut self.user_settings.equalizer,
                            EqualizerSettings::preset_treble_boost(),
                        );
                        changed |= equalizer_preset_option(
                            ui,
                            "Vocal",
                            &mut self.user_settings.equalizer,
                            EqualizerSettings::preset_vocal(),
                        );
                    });
                ui.add_space(20.0);
                changed |= ui
                    .toggle_value(&mut self.user_settings.equalizer.enabled, "Enabled")
                    .changed();
            });
        });

        let graph_rect = egui::Rect::from_min_max(
            card_rect.min + egui::vec2(72.0, 102.0),
            card_rect.max - egui::vec2(72.0, 88.0),
        );
        let label_color = egui::Color32::from_rgb(179, 179, 179);
        ui.painter().text(
            egui::pos2(card_rect.left() + 25.0, graph_rect.top() - 6.0),
            egui::Align2::LEFT_TOP,
            "+12dB",
            egui::FontId::proportional(12.0),
            label_color,
        );
        ui.painter().text(
            egui::pos2(card_rect.left() + 25.0, graph_rect.bottom() - 8.0),
            egui::Align2::LEFT_BOTTOM,
            "-12dB",
            egui::FontId::proportional(12.0),
            label_color,
        );

        let grid = egui::Color32::from_rgb(70, 70, 70);
        for idx in 0..EQ_BANDS.len() {
            let x = band_x(graph_rect, idx);
            ui.painter().line_segment(
                [
                    egui::pos2(x, graph_rect.top()),
                    egui::pos2(x, graph_rect.bottom()),
                ],
                egui::Stroke::new(1.0, grid),
            );
        }
        let zero_y = db_to_graph_y(graph_rect, 0.0);
        ui.painter().line_segment(
            [
                egui::pos2(graph_rect.left(), zero_y),
                egui::pos2(graph_rect.right(), zero_y),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(82, 82, 82)),
        );

        let points: Vec<egui::Pos2> = self
            .user_settings
            .equalizer
            .bands_db
            .iter()
            .enumerate()
            .map(|(idx, gain)| {
                egui::pos2(band_x(graph_rect, idx), db_to_graph_y(graph_rect, *gain))
            })
            .collect();
        for pair in points.windows(2) {
            let left = pair[0];
            let right = pair[1];
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(left.x, graph_rect.bottom()),
                    left,
                    right,
                    egui::pos2(right.x, graph_rect.bottom()),
                ],
                egui::Color32::from_rgba_unmultiplied(30, 215, 96, 70),
                egui::Stroke::NONE,
            ));
        }

        ui.painter().add(egui::Shape::line(
            points.clone(),
            egui::Stroke::new(3.0, egui::Color32::from_rgb(30, 215, 96)),
        ));

        for (idx, point) in points.iter().enumerate() {
            let hit_rect = egui::Rect::from_center_size(*point, egui::vec2(22.0, 22.0));
            let response = ui.interact(
                hit_rect,
                ui.id().with(("eq_band", idx)),
                egui::Sense::drag(),
            );
            if response.dragged() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let clamped_y = pointer.y.clamp(graph_rect.top(), graph_rect.bottom());
                    self.user_settings.equalizer.bands_db[idx] =
                        graph_y_to_db(graph_rect, clamped_y).clamp(-12.0, 12.0);
                    changed = true;
                }
            }
            ui.painter()
                .circle_filled(*point, 4.0, egui::Color32::WHITE);
        }

        for (idx, band) in EQ_BANDS.iter().enumerate() {
            ui.painter().text(
                egui::pos2(band_x(graph_rect, idx), graph_rect.bottom() + 20.0),
                egui::Align2::CENTER_CENTER,
                band.label,
                egui::FontId::proportional(12.0),
                label_color,
            );
        }

        let controls_rect = egui::Rect::from_min_max(
            egui::pos2(card_rect.left() + 24.0, card_rect.bottom() - 48.0),
            egui::pos2(card_rect.right() - 24.0, card_rect.bottom() - 14.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Preamp").color(label_color).size(12.0));
                changed |= ui
                    .add(
                        egui::Slider::new(
                            &mut self.user_settings.equalizer.preamp_db,
                            -12.0..=12.0,
                        )
                        .show_value(true)
                        .suffix(" dB"),
                    )
                    .changed();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked() {
                        self.user_settings.equalizer = EqualizerSettings::preset_flat();
                        changed = true;
                    }
                });
            });
        });

        changed
    }

    fn play_track(&self, track: &PlaylistTrack) {
        if let Ok(mut state) = self.playback_state.lock() {
            state.track_name = track.name.clone();
            state.artist_name = track.artist.clone();
            state.artist_id = track.artist_id.clone();
            state.artwork_url = track.album_image_url.clone();
            state.spotify_uri = Some(track.spotify_uri.clone());
            state.position_ms = 0;
            state.position_anchor_ms = 0;
            state.position_updated_at = Some(Instant::now());
            state.duration_ms = track.duration_ms;
            state.is_playing = true;
        }

        if let Err(e) = self.audio_handle.send(AudioCmd::Load {
            uri: track.spotify_uri.clone(),
            start_playing: true,
            position_ms: 0,
        }) {
            log::error!("Failed to send play command to audio engine: {}", e);
        }
    }

    fn start_playlist_when_ready(&mut self, playlist_id: &str) {
        let state = self.playlist_state.lock().unwrap().clone();
        if state.playlist_id.as_deref() == Some(playlist_id) && !state.tracks.is_empty() {
            self.start_playlist(playlist_id.to_string(), state.tracks);
        } else {
            self.pending_autoplay_playlist_id = Some(playlist_id.to_string());
        }
    }

    fn maybe_run_pending_autoplay(&mut self, playlist_state: &PlaylistLoadState) {
        let Some(pending_id) = self.pending_autoplay_playlist_id.clone() else {
            return;
        };
        if playlist_state.playlist_id.as_deref() == Some(pending_id.as_str())
            && !playlist_state.tracks.is_empty()
        {
            self.pending_autoplay_playlist_id = None;
            self.start_playlist(pending_id, playlist_state.tracks.clone());
        }
    }

    fn ordered_playlists(&self, mut playlists: Vec<PlaylistSummary>) -> Vec<PlaylistSummary> {
        playlists.sort_by(|left, right| {
            compare_playlist_order(left, right, &self.user_settings.playlist_ordering)
        });
        playlists
    }

    fn is_playlist_pinned(&self, playlist_id: &str) -> bool {
        self.user_settings
            .playlist_ordering
            .pinned_playlist_ids
            .iter()
            .any(|id| id == playlist_id)
    }

    fn toggle_playlist_pin(&mut self, playlist_id: &str) {
        let pinned = &mut self.user_settings.playlist_ordering.pinned_playlist_ids;
        if let Some(index) = pinned.iter().position(|id| id == playlist_id) {
            pinned.remove(index);
        } else {
            pinned.insert(0, playlist_id.to_string());
        }
        self.save_user_settings_silent();
    }

    fn mark_playlist_recent(&mut self, playlist_id: &str) {
        let recent = &mut self.user_settings.playlist_ordering.recent_playlist_ids;
        recent.retain(|id| id != playlist_id);
        recent.insert(0, playlist_id.to_string());
        recent.truncate(MAX_RECENT_PLAYLISTS);
        self.save_user_settings_silent();
    }

    fn save_user_settings_silent(&self) {
        if let Err(e) = self.user_settings.save() {
            log::warn!("Failed to save user settings: {}", e);
        }
    }

    fn playlist_download_status_text(&self, playlist_id: &str) -> Option<String> {
        let status = self
            .download_statuses
            .lock()
            .ok()
            .and_then(|statuses| statuses.get(playlist_id).cloned())?;

        match status.state.as_str() {
            DOWNLOAD_DOWNLOADING => Some(format!(
                "Downloading {}/{}",
                status.downloaded_count, status.total_count
            )),
            DOWNLOAD_DOWNLOADED => Some("Downloaded".to_string()),
            crate::downloads::DOWNLOAD_ERROR => status
                .last_error
                .map(|error| format!("Download failed: {}", error))
                .or_else(|| Some("Download failed".to_string())),
            crate::downloads::DOWNLOAD_CANCELLED => Some("Download cancelled".to_string()),
            _ => None,
        }
    }

    fn render_download_menu(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        playlist: &PlaylistSummary,
    ) {
        if crate::spotify_api::is_liked_songs_playlist(&playlist.id) {
            return;
        }
        let status = self
            .download_statuses
            .lock()
            .ok()
            .and_then(|statuses| statuses.get(&playlist.id).cloned());
        let is_downloading = status
            .as_ref()
            .is_some_and(|status| status.state == DOWNLOAD_DOWNLOADING);
        let is_downloaded = status
            .as_ref()
            .is_some_and(|status| status.state == DOWNLOAD_DOWNLOADED);

        if is_downloading {
            if ui.button("Cancel download").clicked() {
                self.cancel_playlist_download(&playlist.id);
                ui.close();
                ctx.request_repaint();
            }
        } else if is_downloaded {
            if ui.button("Remove download").clicked() {
                self.remove_playlist_download(&playlist.id);
                ui.close();
                ctx.request_repaint();
            }
        } else if ui.button("Download playlist").clicked() {
            self.start_playlist_download(playlist.clone(), ctx.clone());
            ui.close();
            ctx.request_repaint();
        }
    }

    fn start_playlist_download(&mut self, playlist: PlaylistSummary, ctx: egui::Context) {
        if self.download_tasks.contains_key(&playlist.id) {
            return;
        }

        let cached_tracks = crate::playlist_cache::PlaylistCache::new()
            .ok()
            .and_then(|cache| cache.load_tracks(&playlist.id).ok().flatten())
            .map(|cached| cached.tracks)
            .unwrap_or_default();
        let Some(spotify) = self.spotify.clone() else {
            return;
        };
        let task = crate::downloads::spawn_playlist_download(
            &self.rt,
            spotify,
            self.audio_handle.clone(),
            self.download_statuses.clone(),
            playlist.clone(),
            cached_tracks,
            ctx,
        );
        self.download_tasks.insert(playlist.id, task);
    }

    fn cancel_playlist_download(&mut self, playlist_id: &str) {
        if let Some(task) = self.download_tasks.remove(playlist_id) {
            task.abort();
        }
        crate::downloads::set_cancelled(&self.download_statuses, playlist_id);
    }

    fn remove_playlist_download(&mut self, playlist_id: &str) {
        if let Some(task) = self.download_tasks.remove(playlist_id) {
            task.abort();
        }
        crate::downloads::remove_download(&self.download_statuses, playlist_id);
    }

    fn toggle_shuffle(&mut self) {
        self.set_shuffle(!self.shuffle);
    }

    fn set_shuffle(&mut self, enabled: bool) {
        if self.shuffle == enabled {
            return;
        }

        self.shuffle = enabled;

        if self.queue.is_empty() {
            return;
        }

        if enabled {
            self.shuffle_queue_after_current_track();
        } else {
            self.restore_queue_order_after_current_track();
        }
    }

    fn shuffle_queue_after_current_track(&mut self) {
        let Some(current_index) = self.queue_index else {
            shuffle_tracks(&mut self.queue);
            return;
        };
        let Some(current_track) = self.queue.get(current_index).cloned() else {
            return;
        };

        let mut upcoming: Vec<_> = self
            .queue
            .iter()
            .enumerate()
            .filter_map(|(index, track)| (index != current_index).then(|| track.clone()))
            .collect();
        shuffle_tracks(&mut upcoming);

        self.queue.clear();
        self.queue.push(current_track);
        self.queue.extend(upcoming);
        self.queue_index = Some(0);
        self.pending_queue_index = None;
    }

    fn restore_queue_order_after_current_track(&mut self) {
        let Some(playlist_id) = self.queue_playlist_id.clone() else {
            return;
        };
        let state = self.playlist_state.lock().unwrap().clone();
        let source_tracks = if state.playlist_id.as_deref() == Some(playlist_id.as_str())
            && !state.tracks.is_empty()
        {
            state.tracks.clone()
        } else if !self.queue_original_tracks.is_empty() {
            self.queue_original_tracks.clone()
        } else {
            return;
        };

        let current_uri = self
            .queue_index
            .and_then(|index| self.queue.get(index))
            .map(|track| track.spotify_uri.clone());

        self.queue = source_tracks;
        self.queue_index = current_uri
            .as_deref()
            .and_then(|uri| self.queue.iter().position(|track| track.spotify_uri == uri))
            .or(Some(0));
        self.pending_queue_index = None;
    }

    fn start_playlist(&mut self, playlist_id: String, tracks: Vec<PlaylistTrack>) {
        if tracks.is_empty() {
            self.pending_autoplay_playlist_id = Some(playlist_id);
            return;
        }

        self.mark_playlist_recent(&playlist_id);
        self.queue_original_tracks = tracks.clone();
        self.queue = tracks;
        self.queue_playlist_id = Some(playlist_id);
        if self.shuffle {
            shuffle_tracks(&mut self.queue);
        }
        self.pending_queue_index = None;
        self.play_queue_index(0);
    }

    fn start_playlist_at(&mut self, playlist_id: String, tracks: Vec<PlaylistTrack>, index: usize) {
        if tracks.is_empty() {
            self.pending_autoplay_playlist_id = Some(playlist_id);
            return;
        }

        let start_index = index.min(tracks.len().saturating_sub(1));
        self.mark_playlist_recent(&playlist_id);
        self.queue_playlist_id = Some(playlist_id);
        self.pending_queue_index = None;
        self.queue_original_tracks = tracks.clone();

        if self.shuffle {
            let current_track = tracks[start_index].clone();
            let mut upcoming: Vec<_> = tracks
                .into_iter()
                .enumerate()
                .filter_map(|(idx, track)| (idx != start_index).then_some(track))
                .collect();
            shuffle_tracks(&mut upcoming);
            self.queue.clear();
            self.queue.push(current_track);
            self.queue.extend(upcoming);
            self.play_queue_index(0);
        } else {
            self.queue = tracks;
            self.play_queue_index(start_index);
        }
    }

    fn play_queue_index(&mut self, index: usize) {
        let now = Instant::now();
        let should_defer = self
            .last_queue_load_at
            .is_some_and(|last| now.duration_since(last).as_millis() < 180);
        if should_defer {
            self.queue_index = Some(index);
            self.pending_queue_index = Some(index);
            return;
        }
        self.load_queue_index_now(index);
    }

    fn load_queue_index_now(&mut self, index: usize) {
        if let Some(track) = self.queue.get(index).cloned() {
            self.queue_index = Some(index);
            self.pending_queue_index = None;
            self.last_queue_load_at = Some(Instant::now());
            self.play_track(&track);
        }
    }

    fn flush_pending_queue_load(&mut self) {
        let Some(index) = self.pending_queue_index else {
            return;
        };
        let ready = self
            .last_queue_load_at
            .map(|last| last.elapsed().as_millis() >= 180)
            .unwrap_or(true);
        if ready {
            self.load_queue_index_now(index);
        }
    }

    fn play_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }

        let next = match self.queue_index {
            Some(index) if index + 1 < self.queue.len() => index + 1,
            Some(_) if self.repeat => 0,
            None => 0,
            _ => return,
        };
        self.play_queue_index(next);
    }

    fn play_previous(&mut self) {
        if self.queue.is_empty() {
            let _ = self.audio_handle.send(AudioCmd::Seek { position_ms: 0 });
            self.update_position_immediately(0, true);
            return;
        }

        let previous = self.queue_index.unwrap_or(0).saturating_sub(1);
        self.play_queue_index(previous);
    }

    fn advance_queue_after_track_end(&mut self, state: &PlaybackState) {
        if state.end_count == self.observed_end_count {
            return;
        }
        self.observed_end_count = state.end_count;
        self.stats_refresh_due_at = Some(Instant::now() + Duration::from_secs(2));
        self.play_next();
    }

    fn playlist_is_current(&self, playlist: &PlaylistSummary) -> bool {
        self.queue_playlist_id.as_deref() == Some(playlist.id.as_str())
            && self.queue_index.is_some()
    }

    fn track_is_current(&self, track: &PlaylistTrack, playback_state: &PlaybackState) -> bool {
        playback_state
            .spotify_uri
            .as_deref()
            .is_some_and(|uri| uri == track.spotify_uri)
            || (playback_state.spotify_uri.is_none() && playback_state.track_name == track.name)
    }

    fn update_position_immediately(&self, position_ms: u32, is_playing: bool) {
        if let Ok(mut shared) = self.playback_state.lock() {
            shared.position_ms = position_ms;
            shared.position_anchor_ms = position_ms;
            shared.position_updated_at = if is_playing {
                Some(Instant::now())
            } else {
                None
            };
            shared.is_playing = is_playing;
        }
    }

    fn set_volume_immediately(&mut self, volume: u16, force_send: bool) {
        if let Ok(mut shared) = self.playback_state.lock() {
            shared.volume = volume;
        }
        if force_send || self.last_sent_volume.abs_diff(volume) > 384 {
            self.last_sent_volume = volume;
            let _ = self
                .audio_handle
                .send(AudioCmd::SetVolume { volume_u16: volume });
        }
    }
}

fn settings_text_field(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool) {
    ui.label(
        egui::RichText::new(label)
            .color(egui::Color32::from_rgb(179, 179, 179))
            .size(12.0),
    );
    ui.add(
        egui::TextEdit::singleline(value)
            .password(password)
            .desired_width((ui.available_width() * 0.7).max(260.0)),
    );
    ui.add_space(8.0);
}

fn equalizer_preset_option(
    ui: &mut egui::Ui,
    label: &str,
    settings: &mut EqualizerSettings,
    preset: EqualizerSettings,
) -> bool {
    if ui.selectable_label(false, label).clicked() {
        *settings = preset;
        true
    } else {
        false
    }
}

fn equalizer_preset_name(settings: &EqualizerSettings) -> &'static str {
    if equalizer_matches(settings, &EqualizerSettings::preset_flat()) {
        "Flat"
    } else if equalizer_matches(settings, &EqualizerSettings::preset_bass_boost()) {
        "Bass Booster"
    } else if equalizer_matches(settings, &EqualizerSettings::preset_treble_boost()) {
        "Treble Booster"
    } else if equalizer_matches(settings, &EqualizerSettings::preset_vocal()) {
        "Vocal"
    } else {
        "Custom"
    }
}

fn equalizer_matches(a: &EqualizerSettings, b: &EqualizerSettings) -> bool {
    a.enabled == b.enabled
        && (a.preamp_db - b.preamp_db).abs() < 0.05
        && a.bands_db
            .iter()
            .zip(b.bands_db.iter())
            .all(|(left, right)| (*left - *right).abs() < 0.05)
}

fn band_x(rect: egui::Rect, idx: usize) -> f32 {
    if EQ_BANDS.len() <= 1 {
        return rect.center().x;
    }
    rect.left() + rect.width() * idx as f32 / (EQ_BANDS.len() - 1) as f32
}

fn db_to_graph_y(rect: egui::Rect, db: f32) -> f32 {
    let t = ((db.clamp(-12.0, 12.0) + 12.0) / 24.0).clamp(0.0, 1.0);
    rect.bottom() - rect.height() * t
}

fn graph_y_to_db(rect: egui::Rect, y: f32) -> f32 {
    let t = ((rect.bottom() - y) / rect.height()).clamp(0.0, 1.0);
    t * 24.0 - 12.0
}

fn play_pause_button(
    ui: &mut egui::Ui,
    size: f32,
    is_playing: bool,
    fill_color: egui::Color32,
    icon_color: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if response.hovered() {
        lighten(fill_color, 18)
    } else {
        fill_color
    };
    ui.painter().circle_filled(rect.center(), size * 0.5, fill);

    if is_playing {
        let bar_w = size * 0.13;
        let bar_h = size * 0.42;
        let gap = size * 0.12;
        let center = rect.center();
        for x_offset in [-(gap + bar_w) / 2.0, (gap + bar_w) / 2.0] {
            let bar = egui::Rect::from_center_size(
                center + egui::vec2(x_offset, 0.0),
                egui::vec2(bar_w, bar_h),
            );
            ui.painter().rect_filled(bar, bar_w * 0.45, icon_color);
        }
    } else {
        let center = rect.center() + egui::vec2(size * 0.035, 0.0);
        let h = size * 0.42;
        let w = size * 0.34;
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                center + egui::vec2(-w * 0.45, -h * 0.5),
                center + egui::vec2(-w * 0.45, h * 0.5),
                center + egui::vec2(w * 0.58, 0.0),
            ],
            icon_color,
            egui::Stroke::NONE,
        ));
    }

    response
}

/// Previous / next track: drawn double chevrons so glyphs do not depend on emoji font coverage.
fn track_skip_button(
    ui: &mut egui::Ui,
    size: f32,
    forward: bool,
    base_color: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if response.hovered() {
        lighten(base_color, 35)
    } else {
        base_color
    };
    let painter = ui.painter();
    let c = rect.center();
    let tri_w = size * 0.36;
    let tri_h = size * 0.4;
    let pair_shift = size * 0.135;
    for dx in [-pair_shift, pair_shift] {
        let cx = c.x + dx;
        let (tip, a, b) = if forward {
            (
                egui::pos2(cx + tri_w * 0.48, c.y),
                egui::pos2(cx - tri_w * 0.42, c.y - tri_h * 0.52),
                egui::pos2(cx - tri_w * 0.42, c.y + tri_h * 0.52),
            )
        } else {
            (
                egui::pos2(cx - tri_w * 0.48, c.y),
                egui::pos2(cx + tri_w * 0.42, c.y - tri_h * 0.52),
                egui::pos2(cx + tri_w * 0.42, c.y + tri_h * 0.52),
            )
        };
        painter.add(egui::Shape::convex_polygon(
            vec![tip, a, b],
            color,
            egui::Stroke::NONE,
        ));
    }
    response
}

fn lighten(color: egui::Color32, amount: u8) -> egui::Color32 {
    egui::Color32::from_rgb(
        color.r().saturating_add(amount),
        color.g().saturating_add(amount),
        color.b().saturating_add(amount),
    )
}

async fn fetch_playlist_color(url: String) -> Option<[u8; 3]> {
    let response = reqwest::get(url).await.ok()?.error_for_status().ok()?;
    let bytes = response.bytes().await.ok()?;
    tokio::task::spawn_blocking(move || dominant_playlist_color(&bytes))
        .await
        .ok()
        .flatten()
}

fn dominant_playlist_color(bytes: &[u8]) -> Option<[u8; 3]> {
    let image = image::load_from_memory(bytes)
        .ok()?
        .thumbnail(96, 96)
        .to_rgb8();
    let mut bins = vec![(0_u32, 0_u32, 0_u32, 0_u32); 512];
    let mut fallback = (0_u32, 0_u32, 0_u32, 0_u32);

    for pixel in image.pixels() {
        let [r, g, b] = pixel.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let saturation = max - min;

        if max > 24 && min < 244 {
            fallback.0 += r as u32;
            fallback.1 += g as u32;
            fallback.2 += b as u32;
            fallback.3 += 1;
        }

        if max < 36 || min > 235 || saturation < 18 {
            continue;
        }

        let brightness_penalty = if max > 225 || max < 52 { 20 } else { 0 };
        let weight = 1 + saturation.saturating_sub(brightness_penalty) as u32;
        let index = ((r as usize / 32) * 64) + ((g as usize / 32) * 8) + (b as usize / 32);
        let bin = &mut bins[index];
        bin.0 += r as u32 * weight;
        bin.1 += g as u32 * weight;
        bin.2 += b as u32 * weight;
        bin.3 += weight;
    }

    let best = bins
        .into_iter()
        .filter(|bin| bin.3 > 0)
        .max_by_key(|bin| bin.3)
        .or_else(|| if fallback.3 > 0 { Some(fallback) } else { None })?;

    Some([
        (best.0 / best.3) as u8,
        (best.1 / best.3) as u8,
        (best.2 / best.3) as u8,
    ])
}

fn playlist_gradient_color(color: [u8; 3]) -> egui::Color32 {
    let [r, g, b] = color;
    let max = r.max(g).max(b);
    let lift = 88_u8.saturating_sub(max);
    egui::Color32::from_rgb(
        r.saturating_add(lift / 2),
        g.saturating_add(lift / 2),
        b.saturating_add(lift / 2),
    )
}

fn paint_playlist_header_gradient(ui: &egui::Ui, color: Option<egui::Color32>) {
    let Some(color) = color else {
        return;
    };

    let base = egui::Color32::from_rgb(18, 18, 18);
    let panel = ui.max_rect();
    let grad_h = 220.0_f32;
    let rect = egui::Rect::from_min_max(
        panel.min,
        egui::pos2(panel.max.x, (panel.min.y + grad_h).min(panel.max.y)),
    );

    let mut vertical = egui::Mesh::default();
    let top_left = lerp_color(color, base, 0.08);
    let top_right = lerp_color(color, base, 0.24);
    vertical.colored_vertex(rect.left_top(), top_left);
    vertical.colored_vertex(rect.right_top(), top_right);
    vertical.colored_vertex(rect.right_bottom(), base);
    vertical.colored_vertex(rect.left_bottom(), base);
    vertical.add_triangle(0, 1, 2);
    vertical.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(vertical));
}

fn lerp_color(from: egui::Color32, to: egui::Color32, t: f32) -> egui::Color32 {
    let mix = |a: u8, b: u8| {
        let value = a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0);
        value.round() as u8
    };
    egui::Color32::from_rgb(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
    )
}

fn paint_volume_icon(ui: &egui::Ui, rect: egui::Rect, muted: bool, hovered: bool) {
    let color = if hovered {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(179, 179, 179)
    };
    let c = rect.center() + egui::vec2(-2.0, 0.0);
    let stroke = egui::Stroke::new(1.7, color);
    let speaker = vec![
        c + egui::vec2(-8.0, -3.5),
        c + egui::vec2(-4.5, -3.5),
        c + egui::vec2(0.5, -7.0),
        c + egui::vec2(0.5, 7.0),
        c + egui::vec2(-4.5, 3.5),
        c + egui::vec2(-8.0, 3.5),
    ];
    ui.painter().add(egui::Shape::closed_line(speaker, stroke));

    if muted {
        let x_center = c + egui::vec2(8.2, 0.0);
        ui.painter().line_segment(
            [
                x_center + egui::vec2(-3.0, -3.0),
                x_center + egui::vec2(3.0, 3.0),
            ],
            stroke,
        );
        ui.painter().line_segment(
            [
                x_center + egui::vec2(3.0, -3.0),
                x_center + egui::vec2(-3.0, 3.0),
            ],
            stroke,
        );
    } else {
        paint_arc(ui, c + egui::vec2(2.5, 0.0), 5.0, -0.75, 0.75, stroke);
        paint_arc(ui, c + egui::vec2(2.5, 0.0), 8.0, -0.65, 0.65, stroke);
    }
}

/// Liked Songs placeholder art: blue–purple diagonal gradient and a white heart.
fn paint_liked_songs_playlist_artwork(painter: &egui::Painter, rect: egui::Rect, _corner_radius: f32) {
    use egui::epaint::{Mesh, Shape, Vertex};
    use egui::{FontId, Pos2, TextureId};

    let tl = egui::Color32::from_rgb(45, 140, 255);
    let tr = egui::Color32::from_rgb(110, 105, 250);
    let bl = egui::Color32::from_rgb(88, 86, 230);
    let br = egui::Color32::from_rgb(180, 95, 245);

    let mut mesh = Mesh::with_texture(TextureId::default());
    let uv = Pos2::ZERO;
    let i0 = mesh.vertices.len() as u32;
    mesh.vertices.push(Vertex {
        pos: rect.left_top(),
        uv,
        color: tl,
    });
    mesh.vertices.push(Vertex {
        pos: rect.right_top(),
        uv,
        color: tr,
    });
    mesh.vertices.push(Vertex {
        pos: rect.right_bottom(),
        uv,
        color: br,
    });
    mesh.vertices.push(Vertex {
        pos: rect.left_bottom(),
        uv,
        color: bl,
    });
    mesh.add_triangle(i0, i0 + 1, i0 + 2);
    mesh.add_triangle(i0, i0 + 2, i0 + 3);
    painter.add(Shape::mesh(mesh));

    let heart_size = rect.height().min(rect.width()) * 0.52;
    let galley = painter.layout_no_wrap(
        "♥".to_string(),
        FontId::proportional(heart_size),
        egui::Color32::WHITE,
    );
    let pos = rect.center() - galley.size() / 2.0;
    painter.galley(pos, galley, egui::Color32::WHITE);
}

fn paint_pin_indicator(ui: &egui::Ui, row_rect: egui::Rect) {
    const PIN_SVG: &[u8] = include_bytes!("../assets/fonts/pin.svg");
    const PIN_URI: &str = "bytes://onyx/pin.svg";
    let bytes = egui::load::Bytes::from(PIN_SVG);
    ui.ctx().include_bytes(PIN_URI, bytes);
    let rect = egui::Rect::from_center_size(
        egui::pos2(row_rect.right() - 12.0, row_rect.center().y),
        egui::vec2(12.0, 12.0),
    );
    if let Ok(texture) = ui.ctx().try_load_texture(
        PIN_URI,
        egui::TextureOptions::LINEAR,
        egui::load::SizeHint::Size {
            width: 24,
            height: 24,
            maintain_aspect_ratio: true,
        },
    ) {
        if let egui::load::TexturePoll::Ready { texture } = texture {
            ui.painter().image(
                texture.id,
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::from_rgb(30, 215, 96),
            );
        }
    }
}

fn paint_arc(
    ui: &egui::Ui,
    center: egui::Pos2,
    radius: f32,
    start_angle: f32,
    end_angle: f32,
    stroke: egui::Stroke,
) {
    let mut points = Vec::new();
    for step in 0..=12 {
        let t = step as f32 / 12.0;
        let angle = start_angle + (end_angle - start_angle) * t;
        points.push(center + egui::vec2(angle.cos() * radius, angle.sin() * radius));
    }
    ui.painter().add(egui::Shape::line(points, stroke));
}

fn icon_button(ui: &mut egui::Ui, kind: IconKind, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hovered = response.hovered();
    let color = if hovered {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(179, 179, 179)
    };

    if hovered {
        ui.painter().circle_filled(
            rect.center(),
            size * 0.48,
            egui::Color32::from_rgb(32, 32, 32),
        );
    }

    match kind {
        IconKind::Home => paint_home_icon(ui, rect, color),
        IconKind::Settings => paint_settings_icon(ui, rect, color),
    }

    response
}

fn paint_home_icon(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.7, color);
    let w = rect.width() * 0.5;
    let h = rect.height() * 0.42;
    let roof_top = c + egui::vec2(0.0, -h * 0.58);
    let left_roof = c + egui::vec2(-w * 0.5, -h * 0.05);
    let right_roof = c + egui::vec2(w * 0.5, -h * 0.05);
    let body_left = c + egui::vec2(-w * 0.38, -h * 0.05);
    let body_right = c + egui::vec2(w * 0.38, -h * 0.05);
    let body_bottom_left = c + egui::vec2(-w * 0.38, h * 0.55);
    let body_bottom_right = c + egui::vec2(w * 0.38, h * 0.55);

    ui.painter().line_segment([left_roof, roof_top], stroke);
    ui.painter().line_segment([roof_top, right_roof], stroke);
    ui.painter()
        .line_segment([body_left, body_bottom_left], stroke);
    ui.painter()
        .line_segment([body_right, body_bottom_right], stroke);
    ui.painter()
        .line_segment([body_bottom_left, body_bottom_right], stroke);
}

fn paint_settings_icon(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.55, color);
    let r = rect.width() * 0.2;
    ui.painter().circle_stroke(c, r, stroke);
    ui.painter().circle_stroke(c, r * 0.42, stroke);

    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let dir = egui::vec2(angle.cos(), angle.sin());
        ui.painter()
            .line_segment([c + dir * (r * 1.18), c + dir * (r * 1.55)], stroke);
    }
}

fn playlist_status_text(state: &PlaylistLoadState, expected_count: u32) -> String {
    match &state.status {
        PlaylistStatus::Idle | PlaylistStatus::Loaded | PlaylistStatus::Cached => String::new(),
        PlaylistStatus::Loading => {
            if state.tracks.is_empty() {
                "Loading tracks...".to_string()
            } else {
                format!("Loaded {} of {}", state.tracks.len(), expected_count)
            }
        }
        PlaylistStatus::Refreshing => {
            if expected_count > 0 {
                format!(
                    "Refreshing... loaded {} of {}",
                    state.tracks.len(),
                    expected_count
                )
            } else {
                format!("Refreshing... loaded {}", state.tracks.len())
            }
        }
        PlaylistStatus::RateLimited(message) => message.clone(),
        PlaylistStatus::Error(err) => format!("Refresh failed: {}", err),
    }
}

/// Proportional stack uses Manrope first for metrics; this family is **only** color/outline emoji
/// fonts so mixed [`LayoutJob`] sections can pick Apple/Segoe/Noto for emoji codepoints.
const EMOJI_FALLBACK_FAMILY: &str = "emoji_fallback";

#[allow(dead_code)]
#[inline]
fn font_emoji(size: f32) -> egui::FontId {
    egui::FontId::new(
        size,
        egui::FontFamily::Name(EMOJI_FALLBACK_FAMILY.into()),
    )
}

#[allow(dead_code)]
fn char_prefers_emoji_font(c: char) -> bool {
    let cp = c as u32;
    if matches!(c, '\u{FE0F}' | '\u{FE0E}' | '\u{200D}') {
        return true;
    }
    if (0x1F300..=0x1FAFF).contains(&cp) {
        return true;
    }
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        return true;
    }
    if (0x1F3FB..=0x1F3FF).contains(&cp) {
        return true;
    }
    if (0x2600..=0x27BF).contains(&cp) {
        return true;
    }
    if (0x2300..=0x23FF).contains(&cp) {
        return true;
    }
    if (0x2190..=0x21FF).contains(&cp) {
        return true;
    }
    if (0x25A0..=0x25FF).contains(&cp) {
        return true;
    }
    if (0x2B00..=0x2BFF).contains(&cp) {
        return true;
    }
    cp == 0x20E3
}

#[allow(dead_code)]
fn split_emoji_runs(s: &str) -> Vec<(bool, String)> {
    let mut runs: Vec<(bool, String)> = Vec::new();
    let mut cur_is_emoji: Option<bool> = None;
    let mut buf = String::new();
    for c in s.chars() {
        // Keep VS16/VS15/ZWJ on the same run as surrounding emoji text (ZWJ sequences).
        if matches!(c, '\u{200d}' | '\u{fe0f}' | '\u{fe0e}') {
            if cur_is_emoji.is_some() {
                buf.push(c);
            }
            continue;
        }
        let is_e = char_prefers_emoji_font(c);
        match cur_is_emoji {
            None => {
                cur_is_emoji = Some(is_e);
                buf.push(c);
            }
            Some(prev) if prev == is_e => buf.push(c),
            Some(_) => {
                runs.push((cur_is_emoji.unwrap_or(false), std::mem::take(&mut buf)));
                cur_is_emoji = Some(is_e);
                buf.push(c);
            }
        }
    }
    if !buf.is_empty() {
        runs.push((cur_is_emoji.unwrap_or(false), buf));
    }
    runs
}

#[allow(dead_code)]
fn mixed_text_format(font_id: egui::FontId, color: egui::Color32, size: f32) -> TextFormat {
    TextFormat {
        font_id,
        color,
        line_height: Some((size * 1.15).ceil()),
        valign: egui::Align::Center,
        ..Default::default()
    }
}

#[allow(dead_code)]
fn paint_left_text_mixed(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
    size: f32,
    strong: bool,
) {
    let text = elide_to_width_mixed(text, rect.width(), size);
    if text.is_empty() {
        return;
    }
    let pos = egui::pos2(rect.left(), rect.center().y - size * 0.55);
    let mut job = LayoutJob::default();
    job.break_on_newline = false;
    for (emoji, chunk) in split_emoji_runs(&text) {
        let mut font_id = if emoji {
            font_emoji(size)
        } else {
            egui::FontId::proportional(size)
        };
        if emoji {
            let probe = ui.painter().layout_no_wrap(
                chunk.clone(),
                font_id.clone(),
                color,
            );
            if !chunk.is_empty() && probe.size().x < 0.01 {
                font_id = egui::FontId::proportional(size);
            }
        }
        job.append(&chunk, 0.0, mixed_text_format(font_id, color, size));
    }
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos, galley.clone(), color);
    if strong {
        ui.painter()
            .galley(pos + egui::vec2(0.35, 0.0), galley, color);
    }
}

fn elided_text_width(ui: &egui::Ui, text: &str, max_width: f32, size: f32) -> f32 {
    let s = elide_to_width(text, max_width, size);
    ui.painter()
        .layout_no_wrap(
            s,
            egui::FontId::proportional(size),
            egui::Color32::PLACEHOLDER,
        )
        .size()
        .x
}

fn paint_left_text(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
    size: f32,
    strong: bool,
) {
    let text = elide_to_width(text, rect.width(), size);
    let font_id = egui::FontId::proportional(size);
    let pos = egui::pos2(rect.left(), rect.center().y - size * 0.55);
    let galley = ui
        .painter()
        .layout_no_wrap(text.clone(), font_id.clone(), color);
    ui.painter().galley(pos, galley, color);
    if strong {
        let strong_galley = ui.painter().layout_no_wrap(text, font_id, color);
        ui.painter()
            .galley(pos + egui::vec2(0.35, 0.0), strong_galley, color);
    }
}

fn paint_right_text(ui: &egui::Ui, rect: egui::Rect, text: &str, color: egui::Color32, size: f32) {
    let text = elide_to_width(text, rect.width(), size);
    let font_id = egui::FontId::proportional(size);
    let galley = ui.painter().layout_no_wrap(text, font_id, color);
    let pos = egui::pos2(
        rect.right() - galley.size().x,
        rect.center().y - size * 0.55,
    );
    ui.painter().galley(pos, galley, color);
}

/// Sort triangle height (width scales with this).
const SORT_HEADER_ARROW_SIZE: f32 = 9.0;
/// Gap between header label text and the sort triangle centerline.
const HEADER_SORT_ARROW_GAP: f32 = 8.0;

#[inline]
fn sort_triangle_half_width() -> f32 {
    SORT_HEADER_ARROW_SIZE * 0.95 * 0.5
}

/// Vertical alignment for sort triangles vs header caps (text uses `center.y - size * 0.55`).
fn sort_header_triangle_center_y(col_rect: egui::Rect, font_size: f32) -> f32 {
    col_rect.center().y + font_size * 0.14
}

fn header_label_width(ui: &egui::Ui, label: &str, size: f32) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::FontId::proportional(size),
        egui::Color32::PLACEHOLDER,
    );
    galley.size().x
}

fn paint_sort_triangle_arrow(
    painter: &egui::Painter,
    center: egui::Pos2,
    ascending: bool,
    color: egui::Color32,
    height: f32,
) {
    let w = height * 0.95;
    let h = height * 0.52;
    if ascending {
        let tip = center + egui::vec2(0.0, -h * 0.5);
        let bl = center + egui::vec2(-w * 0.5, h * 0.5);
        let br = center + egui::vec2(w * 0.5, h * 0.5);
        painter.add(egui::Shape::convex_polygon(
            vec![tip, bl, br],
            color,
            egui::Stroke::NONE,
        ));
    } else {
        let tip = center + egui::vec2(0.0, h * 0.5);
        let tl = center + egui::vec2(-w * 0.5, -h * 0.5);
        let tr = center + egui::vec2(w * 0.5, -h * 0.5);
        painter.add(egui::Shape::convex_polygon(
            vec![tip, tl, tr],
            color,
            egui::Stroke::NONE,
        ));
    }
}

fn elide_to_width(text: &str, width: f32, size: f32) -> String {
    let approx_chars = (width / (size * 0.48)).floor().max(1.0) as usize;
    if text.chars().count() <= approx_chars {
        return text.to_string();
    }
    let take = approx_chars.saturating_sub(1);
    let mut clipped: String = text.chars().take(take).collect();
    clipped.push('…');
    clipped
}

/// Like [`elide_to_width`] but counts emoji codepoints as wider so all-emoji titles are not over-elided.
#[allow(dead_code)]
fn elide_to_width_mixed(text: &str, width: f32, size: f32) -> String {
    let unit = size * 0.48;
    let max_units = (width / unit).floor().max(1.0);
    let mut used = 0.0_f32;
    let mut out = String::new();
    for c in text.chars() {
        let w = if char_prefers_emoji_font(c) { 2.0 } else { 1.0 };
        if used + w > max_units {
            if !out.is_empty() {
                out.push('…');
            }
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

fn cycle_track_sort(
    current: Option<(TrackSortColumn, TrackSortDirection)>,
    column: TrackSortColumn,
) -> Option<(TrackSortColumn, TrackSortDirection)> {
    match current {
        None => Some((column, TrackSortDirection::Asc)),
        Some((c, TrackSortDirection::Asc)) if c == column => {
            Some((column, TrackSortDirection::Desc))
        }
        Some((c, TrackSortDirection::Desc)) if c == column => None,
        Some(_) => Some((column, TrackSortDirection::Asc)),
    }
}

fn cmp_opt_date_added(a: &Option<String>, b: &Option<String>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            match (
                DateTime::parse_from_rfc3339(a),
                DateTime::parse_from_rfc3339(b),
            ) {
                (Ok(pa), Ok(pb)) => pa.cmp(&pb),
                (Err(_), Err(_)) => a.cmp(b),
                (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            }
        }
    }
}

fn ordered_tracks_for_view(
    tracks: &[PlaylistTrack],
    sort: Option<(TrackSortColumn, TrackSortDirection)>,
) -> Vec<PlaylistTrack> {
    let mut v: Vec<PlaylistTrack> = tracks.to_vec();
    let Some((col, dir)) = sort else {
        return v;
    };
    let ascending = matches!(dir, TrackSortDirection::Asc);
    match col {
        TrackSortColumn::Index => {
            v.sort_by(|a, b| {
                let o = a.position.cmp(&b.position);
                let o = if ascending { o } else { o.reverse() };
                o.then_with(|| a.spotify_uri.cmp(&b.spotify_uri))
            });
        }
        TrackSortColumn::Title => {
            v.sort_by(|a, b| {
                let o = a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.artist.to_lowercase().cmp(&b.artist.to_lowercase()));
                let o = if ascending { o } else { o.reverse() };
                o.then_with(|| a.spotify_uri.cmp(&b.spotify_uri))
            });
        }
        TrackSortColumn::Album => {
            v.sort_by(|a, b| {
                let o = a
                    .album
                    .to_lowercase()
                    .cmp(&b.album.to_lowercase())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                let o = if ascending { o } else { o.reverse() };
                o.then_with(|| a.spotify_uri.cmp(&b.spotify_uri))
            });
        }
        TrackSortColumn::DateAdded => {
            v.sort_by(|a, b| {
                let o = cmp_opt_date_added(&a.added_at, &b.added_at);
                let o = if ascending { o } else { o.reverse() };
                o.then_with(|| a.spotify_uri.cmp(&b.spotify_uri))
            });
        }
        TrackSortColumn::Duration => {
            v.sort_by(|a, b| {
                let o = a.duration_ms.cmp(&b.duration_ms);
                let o = if ascending { o } else { o.reverse() };
                o.then_with(|| a.spotify_uri.cmp(&b.spotify_uri))
            });
        }
    }
    v
}

fn shuffle_tracks(tracks: &mut [PlaylistTrack]) {
    if tracks.len() < 2 {
        return;
    }

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);

    for i in (1..tracks.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (seed as usize) % (i + 1);
        tracks.swap(i, j);
    }
}

fn compare_playlist_order(
    left: &PlaylistSummary,
    right: &PlaylistSummary,
    ordering: &PlaylistOrderingSettings,
) -> std::cmp::Ordering {
    playlist_order_rank(&left.id, ordering)
        .cmp(&playlist_order_rank(&right.id, ordering))
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
}

fn playlist_order_rank(playlist_id: &str, ordering: &PlaylistOrderingSettings) -> (usize, usize) {
    if let Some(index) = ordering
        .pinned_playlist_ids
        .iter()
        .position(|id| id == playlist_id)
    {
        return (0, index);
    }

    if crate::spotify_api::is_liked_songs_playlist(playlist_id) {
        return (1, 0);
    }

    if let Some(index) = ordering
        .recent_playlist_ids
        .iter()
        .position(|id| id == playlist_id)
    {
        return (2, index);
    }

    (3, usize::MAX)
}

/// UI font setup: **Manrope must be first** for proportional text so Latin, digits, and spaces use
/// correct advances (embedded **Noto Emoji** ahead of Manrope can still “win” basic codepoints with
/// broken metrics—same root cause as wide spacing). Order after Manrope: Noto Emoji, optional
/// system color emoji, CJK, Ubuntu, emoji-icon-font last.
fn configure_ui_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    if let Some(bytes) = load_manrope_font_bytes() {
        fonts.font_data.insert(
            "Manrope".to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
    }

    let mut system_emoji_key: Option<String> = None;
    for path in system_emoji_font_paths() {
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                log::info!(
                    "Loaded system color emoji font from {}",
                    path.display()
                );
                fonts.font_data.insert(
                    "system_color_emoji".to_owned(),
                    egui::FontData::from_owned(bytes).into(),
                );
                system_emoji_key = Some("system_color_emoji".to_owned());
                break;
            }
            Ok(_) => log::debug!("Skipping empty emoji font path {}", path.display()),
            Err(e) => log::debug!("No emoji font at {}: {}", path.display(), e),
        }
    }

    let mut system_cjk_keys: Vec<String> = Vec::new();
    for path in system_cjk_font_paths() {
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                let key = format!("system_cjk_{}", system_cjk_keys.len());
                log::info!("Loaded CJK fallback '{}' from {}", key, path.display());
                fonts.font_data.insert(
                    key.clone(),
                    egui::FontData::from_owned(bytes).into(),
                );
                system_cjk_keys.push(key);
                break;
            }
            Ok(_) => log::debug!("Skipping empty CJK font path {}", path.display()),
            Err(e) => log::debug!("No CJK font at {}: {}", path.display(), e),
        }
    }

    let mut proportional: Vec<String> = Vec::new();
    let primary_ui = if fonts.font_data.contains_key("Manrope") {
        "Manrope"
    } else {
        "Ubuntu-Light"
    };
    proportional.push(primary_ui.to_owned());
    proportional.push("NotoEmoji-Regular".to_owned());
    if let Some(ref k) = system_emoji_key {
        proportional.push(k.clone());
    }
    proportional.extend(system_cjk_keys.iter().cloned());
    if primary_ui == "Manrope" {
        proportional.push("Ubuntu-Light".to_owned());
    }
    proportional.push("emoji-icon-font".to_owned());

    let mut monospace: Vec<String> = Vec::new();
    monospace.push("Hack".to_owned());
    monospace.push("NotoEmoji-Regular".to_owned());
    if let Some(ref k) = system_emoji_key {
        monospace.push(k.clone());
    }
    monospace.extend(system_cjk_keys.iter().cloned());
    monospace.push("Ubuntu-Light".to_owned());
    monospace.push("emoji-icon-font".to_owned());

    fonts.families.insert(egui::FontFamily::Proportional, proportional);
    fonts.families.insert(egui::FontFamily::Monospace, monospace);

    let mut emoji_only: Vec<String> = Vec::new();
    if let Some(ref k) = system_emoji_key {
        emoji_only.push(k.clone());
    }
    emoji_only.push("NotoEmoji-Regular".to_owned());
    emoji_only.push("emoji-icon-font".to_owned());
    fonts.families.insert(
        egui::FontFamily::Name(EMOJI_FALLBACK_FAMILY.into()),
        emoji_only,
    );

    ctx.set_fonts(fonts);
}

fn load_manrope_font_bytes() -> Option<Vec<u8>> {
    let manrope_font_files = ["Manrope.ttf", "Manrope-Regular.ttf", "Manrope-Medium.ttf"];
    let app_font_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("fonts");
    let mut candidates: Vec<String> = manrope_font_files
        .iter()
        .map(|file| app_font_dir.join(file).display().to_string())
        .collect();
    candidates.extend(manrope_font_files.iter().flat_map(|file| {
        [
            format!("/Library/Fonts/{file}"),
            format!("/System/Library/Fonts/{file}"),
        ]
    }));
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        candidates.extend(manrope_font_files.iter().map(|file| {
            home.join("Library")
                .join("Fonts")
                .join(file)
                .display()
                .to_string()
        }));
    }
    candidates.extend(
        [
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/SFNSDisplay.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ]
        .iter()
        .map(|path| (*path).to_string()),
    );

    candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().filter(|b| !b.is_empty()))
}

fn system_emoji_font_paths() -> Vec<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![std::path::PathBuf::from(
            "/System/Library/Fonts/Apple Color Emoji.ttc",
        )]
    }
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        vec![std::path::PathBuf::from(windir)
            .join("Fonts")
            .join("seguiemj.ttf")]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            std::path::PathBuf::from("/usr/share/fonts/noto/NotoColorEmoji.ttf"),
            std::path::PathBuf::from("/usr/share/fonts/opentype/noto/NotoColorEmoji.ttf"),
            std::path::PathBuf::from("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"),
        ]
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    {
        Vec::new()
    }
}

fn system_cjk_font_paths() -> Vec<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            std::path::PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
            std::path::PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
            std::path::PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        let fonts = std::path::PathBuf::from(windir).join("Fonts");
        vec![
            fonts.join("msyh.ttc"),
            fonts.join("YuGothM.ttc"),
            fonts.join("msgothic.ttc"),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            std::path::PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            std::path::PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
            std::path::PathBuf::from("/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc"),
            std::path::PathBuf::from("/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
        ]
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    {
        Vec::new()
    }
}

fn display_position_ms(state: &PlaybackState) -> u32 {
    if !state.is_playing {
        return state.position_ms;
    }

    let elapsed_ms = state
        .position_updated_at
        .map(|updated_at| updated_at.elapsed().as_millis() as u32)
        .unwrap_or(0);
    state
        .position_anchor_ms
        .saturating_add(elapsed_ms)
        .min(state.duration_ms.max(state.position_anchor_ms))
}

fn summary_card(ui: &mut egui::Ui, label: &str, value: &str, outer_width: f32) {
    let card_height = 110.0;
    const INNER_PAD: i8 = 18;
    let pad = f32::from(INNER_PAD);
    let inner_w = (outer_width - 2.0 * pad).max(1.0);
    // Avoid inheriting horizontal layout when this card sits in `horizontal_top`.
    ui.allocate_ui_with_layout(
        egui::vec2(outer_width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(outer_width);
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(31, 31, 31))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::same(INNER_PAD))
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(inner_w, card_height));
                    ui.set_width(inner_w);
                    ui.label(
                        egui::RichText::new(label)
                            .color(egui::Color32::from_rgb(179, 179, 179))
                            .size(13.0),
                    );
                    ui.add_space(14.0);
                    ui.label(
                        egui::RichText::new(value)
                            .color(egui::Color32::WHITE)
                            .size(34.0)
                            .strong(),
                    );
                });
        },
    );
}

struct RankingResponse {
    metric_changed: bool,
    show_more: bool,
    show_less: bool,
}

const RANKING_ROW_HEIGHT: f32 = 31.0;
const RANKING_VALUE_WIDTH: f32 = 112.0;
const RANKING_RIGHT_GUTTER: f32 = 18.0;
const RANKING_ROW_RIGHT_INSET: f32 = 10.0;

fn render_bar_rankings(
    ui: &mut egui::Ui,
    title: &str,
    items: &[RankedItem],
    metric: StatsMetric,
    limit: u32,
    show_controls: bool,
) -> RankingResponse {
    let mut response = RankingResponse {
        metric_changed: false,
        show_more: false,
        show_less: false,
    };
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(31, 31, 31))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            let w = ui.available_width().max(0.0);
            ui.set_width(w);
            ui.set_min_width(0.0);
            ui.horizontal(|ui| {
                ui.heading(
                    egui::RichText::new(title)
                        .color(egui::Color32::WHITE)
                        .size(18.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if show_controls {
                        if ui
                            .add(metric_toggle_button(metric))
                            .on_hover_text("Toggle between plays and time")
                            .clicked()
                        {
                            response.metric_changed = true;
                        }
                    }
                });
            });
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(format!("Showing top {}", items.len().min(limit as usize)))
                    .color(egui::Color32::from_rgb(130, 130, 130))
                    .size(11.0),
            );
            ui.add_space(10.0);

            if items.is_empty() {
                ui.label(
                    egui::RichText::new("No data yet")
                        .color(egui::Color32::from_rgb(179, 179, 179))
                        .size(13.0),
                );
                return;
            }

            let max_value = items
                .first()
                .map(|item| ranking_value(item, metric))
                .unwrap_or(1)
                .max(1);

            if items.len() > 10 {
                let list_height = 10.0 * RANKING_ROW_HEIGHT;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), list_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_height(list_height);
                        egui::ScrollArea::vertical()
                            .id_salt(format!("{}_ranking_rows", title))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(
                                    (ui.available_width() - RANKING_ROW_RIGHT_INSET).max(120.0),
                                );
                                for (index, item) in items.iter().enumerate() {
                                    render_bar_row(ui, index, item, metric, max_value);
                                }
                            });
                    },
                );
            } else {
                ui.allocate_ui_with_layout(
                    egui::vec2(
                        ui.available_width(),
                        items.len() as f32 * RANKING_ROW_HEIGHT,
                    ),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        for (index, item) in items.iter().enumerate() {
                            render_bar_row(ui, index, item, metric, max_value);
                        }
                    },
                );
            }

            if show_controls {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("Show more")).clicked() {
                        response.show_more = true;
                    }
                    if limit > 10 && ui.add(egui::Button::new("Show less")).clicked() {
                        response.show_less = true;
                    }
                });
            }
        });
    response
}

fn render_bar_row(
    ui: &mut egui::Ui,
    index: usize,
    item: &RankedItem,
    metric: StatsMetric,
    max_value: u64,
) {
    let row_width = (ui.available_width() - RANKING_ROW_RIGHT_INSET).max(120.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(row_width, RANKING_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let value = ranking_value(item, metric);
    let fraction = (value as f32 / max_value as f32).clamp(0.04, 1.0);
    let chart_right = (rect.right() - RANKING_VALUE_WIDTH - RANKING_RIGHT_GUTTER).max(rect.left());
    let chart_rect = egui::Rect::from_min_max(rect.min, egui::pos2(chart_right, rect.bottom()));
    let bar_rect = egui::Rect::from_min_size(
        chart_rect.min,
        egui::vec2(chart_rect.width() * fraction, rect.height() - 3.0),
    );
    ui.painter()
        .rect_filled(bar_rect, 5.0, egui::Color32::from_rgb(64, 64, 64));

    let text_rect = rect.shrink2(egui::vec2(10.0, 0.0));
    let rank_rect = egui::Rect::from_min_size(text_rect.min, egui::vec2(28.0, text_rect.height()));
    let value_rect = egui::Rect::from_min_size(
        egui::pos2(
            text_rect.right() - RANKING_VALUE_WIDTH - RANKING_RIGHT_GUTTER,
            text_rect.top(),
        ),
        egui::vec2(RANKING_VALUE_WIDTH, text_rect.height()),
    );
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(rank_rect.right() + 4.0, text_rect.top()),
        egui::pos2(value_rect.left() - 8.0, text_rect.bottom()),
    );
    paint_left_text(
        ui,
        rank_rect,
        &format!("{}.", index + 1),
        egui::Color32::from_rgb(190, 190, 190),
        13.0,
        false,
    );
    paint_left_text(ui, name_rect, &item.name, egui::Color32::WHITE, 13.0, false);
    paint_right_text(
        ui,
        value_rect,
        &ranking_value_label(item, metric),
        egui::Color32::from_rgb(210, 210, 210),
        12.0,
    );
}

fn metric_toggle_button(metric: StatsMetric) -> egui::Button<'static> {
    let label = match metric {
        StatsMetric::Plays => "Plays",
        StatsMetric::Time => "Time",
    };
    egui::Button::new(label).fill(egui::Color32::from_rgb(45, 45, 45))
}

fn range_mode_button(
    ui: &mut egui::Ui,
    value: &mut StatsRangeMode,
    option: StatsRangeMode,
    label: &str,
) -> bool {
    let selected = *value == option;
    let (fill, text_color) = if selected {
        (ACCENT_GREEN, egui::Color32::WHITE)
    } else {
        (
            egui::Color32::from_rgb(38, 38, 38),
            egui::Color32::from_rgb(179, 179, 179),
        )
    };
    let button = egui::Button::new(egui::RichText::new(label).color(text_color)).fill(fill);
    if ui.add(button).clicked() {
        let changed = *value != option;
        *value = option;
        return changed;
    }
    false
}

fn toggle_metric(metric: StatsMetric) -> StatsMetric {
    match metric {
        StatsMetric::Plays => StatsMetric::Time,
        StatsMetric::Time => StatsMetric::Plays,
    }
}

fn next_stats_limit(limit: u32) -> u32 {
    match limit {
        0..=10 => 25,
        11..=25 => 50,
        _ => 100,
    }
}

fn ranking_value(item: &RankedItem, metric: StatsMetric) -> u64 {
    match metric {
        StatsMetric::Plays => u64::from(item.plays),
        StatsMetric::Time => item.duration_ms,
    }
}

fn ranking_value_label(item: &RankedItem, metric: StatsMetric) -> String {
    match metric {
        StatsMetric::Plays => format!("{} plays", item.plays),
        StatsMetric::Time => format_total_duration(item.duration_ms),
    }
}

fn month_name(month: u32) -> String {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Month",
    }
    .to_string()
}

fn format_duration(duration_ms: u32) -> String {
    let total_seconds = duration_ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{}:{:02}", minutes, seconds)
}

fn format_total_duration(duration_ms: u64) -> String {
    let total_minutes = duration_ms / 60_000;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{} hr {} min", hours, minutes)
    } else {
        format!("{} min", minutes)
    }
}

fn format_added_at(added_at: Option<&str>) -> String {
    added_at
        .and_then(|value| value.split('T').next())
        .unwrap_or("")
        .to_string()
}
