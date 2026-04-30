use crate::app_settings::{EQ_BANDS, EqualizerSettings, PlaylistOrderingSettings, UserSettings};
use crate::config::AppConfig;
use crate::downloads::{DOWNLOAD_DOWNLOADED, DOWNLOAD_DOWNLOADING, DownloadStatuses};
use crate::player::AudioCmd;
use crate::spotify_api::{PlaylistSummary, PlaylistTrack};
use crate::telemetry::{ListeningStats, RankedItem, StatsDateRange, StatsMetric, TelemetryDb};
use chrono::{Datelike, Utc};
use eframe::egui;
use rspotify::AuthCodeSpotify;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub track_name: String,
    pub artist_name: String,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainView {
    Dashboard,
    Playlist,
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

const MAX_RECENT_PLAYLISTS: usize = 50;

pub struct OnyxApp {
    pub spotify: AuthCodeSpotify,
    pub audio_cmd_tx: UnboundedSender<AudioCmd>,
    pub playback_state: Arc<Mutex<PlaybackState>>,
    pub db: Arc<Mutex<TelemetryDb>>,

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

    // Playback state toggles
    shuffle: bool,
    repeat: bool,
}

impl OnyxApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        spotify: AuthCodeSpotify,
        audio_cmd_tx: UnboundedSender<AudioCmd>,
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
        install_manrope_font(&cc.egui_ctx);

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

        let cached_playlists = crate::playlist_cache::PlaylistCache::new()
            .and_then(|cache| cache.load_playlists())
            .unwrap_or_else(|e| {
                log::warn!("Failed to load cached playlists: {}", e);
                Vec::new()
            });
        let cache_only = crate::spotify_api::cache_only_mode();
        let playlists_status_text = if cache_only && cached_playlists.is_empty() {
            "Cache-only mode: no cached playlists yet.".to_string()
        } else if cache_only {
            "Cache-only mode: Spotify API disabled.".to_string()
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
        let spotify_clone = spotify.clone();
        let ctx_clone = cc.egui_ctx.clone();

        egui_extras::install_image_loaders(&cc.egui_ctx);

        if !cache_only {
            rt.spawn(async move {
                match crate::spotify_api::user_playlists(&spotify_clone).await {
                    Ok(pl) => {
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

        Self {
            spotify,
            audio_cmd_tx,
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
            shuffle: false,
            repeat: false,
        }
    }

    fn select_playlist(&mut self, playlist: PlaylistSummary, ctx: &egui::Context) {
        if self
            .selected_playlist
            .as_ref()
            .is_some_and(|selected| selected.id == playlist.id)
        {
            return;
        }

        if let Some(task) = self.playlist_task.take() {
            task.abort();
        }

        self.playlist_generation += 1;
        let generation = self.playlist_generation;
        self.selected_playlist = Some(playlist.clone());
        self.main_view = MainView::Playlist;
        self.ensure_playlist_color(&playlist, ctx);
        let cache_only = crate::spotify_api::cache_only_mode();

        let cached_tracks = crate::playlist_cache::PlaylistCache::new()
            .ok()
            .and_then(|cache| cache.load_tracks(&playlist.id).ok().flatten());
        let can_use_cache_without_refresh = cached_tracks.as_ref().is_some_and(|cached| {
            cached.complete
                && !cached.tracks.is_empty()
                && (cache_only
                    || cached.snapshot_id.is_some()
                        && playlist.snapshot_id.is_some()
                        && cached.snapshot_id == playlist.snapshot_id
                    || crate::playlist_cache::PlaylistCache::cache_is_fresh(cached.fetched_at))
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

        let spotify = self.spotify.clone();
        let state = self.playlist_state.clone();
        let ctx = ctx.clone();
        let playlist_id = playlist.id.clone();

        self.playlist_task = Some(self.rt.spawn(async move {
            let mut cache = match crate::playlist_cache::PlaylistCache::new() {
                Ok(cache) => Some(cache),
                Err(e) => {
                    log::warn!("Playlist cache unavailable: {}", e);
                    None
                }
            };

            if let Some(cache) = cache.as_ref() {
                if let Err(e) = cache.save_playlist(&playlist, false) {
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
                        if let Err(e) = cache.finish_refresh(&playlist, tracks.len()) {
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
        let Some(url) = playlist
            .image_url
            .clone()
            .or_else(|| playlist.thumbnail_url.clone())
        else {
            if let Ok(mut colors) = self.playlist_colors.lock() {
                colors.entry(playlist.id.clone()).or_insert(None);
            }
            return;
        };

        if let Ok(mut colors) = self.playlist_colors.lock() {
            if colors.contains_key(&playlist.id) {
                return;
            }
            colors.insert(playlist.id.clone(), None);
        } else {
            return;
        }

        let playlist_id = playlist.id.clone();
        let colors = self.playlist_colors.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let color = fetch_playlist_color(url).await;
            if let Ok(mut colors) = colors.lock() {
                colors.insert(playlist_id, color);
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

impl eframe::App for OnyxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut state = self.playback_state.lock().unwrap().clone();
        self.advance_queue_after_track_end(&state);
        self.flush_pending_queue_load();
        state = self.playback_state.lock().unwrap().clone();
        let display_position_ms = display_position_ms(&state);

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

                // Left Section
                ui.allocate_ui_at_rect(left_rect, |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add_space(8.0); // push track image right just enough to align with sidebar (8 + 8 = 16)
                        if let Some(url) = &state.artwork_url {
                            ui.add(
                                egui::Image::new(url)
                                    .corner_radius(4_u8)
                                    .fit_to_exact_size(egui::vec2(56.0, 56.0)),
                            );
                        } else {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
                            ui.painter().rect_filled(
                                rect,
                                4.0,
                                egui::Color32::from_rgb(40, 40, 40),
                            );
                        }

                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.add_space(10.0); // vertically align
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
                                ui.label(
                                    egui::RichText::new(&state.artist_name)
                                        .color(egui::Color32::from_rgb(179, 179, 179))
                                        .size(12.0),
                                );
                            }
                        });
                    });
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
                                    egui::Color32::from_rgb(30, 215, 96)
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
                                    .clicked()
                                {
                                    self.toggle_shuffle();
                                }
                                if ui
                                    .add_sized(
                                        [btn_w, btn_w],
                                        egui::Button::new(egui::RichText::new("⏮").size(14.0))
                                            .frame(false),
                                    )
                                    .clicked()
                                {
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
                                        let _ = self.audio_cmd_tx.send(AudioCmd::Pause);
                                    } else {
                                        self.update_position_immediately(display_position_ms, true);
                                        let _ = self.audio_cmd_tx.send(AudioCmd::Play);
                                    }
                                }

                                if ui
                                    .add_sized(
                                        [btn_w, btn_w],
                                        egui::Button::new(egui::RichText::new("⏭").size(14.0))
                                            .frame(false),
                                    )
                                    .clicked()
                                {
                                    self.play_next();
                                }
                                let repeat_color = if self.repeat {
                                    egui::Color32::from_rgb(30, 215, 96)
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

                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(pb_width, 4.0),
                                    egui::Sense::click_and_drag(),
                                );
                                if resp.clicked() || resp.dragged() {
                                    if let Some(pos) = resp.interact_pointer_pos() {
                                        let x = (pos.x - rect.left()).clamp(0.0, pb_width);
                                        let pct = x / pb_width;
                                        let duration = state.duration_ms.max(1) as f32;
                                        let new_pos = (pct * duration) as u32;
                                        self.update_position_immediately(new_pos, state.is_playing);
                                        let _ = self.audio_cmd_tx.send(AudioCmd::Seek {
                                            position_ms: new_pos,
                                        });
                                    }
                                }
                                ui.painter().rect_filled(
                                    rect,
                                    2.0,
                                    egui::Color32::from_rgb(83, 83, 83),
                                );
                                let mut filled_rect = rect;
                                let pct = if state.duration_ms > 0 {
                                    (display_position_ms as f32 / state.duration_ms as f32)
                                        .clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                filled_rect.set_width(pb_width * pct);
                                ui.painter()
                                    .rect_filled(filled_rect, 2.0, egui::Color32::WHITE);

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
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(vol_w, 4.0),
                            egui::Sense::click_and_drag(),
                        );
                        if response.dragged() || response.clicked() {
                            if let Some(pos) = response.interact_pointer_pos() {
                                let x = (pos.x - rect.left()).clamp(0.0, vol_w);
                                let vol_pct = x / vol_w;
                                let new_vol = (vol_pct * 65535.0) as u16;
                                if new_vol != state.volume {
                                    state.volume = new_vol;
                                    if new_vol > 0 {
                                        self.previous_volume = new_vol;
                                    }
                                    self.set_volume_immediately(new_vol, true);
                                }
                            }
                        }

                        ui.painter()
                            .rect_filled(rect, 2.0, egui::Color32::from_rgb(83, 83, 83));
                        let mut filled_rect = rect;
                        filled_rect.set_width(vol_w * (state.volume as f32 / 65535.0));
                        ui.painter()
                            .rect_filled(filled_rect, 2.0, egui::Color32::WHITE);

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

        egui::SidePanel::left("sidebar")
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
                            if let Some(url) = p.thumbnail_url.as_ref().or(p.image_url.as_ref()) {
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
                            let meta_text = if let Some(status_text) = status_text {
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
        central_frame.inner_margin = if self.main_view == MainView::Dashboard {
            egui::Margin::same(0)
        } else {
            egui::Margin::same(24)
        };

        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ctx, |ui| {
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
                            self.render_playlist_view(ui, &playlist, &playlist_state, &state);
                        } else {
                            self.render_dashboard_view(ui);
                        }
                    }
                    MainView::Dashboard => self.render_dashboard_view(ui),
                }
            });
    }
}

impl OnyxApp {
    fn render_central_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, IconKind::Settings, 28.0).clicked() {
                    self.main_view = MainView::Settings;
                }
            });
        });
        ui.add_space(8.0);
    }

    fn render_playlist_view(
        &mut self,
        ui: &mut egui::Ui,
        playlist: &PlaylistSummary,
        playlist_state: &PlaylistLoadState,
        playback_state: &PlaybackState,
    ) {
        self.ensure_playlist_color(playlist, ui.ctx());
        let tracks = playlist_state.tracks.clone();
        let total_duration_ms: u64 = tracks.iter().map(|track| track.duration_ms as u64).sum();

        ui.horizontal(|ui| {
            if let Some(url) = &playlist.image_url {
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
                    let _ = self.audio_cmd_tx.send(AudioCmd::Pause);
                } else if self.playlist_is_current(playlist) {
                    let pos = display_position_ms(playback_state);
                    self.update_position_immediately(pos, true);
                    let _ = self.audio_cmd_tx.send(AudioCmd::Play);
                } else {
                    self.start_playlist(playlist.id.clone(), tracks.clone());
                }
            }

            let shuffle_color = if self.shuffle {
                egui::Color32::from_rgb(30, 215, 96)
            } else {
                egui::Color32::from_rgb(179, 179, 179)
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("🔀").size(20.0).color(shuffle_color))
                        .frame(false),
                )
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
        self.render_track_table_header(ui);

        if tracks.is_empty() {
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
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_rows(ui, row_height, tracks.len(), |ui, row_range| {
                for row in row_range {
                    let track = &tracks[row];
                    self.render_track_row(
                        ui,
                        playlist,
                        &tracks,
                        row,
                        track,
                        playback_state,
                        row_height,
                    );
                }
            });
    }

    fn render_track_table_header(&self, ui: &mut egui::Ui) {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
        let rect = rect.shrink2(egui::vec2(16.0, 0.0));
        let columns = TrackTableLayout::for_width(rect.width()).rects(rect);
        let color = egui::Color32::from_rgb(179, 179, 179);
        paint_left_text(ui, columns.index, "#", color, 13.0, false);
        paint_left_text(ui, columns.title, "Title", color, 13.0, false);
        paint_left_text(ui, columns.album, "Album", color, 13.0, false);
        paint_left_text(ui, columns.added, "Date added", color, 13.0, false);
        paint_right_text(ui, columns.duration, "Time", color, 13.0);
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
    ) {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click(),
        );
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
        }
        if resp.clicked() {
            self.start_playlist_at(playlist.id.clone(), tracks.to_vec(), row);
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
        paint_left_text(ui, artist_rect, &track.artist, muted, 12.0, false);
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
    }

    fn render_dashboard_view(&mut self, ui: &mut egui::Ui) {
        const DASHBOARD_PAD_LEFT: f32 = 28.0;
        const DASHBOARD_PAD_RIGHT: f32 = 44.0;
        const DASHBOARD_PAD_TOP: f32 = 18.0;
        const DASHBOARD_PAD_BOTTOM: f32 = 28.0;

        self.render_dashboard_edge_header(ui);

        egui::ScrollArea::vertical()
            .id_salt("dashboard_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let content_width =
                    (ui.available_width() - DASHBOARD_PAD_LEFT - DASHBOARD_PAD_RIGHT).max(280.0);
                ui.add_space(DASHBOARD_PAD_TOP);
                ui.horizontal(|ui| {
                    ui.add_space(DASHBOARD_PAD_LEFT);
                    ui.allocate_ui_with_layout(
                        egui::vec2(content_width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(content_width);
                            self.render_dashboard_content(ui);
                        },
                    );
                    ui.add_space(DASHBOARD_PAD_RIGHT);
                });
                ui.add_space(DASHBOARD_PAD_BOTTOM);
            });
    }

    fn render_dashboard_edge_header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(24.0);
                if icon_button(ui, IconKind::Settings, 28.0).clicked() {
                    self.main_view = MainView::Settings;
                }
            });
        });
        ui.add_space(4.0);
    }

    fn render_dashboard_content(&mut self, ui: &mut egui::Ui) {
        self.render_dashboard_header(ui);
        ui.add_space(16.0);

        if let Some(status) = &self.stats_status {
            ui.label(
                egui::RichText::new(status)
                    .color(egui::Color32::from_rgb(255, 180, 120))
                    .size(12.0),
            );
            ui.add_space(12.0);
        }

        if self.listening_stats.total_plays == 0 {
            let card_width = ui.available_width().min(760.0);
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(31, 31, 31))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(card_width);
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
                            egui::Button::new("Open Settings")
                                .fill(egui::Color32::from_rgb(30, 215, 96)),
                        )
                        .clicked()
                    {
                        self.main_view = MainView::Settings;
                    }
                });
            return;
        }

        self.render_summary_cards(ui);
        ui.add_space(24.0);

        if ui.available_width() < 760.0 {
            self.render_ranked_card(ui, "Top Tracks", RankingKind::Tracks);
            ui.add_space(16.0);
            self.render_ranked_card(ui, "Top Artists", RankingKind::Artists);
        } else {
            ui.columns(2, |columns| {
                self.render_ranked_card(&mut columns[0], "Top Tracks", RankingKind::Tracks);
                self.render_ranked_card(&mut columns[1], "Top Artists", RankingKind::Artists);
            });
        }

        if !self.listening_stats.top_albums.is_empty() {
            ui.add_space(24.0);
            let width = if ui.available_width() < 760.0 {
                ui.available_width()
            } else {
                (ui.available_width() - 12.0) / 2.0
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
        ui.add_space(14.0);
        self.render_stats_range_controls(ui);
    }

    fn render_summary_cards(&self, ui: &mut egui::Ui) {
        let available = ui.available_width();
        if available < 560.0 {
            summary_card(
                ui,
                "Time listened",
                &format_total_duration(self.listening_stats.total_listening_time_ms),
                available,
            );
            ui.add_space(12.0);
            summary_card(
                ui,
                "Tracks played",
                &self.listening_stats.total_plays.to_string(),
                available,
            );
        } else {
            let gap = 12.0;
            let card_width = (available - gap) / 2.0;
            ui.horizontal(|ui| {
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
                    egui::Button::new("Import Spotify ZIP")
                        .fill(egui::Color32::from_rgb(30, 215, 96)),
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
        egui::ScrollArea::vertical().show(ui, |ui| {
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
                        egui::Button::new("Save API Keys")
                            .fill(egui::Color32::from_rgb(30, 215, 96)),
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
        });
    }

    fn apply_equalizer_settings(&mut self) {
        let _ = self
            .audio_cmd_tx
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
            state.artwork_url = track.album_image_url.clone();
            state.spotify_uri = Some(track.spotify_uri.clone());
            state.position_ms = 0;
            state.position_anchor_ms = 0;
            state.position_updated_at = Some(Instant::now());
            state.duration_ms = track.duration_ms;
            state.is_playing = true;
        }

        let _ = self.audio_cmd_tx.send(AudioCmd::Load {
            uri: track.spotify_uri.clone(),
            start_playing: true,
            position_ms: 0,
        });
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
        let task = crate::downloads::spawn_playlist_download(
            &self.rt,
            self.spotify.clone(),
            self.audio_cmd_tx.clone(),
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
        if state.playlist_id.as_deref() != Some(playlist_id.as_str()) || state.tracks.is_empty() {
            return;
        }

        let current_uri = self
            .queue_index
            .and_then(|index| self.queue.get(index))
            .map(|track| track.spotify_uri.clone());

        self.queue = state.tracks;
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
            let _ = self.audio_cmd_tx.send(AudioCmd::Seek { position_ms: 0 });
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
                .audio_cmd_tx
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
    let panel_rect = ui.max_rect().expand2(egui::vec2(24.0, 24.0));
    let rect = egui::Rect::from_min_size(panel_rect.min, egui::vec2(panel_rect.width(), 280.0));

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

    let mut depth = egui::Mesh::default();
    let transparent = egui::Color32::from_rgba_unmultiplied(18, 18, 18, 0);
    let shaded = egui::Color32::from_rgba_unmultiplied(18, 18, 18, 92);
    depth.colored_vertex(rect.left_top(), transparent);
    depth.colored_vertex(rect.right_top(), shaded);
    depth.colored_vertex(rect.right_bottom(), shaded);
    depth.colored_vertex(rect.left_bottom(), transparent);
    depth.add_triangle(0, 1, 2);
    depth.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(depth));
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

    if let Some(index) = ordering
        .recent_playlist_ids
        .iter()
        .position(|id| id == playlist_id)
    {
        return (1, index);
    }

    (2, usize::MAX)
}

fn install_manrope_font(ctx: &egui::Context) {
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
            format!("/Library/Fonts/{}", file),
            format!("/System/Library/Fonts/{}", file),
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

    let mut fonts = egui::FontDefinitions::default();
    let Some((font_name, font_data)) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (path.clone(), bytes)))
    else {
        return;
    };

    fonts.font_data.insert(
        font_name.clone(),
        egui::FontData::from_owned(font_data).into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, font_name);
    ctx.set_fonts(fonts);
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

fn summary_card(ui: &mut egui::Ui, label: &str, value: &str, width: f32) {
    let card_height = 110.0;
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(31, 31, 31))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(width, card_height));
            ui.set_width(width);
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
            ui.set_width((ui.available_width() - 2.0).max(260.0));
            ui.set_min_width(260.0);
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
    let button = egui::Button::new(label).fill(if selected {
        egui::Color32::from_rgb(30, 215, 96)
    } else {
        egui::Color32::from_rgb(38, 38, 38)
    });
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
