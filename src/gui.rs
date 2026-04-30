use crate::app_settings::{EQ_BANDS, EqualizerSettings, UserSettings};
use crate::config::AppConfig;
use crate::player::AudioCmd;
use crate::spotify_api::{PlaylistSummary, PlaylistTrack};
use crate::telemetry::TelemetryDb;
use eframe::egui;
use rspotify::AuthCodeSpotify;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Default, Clone)]
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

pub struct OnyxApp {
    pub spotify: AuthCodeSpotify,
    pub audio_cmd_tx: UnboundedSender<AudioCmd>,
    pub playback_state: Arc<Mutex<PlaybackState>>,
    pub db: Arc<Mutex<TelemetryDb>>,

    top_artists: Vec<crate::telemetry::TopItem>,
    top_tracks: Vec<crate::telemetry::TopItem>,
    main_view: MainView,
    app_config: AppConfig,
    config_draft: AppConfig,
    user_settings: UserSettings,
    settings_status: Option<String>,

    // Phase 5 Additions
    pub rt: tokio::runtime::Handle,
    playlists: Arc<Mutex<Vec<PlaylistSummary>>>,
    playlists_status: Arc<Mutex<String>>,
    selected_playlist: Option<PlaylistSummary>,
    playlist_state: Arc<Mutex<PlaylistLoadState>>,
    playlist_generation: u64,
    playlist_task: Option<JoinHandle<()>>,
    queue: Vec<PlaylistTrack>,
    queue_playlist_id: Option<String>,
    queue_index: Option<usize>,
    pending_autoplay_playlist_id: Option<String>,
    pending_queue_index: Option<usize>,
    last_queue_load_at: Option<Instant>,
    observed_end_count: u64,
    last_sent_volume: u16,

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
        install_system_font(&cc.egui_ctx);

