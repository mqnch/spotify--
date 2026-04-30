use eframe::egui;
use rspotify::AuthCodeSpotify;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use crate::player::AudioCmd;
use crate::telemetry::TelemetryDb;

#[derive(Default, Clone)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub track_name: String,
    pub artist_name: String,
    pub position_ms: u32,
    pub volume: u16,
}

pub struct OnyxApp {
    pub spotify: AuthCodeSpotify,
    pub audio_cmd_tx: UnboundedSender<AudioCmd>,
    pub playback_state: Arc<Mutex<PlaybackState>>,
    pub db: Arc<Mutex<TelemetryDb>>,
    
    top_artists: Vec<crate::telemetry::TopItem>,
    top_tracks: Vec<crate::telemetry::TopItem>,
}

impl OnyxApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        spotify: AuthCodeSpotify,
        audio_cmd_tx: UnboundedSender<AudioCmd>,
        playback_state: Arc<Mutex<PlaybackState>>,
        db: Arc<Mutex<TelemetryDb>>,
    ) -> Self {
        // Spotify-like Visuals
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(18, 18, 18); // Default background
        visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(179, 179, 179); // Gray text
        visuals.selection.bg_fill = egui::Color32::from_rgb(30, 215, 96); // Spotify Green
        cc.egui_ctx.set_visuals(visuals);

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

        Self {
            spotify,
            audio_cmd_tx,
            playback_state,
            db,
            top_artists,
            top_tracks,
        }
    }
}