        let (top_artists, top_tracks) = {
            if let Ok(db_lock) = db.lock() {
                (
                    db_lock.top_artists(10).unwrap_or_default(),
                    db_lock.top_tracks(10).unwrap_or_default(),
                )
            } else {
                (Vec::new(), Vec::new())
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
            top_artists,
            top_tracks,
            main_view: MainView::Dashboard,
            app_config: app_config.clone(),
            config_draft: app_config,
            user_settings,
            settings_status: None,
            rt,
            playlists,
            playlists_status,
            selected_playlist: None,
            playlist_state: Arc::new(Mutex::new(PlaylistLoadState::default())),
            playlist_generation: 0,
            playlist_task: None,
            queue: Vec::new(),
            queue_playlist_id: None,
            queue_index: None,
            pending_autoplay_playlist_id: None,
            pending_queue_index: None,
            last_queue_load_at: None,
            observed_end_count: 0,
            last_sent_volume: 0,
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
                PlaylistStatus::Error("Cache-only mode: no cached tracks for this playlist.".to_string())
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
                        ui.add_space(2.0);
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
                                    if let Ok(mut shared) = self.playback_state.lock() {
                                        shared.volume = new_vol;
                                    }
                                    if self.last_sent_volume.abs_diff(new_vol) > 384 {
                                        self.last_sent_volume = new_vol;
                                        let _ = self.audio_cmd_tx.send(AudioCmd::SetVolume {
                                            volume_u16: new_vol,
                                        });
                                    }
                                }
                            }
                        }

                        ui.painter()
                            .rect_filled(rect, 2.0, egui::Color32::from_rgb(83, 83, 83));
                        let mut filled_rect = rect;
                        filled_rect.set_width(vol_w * (state.volume as f32 / 65535.0));
                        ui.painter()
                            .rect_filled(filled_rect, 2.0, egui::Color32::WHITE);

                        // Volume Icon
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_rgb(179, 179, 179)
                        };
                        let c = rect.center() + egui::vec2(-2.0, 0.0);
                        let stroke = (1.5, color);
                        ui.painter().rect_stroke(
                            egui::Rect::from_center_size(
                                c - egui::vec2(2.0, 0.0),
                                egui::vec2(3.0, 6.0),
                            ),
                            0.0,
                            stroke,
                            egui::StrokeKind::Middle,
                        );
                        ui.painter().line_segment(
                            [c - egui::vec2(0.5, 3.0), c + egui::vec2(3.5, -6.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.5, -6.0), c + egui::vec2(3.5, 6.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(3.5, 6.0), c - egui::vec2(0.5, 3.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(6.5, -3.0), c + egui::vec2(6.5, 3.0)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [c + egui::vec2(9.5, -5.0), c + egui::vec2(9.5, 5.0)],
                            stroke,
                        );

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
                                egui::vec2((rect.right() - text_left - 8.0).max(20.0), 18.0),
                            );
                            let meta_rect = egui::Rect::from_min_size(
                                egui::pos2(text_left, rect.top() + 29.0),
                                egui::vec2((rect.right() - text_left - 8.0).max(20.0), 16.0),
                            );
                            paint_left_text(ui, name_rect, &p.name, name_color, 13.0, true);
                            paint_left_text(
                                ui,
                                meta_rect,
                                &format!("Playlist • {} tracks", p.track_count),
                                egui::Color32::from_rgb(179, 179, 179),
                                12.0,
                                false,
                            );
                        }
                    });
            });

        // CENTRAL PANEL (#121212)
        let mut central_frame = egui::Frame::default();
        central_frame.fill = egui::Color32::from_rgb(18, 18, 18);
        central_frame.inner_margin = egui::Margin::same(24);

        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, IconKind::Settings, 28.0).clicked() {
                            self.main_view = MainView::Settings;
                        }
                    });
                });
                ui.add_space(8.0);

                match self.main_view {
                    MainView::Settings => self.render_settings_view(ui),
                    MainView::Playlist => {
                        if let Some(playlist) = self.selected_playlist.clone() {
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
    fn render_playlist_view(
        &mut self,
        ui: &mut egui::Ui,
        playlist: &PlaylistSummary,
        playlist_state: &PlaylistLoadState,
        playback_state: &PlaybackState,
    ) {
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
        let title_color = if is_current { green } else { egui::Color32::WHITE };
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
            egui::pos2(columns.title.left(), columns.title.center().y - image_size / 2.0),
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
        paint_right_text(ui, columns.duration, &format_duration(track.duration_ms), muted, 13.0);
    }

    fn render_dashboard_view(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(
                egui::RichText::new("Dashboard")
                    .color(egui::Color32::WHITE)
                    .size(24.0)
                    .strong(),
            );
            ui.add_space(16.0);

            ui.columns(2, |columns| {
                columns[0].heading(egui::RichText::new("Top Tracks").color(egui::Color32::WHITE));
                columns[0].add_space(8.0);
                for track in &self.top_tracks {
                    columns[0].label(
                        egui::RichText::new(format!("{} ({} plays)", track.name, track.count))
                            .color(egui::Color32::from_rgb(179, 179, 179)),
                    );
                }

                columns[1].heading(egui::RichText::new("Top Artists").color(egui::Color32::WHITE));
                columns[1].add_space(8.0);
                for artist in &self.top_artists {
                    columns[1].label(
                        egui::RichText::new(format!("{} ({} plays)", artist.name, artist.count))
                            .color(egui::Color32::from_rgb(179, 179, 179)),
                    );
                }
            });
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
                    .add(egui::Button::new("Save API Keys").fill(egui::Color32::from_rgb(30, 215, 96)))
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
            Err(e) => self.settings_status = Some(format!("Failed to save equalizer settings: {}", e)),
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
                [egui::pos2(x, graph_rect.top()), egui::pos2(x, graph_rect.bottom())],
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
            .map(|(idx, gain)| egui::pos2(band_x(graph_rect, idx), db_to_graph_y(graph_rect, *gain)))
            .collect();
        let mut fill_points = Vec::with_capacity(points.len() + 2);
        fill_points.push(egui::pos2(graph_rect.left(), graph_rect.bottom()));
        fill_points.extend(points.iter().copied());
        fill_points.push(egui::pos2(graph_rect.right(), graph_rect.bottom()));
        ui.painter().add(egui::Shape::convex_polygon(
            fill_points,
            egui::Color32::from_rgba_unmultiplied(30, 215, 96, 70),
            egui::Stroke::NONE,
        ));

        ui.painter().add(egui::Shape::line(
            points.clone(),
            egui::Stroke::new(3.0, egui::Color32::from_rgb(30, 215, 96)),
        ));

        for (idx, point) in points.iter().enumerate() {
            let hit_rect = egui::Rect::from_center_size(*point, egui::vec2(22.0, 22.0));
            let response = ui.interact(hit_rect, ui.id().with(("eq_band", idx)), egui::Sense::drag());
            if response.dragged() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    self.user_settings.equalizer.bands_db[idx] =
                        graph_y_to_db(graph_rect, pointer.y).clamp(-12.0, 12.0);
                    changed = true;
                }
            }
            ui.painter().circle_filled(*point, 4.0, egui::Color32::WHITE);
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
                ui.label(
                    egui::RichText::new("Preamp")
                        .color(label_color)
                        .size(12.0),
                );
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.user_settings.equalizer.preamp_db, -12.0..=12.0)
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
        let h = size * 0.44;
        let w = size * 0.36;
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                center + egui::vec2(-w * 0.42, -h * 0.5),
                center + egui::vec2(-w * 0.42, h * 0.5),
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
        ui.painter()
            .circle_filled(rect.center(), size * 0.48, egui::Color32::from_rgb(32, 32, 32));
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
    ui.painter().line_segment([body_left, body_bottom_left], stroke);
    ui.painter().line_segment([body_right, body_bottom_right], stroke);
    ui.painter()
        .line_segment([body_bottom_left, body_bottom_right], stroke);
}

fn paint_settings_icon(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.5, color);
    let r = rect.width() * 0.19;
    ui.painter().circle_stroke(c, r, stroke);
    ui.painter().circle_stroke(c, r * 0.38, stroke);

    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let dir = egui::vec2(angle.cos(), angle.sin());
        ui.painter()
            .line_segment([c + dir * (r * 1.25), c + dir * (r * 1.75)], stroke);
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
    let galley = ui.painter().layout_no_wrap(text.clone(), font_id.clone(), color);
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
    let pos = egui::pos2(rect.right() - galley.size().x, rect.center().y - size * 0.55);
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

fn install_system_font(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/SFNSDisplay.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];

    let Some((font_name, font_data)) = candidates.iter().find_map(|path| {
        std::fs::read(path)
            .ok()
            .map(|bytes| ((*path).to_string(), bytes))
    }) else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
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