impl eframe::App for OnyxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut state = self.playback_state.lock().unwrap().clone();

        if state.is_playing {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
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
                
                let left_rect = egui::Rect::from_min_size(available.min, egui::vec2(w_left, available.height()));
                let center_rect = egui::Rect::from_min_size(left_rect.right_top(), egui::vec2(w_center, available.height()));
                let right_rect = egui::Rect::from_min_size(center_rect.right_top(), egui::vec2(w_right, available.height()));

                // Left Section
                ui.allocate_ui_at_rect(left_rect, |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add_space(8.0); // push track image right just enough to align with sidebar (8 + 8 = 16)
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
                        
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.add_space(10.0); // vertically align
                            if state.track_name.is_empty() {
                                ui.label(egui::RichText::new("No track playing").color(egui::Color32::WHITE).strong());
                            } else {
                                ui.label(egui::RichText::new(&state.track_name).color(egui::Color32::WHITE).strong());
                                ui.label(egui::RichText::new(&state.artist_name).color(egui::Color32::from_rgb(179, 179, 179)).size(12.0));
                            }
                        });
                    });
                });
                
                // Center Section
                ui.allocate_ui_at_rect(center_rect, |ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.add_space(8.0); // center vertically within the 72px bottom bar
                        // Controls Row
                        ui.allocate_ui_with_layout(
                            egui::vec2(center_rect.width(), 32.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let spacing = 16.0;
                                let btn_w = 28.0;
                                let play_w = 32.0;
                                let total_w = 4.0 * btn_w + play_w + 4.0 * spacing;
                                
                                // Push to center
                                let center_space = ((center_rect.width() - total_w) / 2.0).max(0.0).floor();
                                ui.add_space(center_space);
                                ui.spacing_mut().item_spacing.x = spacing;
                                
                                let _ = ui.add_sized([btn_w, btn_w], egui::Button::new(egui::RichText::new("🔀").size(16.0)).frame(false));
                                let _ = ui.add_sized([btn_w, btn_w], egui::Button::new(egui::RichText::new("⏮").size(16.0)).frame(false));
                                
                                let play_icon = if state.is_playing { "⏸" } else { "▶" };
                                let play_btn = egui::Button::new(egui::RichText::new(play_icon).size(18.0).color(egui::Color32::BLACK))
                                    .fill(egui::Color32::WHITE)
                                    .corner_radius(16);
                                    
                                if ui.add_sized([play_w, play_w], play_btn).clicked() {
                                    if state.is_playing {
                                        let _ = self.audio_cmd_tx.send(AudioCmd::Pause);
                                    } else {
                                        let _ = self.audio_cmd_tx.send(AudioCmd::Play);
                                    }
                                }
                                
                                let _ = ui.add_sized([btn_w, btn_w], egui::Button::new(egui::RichText::new("⏭").size(16.0)).frame(false));
                                let _ = ui.add_sized([btn_w, btn_w], egui::Button::new(egui::RichText::new("🔁").size(16.0)).frame(false));
                            }
                        );
                        
                        ui.add_space(4.0);
                        // Progress Row
                        ui.allocate_ui_with_layout(
                            egui::vec2(center_rect.width(), 20.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let mins = state.position_ms / 60000;
                                let secs = (state.position_ms / 1000) % 60;
                                let time_text = format!("{}:{:02}", mins, secs);
                                
                                let time_w = 30.0;
                                let pb_width = (center_rect.width() - (time_w * 2.0) - 32.0).max(10.0);
                                
                                let center_space = ((center_rect.width() - (pb_width + time_w * 2.0 + 16.0)) / 2.0).max(0.0).floor();
                                ui.add_space(center_space);
                                
                                ui.add_sized([time_w, 12.0], egui::Label::new(egui::RichText::new(time_text).size(11.0).color(egui::Color32::from_rgb(179, 179, 179))));
                                
                                ui.spacing_mut().item_spacing.x = 8.0;
                                
                                // Custom thin progress bar
                                let (rect, _resp) = ui.allocate_exact_size(egui::vec2(pb_width, 4.0), egui::Sense::hover());
                                ui.painter().rect_filled(rect, 2.0, egui::Color32::from_rgb(83, 83, 83));
                                let mut filled_rect = rect;
                                // In a real app, this would be a percentage based on track length:
                                // let pct = (state.position_ms as f32 / duration).clamp(0.0, 1.0);
                                filled_rect.set_width(0.0); 
                                ui.painter().rect_filled(filled_rect, 2.0, egui::Color32::WHITE);
                                
                                ui.add_sized([time_w, 12.0], egui::Label::new(egui::RichText::new("-:--").size(11.0).color(egui::Color32::from_rgb(179, 179, 179))));
                            }
                        );
                    });
                });
                
                // Right Section
                ui.allocate_ui_at_rect(right_rect, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(24.0);
                        
                        let btn_w = 24.0;
                        
                        // Fullscreen Icon
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() { egui::Color32::WHITE } else { egui::Color32::from_rgb(179, 179, 179) };
                        let p = ui.painter();
                        let m = rect.center() - egui::vec2(6.0, 6.0);
                        let s = 12.0;
                        let stroke = (1.5, color);
                        p.line_segment([m, m + egui::vec2(4.0, 0.0)], stroke); p.line_segment([m, m + egui::vec2(0.0, 4.0)], stroke);
                        p.line_segment([m + egui::vec2(s - 4.0, 0.0), m + egui::vec2(s, 0.0)], stroke); p.line_segment([m + egui::vec2(s, 0.0), m + egui::vec2(s, 4.0)], stroke);
                        p.line_segment([m + egui::vec2(0.0, s - 4.0), m + egui::vec2(0.0, s)], stroke); p.line_segment([m + egui::vec2(0.0, s), m + egui::vec2(4.0, s)], stroke);
                        p.line_segment([m + egui::vec2(s - 4.0, s), m + egui::vec2(s, s)], stroke); p.line_segment([m + egui::vec2(s, s - 4.0), m + egui::vec2(s, s)], stroke);
                        
                        let vol_w = 80.0;
                        let (rect, response) = ui.allocate_exact_size(egui::vec2(vol_w, 4.0), egui::Sense::click_and_drag());
                        if response.dragged() || response.clicked() {
                            if let Some(pos) = response.interact_pointer_pos() {
                                let x = (pos.x - rect.left()).clamp(0.0, vol_w);
                                let vol_pct = x / vol_w;
                                let new_vol = (vol_pct * 65535.0) as u16;
                                if new_vol != state.volume {
                                    state.volume = new_vol;
                                    let _ = self.audio_cmd_tx.send(AudioCmd::SetVolume { volume_u16: new_vol });
                                }
                            }
                        }
                        
                        ui.painter().rect_filled(rect, 2.0, egui::Color32::from_rgb(83, 83, 83));
                        let mut filled_rect = rect;
                        filled_rect.set_width(vol_w * (state.volume as f32 / 65535.0));
                        ui.painter().rect_filled(filled_rect, 2.0, egui::Color32::WHITE);
                        
                        // Volume Icon
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() { egui::Color32::WHITE } else { egui::Color32::from_rgb(179, 179, 179) };
                        let c = rect.center() + egui::vec2(-2.0, 0.0);
                        let stroke = (1.5, color);
                        ui.painter().rect_stroke(egui::Rect::from_center_size(c - egui::vec2(2.0, 0.0), egui::vec2(3.0, 6.0)), 0.0, stroke, egui::StrokeKind::Middle);
                        ui.painter().line_segment([c - egui::vec2(0.5, 3.0), c + egui::vec2(3.5, -6.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.5, -6.0), c + egui::vec2(3.5, 6.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.5, 6.0), c - egui::vec2(0.5, 3.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(6.5, -3.0), c + egui::vec2(6.5, 3.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(9.5, -5.0), c + egui::vec2(9.5, 5.0)], stroke);
                        
                        // Device Icon
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() { egui::Color32::WHITE } else { egui::Color32::from_rgb(179, 179, 179) };
                        let c = rect.center();
                        let stroke = (1.5, color);
                        ui.painter().rect_stroke(egui::Rect::from_center_size(c - egui::vec2(0.0, 1.0), egui::vec2(14.0, 10.0)), 1.0, stroke, egui::StrokeKind::Middle);
                        ui.painter().line_segment([c + egui::vec2(-4.0, 7.0), c + egui::vec2(4.0, 7.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(0.0, 4.0), c + egui::vec2(0.0, 7.0)], stroke);
                        
                        // Queue Icon
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(btn_w, btn_w), egui::Sense::click());
                        let color = if resp.hovered() { egui::Color32::WHITE } else { egui::Color32::from_rgb(179, 179, 179) };
                        let c = rect.center();
                        let stroke = (1.5, color);
                        ui.painter().line_segment([c + egui::vec2(-6.0, -4.0), c + egui::vec2(6.0, -4.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(-6.0, 0.0), c + egui::vec2(6.0, 0.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(-6.0, 4.0), c + egui::vec2(1.0, 4.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.0, 2.0), c + egui::vec2(3.0, 6.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.0, 2.0), c + egui::vec2(7.0, 4.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.0, 6.0), c + egui::vec2(7.0, 4.0)], stroke);
                    });
                });
            });

        // SIDEBAR (#000000)
        let mut side_frame = egui::Frame::default();
        side_frame.fill = egui::Color32::from_rgb(0, 0, 0);
        side_frame.inner_margin = egui::Margin::same(16);
        
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(280.0)
            .width_range(200.0..=400.0)
            .frame(side_frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let btn_size = 12.0;
                    
                    let (close_rect, close_resp) = ui.allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(close_rect.center(), btn_size / 2.0, egui::Color32::from_rgb(255, 95, 86));
                    if close_resp.hovered() {
                        let c = close_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(77, 0, 0));
                        ui.painter().line_segment([c - egui::vec2(3.0, 3.0), c + egui::vec2(3.0, 3.0)], stroke);
                        ui.painter().line_segment([c + egui::vec2(3.0, -3.0), c - egui::vec2(3.0, -3.0)], stroke);
                    }
                    if close_resp.clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                    
                    let (min_rect, min_resp) = ui.allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(min_rect.center(), btn_size / 2.0, egui::Color32::from_rgb(255, 189, 46));
                    if min_resp.hovered() {
                        let c = min_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(153, 87, 0));
                        ui.painter().line_segment([c - egui::vec2(3.0, 0.0), c + egui::vec2(3.0, 0.0)], stroke);
                    }
                    if min_resp.clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)); }
                    
                    let (max_rect, max_resp) = ui.allocate_exact_size(egui::vec2(btn_size, btn_size), egui::Sense::click());
                    ui.painter().circle_filled(max_rect.center(), btn_size / 2.0, egui::Color32::from_rgb(39, 201, 63));
                    if max_resp.hovered() {
                        let c = max_rect.center();
                        let stroke = (1.5, egui::Color32::from_rgb(0, 101, 0));
                        ui.painter().line_segment([c - egui::vec2(0.0, 3.0), c + egui::vec2(0.0, 3.0)], stroke);
                        ui.painter().line_segment([c - egui::vec2(3.0, 0.0), c + egui::vec2(3.0, 0.0)], stroke);
                    }
                    if max_resp.clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true)); }
                });
                ui.add_space(24.0);
                
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Your Library").color(egui::Color32::from_rgb(179, 179, 179)).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let _ = ui.add(egui::Button::new(egui::RichText::new("→").size(16.0)).frame(false));
                        let _ = ui.add(egui::Button::new(egui::RichText::new("+").size(16.0)).frame(false));
                    });
                });
                
                ui.add_space(12.0);
                
                // Recents row
                ui.horizontal(|ui| {
                    let _ = ui.add(egui::Button::new(egui::RichText::new("🔍").size(14.0)).frame(false));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new("Recents ☰").size(12.0));
                    });
                });
                
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for i in 1..=20 {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                                ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 40, 40));
                                
                                ui.vertical(|ui| {
                                    ui.add_space(6.0);
                                    ui.label(egui::RichText::new(format!("Playlist {}", i)).color(egui::Color32::WHITE).strong());
                                    ui.label(egui::RichText::new("Playlist • felix").color(egui::Color32::from_rgb(179, 179, 179)).size(12.0));
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });
            });

        // CENTRAL PANEL (#121212)
        let mut central_frame = egui::Frame::default();
        central_frame.fill = egui::Color32::from_rgb(18, 18, 18);
        central_frame.inner_margin = egui::Margin::same(24);
        
        egui::CentralPanel::default().frame(central_frame).show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                ui.heading(egui::RichText::new("Dashboard").color(egui::Color32::WHITE).size(24.0).strong());
                ui.add_space(16.0);
    
                ui.columns(2, |columns| {
                    columns[0].heading(egui::RichText::new("Top Tracks").color(egui::Color32::WHITE));
                    columns[0].add_space(8.0);
                    for track in &self.top_tracks {
                        columns[0].label(egui::RichText::new(format!("{} ({} plays)", track.name, track.count)).color(egui::Color32::from_rgb(179, 179, 179)));
                    }
    
                    columns[1].heading(egui::RichText::new("Top Artists").color(egui::Color32::WHITE));
                    columns[1].add_space(8.0);
                    for artist in &self.top_artists {
                        columns[1].label(egui::RichText::new(format!("{} ({} plays)", artist.name, artist.count)).color(egui::Color32::from_rgb(179, 179, 179)));
                    }
                });
            });
        });
    }
}
