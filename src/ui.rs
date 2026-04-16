use crate::api::SpeechModel;
use crate::types::{AppStatus, TriggerEvent};
use eframe::egui;
use egui::{Align2, Button, Color32, FontId, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2, ViewportCommand};
use std::sync::{atomic::Ordering, Arc, RwLock, atomic::AtomicU32, mpsc};
use std::fs;
use std::env;
use crate::indicator_hwnd;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::Win32::Foundation::HWND;
use crate::{UI_REPAINT_INTERVAL, SPEECH_MODEL_SETTINGS_FILE};
use crate::utils::{top_center_position, offscreen_position, move_indicator_to_current_virtual_desktop};

#[derive(Default)]
pub struct PopupUiResult {
    pub close_clicked: bool,
    pub help_clicked: bool,
    pub toggle_settings: bool,
    pub selected_model: Option<SpeechModel>,
}

pub struct DictationIndicatorWrapper {
    pub status: Arc<RwLock<AppStatus>>,
    pub speech_model: Arc<RwLock<SpeechModel>>,
    pub audio_level: Arc<AtomicU32>,
    pub was_visible: bool,
    pub style_applied: bool,
    pub visibility_initialized: bool,
    pub settings_open: bool,
    pub _tray_icon: tray_icon::TrayIcon,
    pub tray_models: Vec<(SpeechModel, tray_icon::menu::CheckMenuItem)>,
    pub tray_quit_id: tray_icon::menu::MenuId,
    pub trigger_tx: mpsc::Sender<TriggerEvent>,
    // Animation States
    pub icon_scale: f32,
    pub wave_scale1: f32,
    pub wave_scale2: f32,
}

impl eframe::App for DictationIndicatorWrapper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::TRANSPARENT;
        visuals.window_fill = Color32::TRANSPARENT;
        ctx.set_visuals(visuals);

        let current_status = self
            .status
            .read()
            .map(|status| *status)
            .unwrap_or(AppStatus::Idle);
        let current_speech_model = self
            .speech_model
            .read()
            .map(|model| *model)
            .unwrap_or_default();
        let audio_level_raw = (self.audio_level.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0);
        let should_show = current_status != AppStatus::Idle || self.settings_open;

        // --- ANIMATION & LERPING ---
        if should_show {
            ctx.request_repaint();
        }

        let (target_icon_scale, target_wave_scale) = if current_status == AppStatus::Recording {
            let volume_normalized = (audio_level_raw * 10.0).clamp(0.0, 1.0);
            (1.0 + (volume_normalized * 0.3), if volume_normalized > 0.05 { 1.0 + (volume_normalized * 6.0) } else { 0.5 })
        } else {
            (1.0, 0.5)
        };

        self.icon_scale += (target_icon_scale - self.icon_scale) * 0.3;
        self.wave_scale1 += (target_wave_scale - self.wave_scale1) * 0.25;
        self.wave_scale2 += (target_wave_scale - self.wave_scale2) * 0.10;

        // Continuously enforce taskbar hiding (eframe/winit sometimes internally resets styles)
        apply_no_taskbar_style();

        if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id == self.tray_quit_id {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                for (model, item) in &self.tray_models {
                    if event.id == *item.id() {
                        if let Ok(mut m) = self.speech_model.write() {
                            *m = *model;
                        }
                        crate::ui::persist_selected_speech_model(*model);
                    }
                }
            }
        }

        let synced_model = self
            .speech_model
            .read()
            .map(|model| *model)
            .unwrap_or_default();

        for (model, item) in &self.tray_models {
            let should_be_checked = *model == synced_model;
            if item.is_checked() != should_be_checked {
                item.set_checked(should_be_checked);
            }
        }

        if !self.visibility_initialized {
            let initial_position = if should_show {
                move_indicator_to_current_virtual_desktop();
                top_center_position(ctx)
            } else {
                offscreen_position()
            };
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(initial_position));
            self.was_visible = should_show;
            self.visibility_initialized = true;
        }

        if should_show != self.was_visible {
            let target_position = if should_show {
                move_indicator_to_current_virtual_desktop();
                top_center_position(ctx)
            } else {
                offscreen_position()
            };
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(target_position));
            self.was_visible = should_show;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::TRANSPARENT))
            .show(ctx, |ui| {
                if should_show {
                    let result = draw_capsule_ui(
                        ui,
                        current_status,
                        self.icon_scale,
                        self.wave_scale1,
                        self.wave_scale2,
                        self.settings_open,
                        current_speech_model,
                    );

                    if result.toggle_settings {
                        self.settings_open = !self.settings_open;
                        
                        // If we are currently recording or transcribing, stop it when opening settings
                        // so they can choose a new model without it failing to apply.
                        if self.settings_open {
                            let current_status = self.status.read().map(|s| *s).unwrap_or(AppStatus::Idle);
                            if current_status != AppStatus::Idle {
                                let _ = self.trigger_tx.send(TriggerEvent::Transcribe);
                            }
                        }
                    }

                    if result.close_clicked {
                        self.settings_open = false;

                        // If we are currently recording or transcribing, send a signal to stop.
                        let current_status = self.status.read().map(|s| *s).unwrap_or(AppStatus::Idle);
                        if current_status != AppStatus::Idle {
                            let _ = self.trigger_tx.send(TriggerEvent::Transcribe);
                        }

                        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(offscreen_position()));
                    }

                    if let Some(new_model) = result.selected_model {
                        if let Ok(mut model) = self.speech_model.write() {
                            *model = new_model;
                        }
                        persist_selected_speech_model(new_model);
                        
                        // Close the settings dropdown automatically
                        self.settings_open = false;
                        
                        // Automatically start recording with the new model
                        let current_status = self.status.read().map(|s| *s).unwrap_or(AppStatus::Idle);
                        if current_status == AppStatus::Idle {
                            let _ = self.trigger_tx.send(TriggerEvent::Transcribe);
                        }
                    }
                }
            });

        ctx.request_repaint_after(UI_REPAINT_INTERVAL);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }
}

pub fn draw_capsule_ui(
    ui: &mut egui::Ui,
    status: AppStatus,
    icon_scale: f32,
    wave_scale1: f32,
    wave_scale2: f32,
    settings_open: bool,
    selected_model: SpeechModel,
) -> PopupUiResult {
    let mut result = PopupUiResult::default();
    
    // We anchor the capsule near the top to leave space for the dropdown below
    let capsule_size = Vec2::new(140.0, 52.0);
    let capsule_rect = Rect::from_center_size(ui.max_rect().center_top() + Vec2::new(0.0, 40.0), capsule_size);
    
    // Background and Interaction
    let response = ui.interact(capsule_rect, ui.id().with("capsule_main"), Sense::click_and_drag());
    if response.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    
    let painter = ui.painter();
    
    // Capsule Background
    painter.rect(
        capsule_rect,
        Rounding::same(22.0),
        Color32::from_rgb(31, 41, 55),
        Stroke::new(1.0, Color32::from_rgb(55, 65, 81)),
    );

    // Close button (small 'x' in corner or just use existing)
    // Actually, let's add a small close button to the top right of the capsule
    let close_rect = Rect::from_center_size(capsule_rect.right_top() + Vec2::new(-8.0, 8.0), Vec2::new(14.0, 14.0));
    let close_resp = ui.interact(close_rect, ui.id().with("close_icon"), Sense::click());
    if close_resp.hovered() {
        painter.circle_filled(close_rect.center(), 7.0, Color32::from_rgba_unmultiplied(255, 255, 255, 20));
    }
    painter.text(close_rect.center(), Align2::CENTER_CENTER, "×", FontId::proportional(14.0), Color32::GRAY);
    if close_resp.clicked() {
        result.close_clicked = true;
    }

    // --- Layout Inside Capsule ---
    
    // Left: Settings
    let settings_center = capsule_rect.left_center() + Vec2::new(28.0, 0.0);
    let settings_rect = Rect::from_center_size(settings_center, Vec2::new(28.0, 28.0));
    let settings_resp = ui.interact(settings_rect, ui.id().with("settings_btn"), Sense::click());
    
    let settings_color = if settings_resp.hovered() || settings_open { 
        Color32::from_rgb(205, 132, 255) 
    } else { 
        Color32::GRAY 
    };
    
    painter.circle_stroke(settings_center, 7.0, Stroke::new(1.5, settings_color));
    painter.circle_stroke(settings_center, 2.5, Stroke::new(1.5, settings_color));

    if settings_resp.clicked() {
        result.toggle_settings = true;
    }

    // Middle Divider
    let div_x = capsule_rect.center().x;
    painter.line_segment(
        [Pos2::new(div_x, capsule_rect.min.y + 12.0), Pos2::new(div_x, capsule_rect.max.y - 12.0)],
        Stroke::new(1.0, Color32::from_rgb(75, 85, 99)),
    );

    // Right: Mic
    let mic_center = capsule_rect.right_center() - Vec2::new(32.0, 0.0);
    draw_mic_and_waves(ui, mic_center, capsule_rect, status, icon_scale * 1.2, wave_scale1, wave_scale2);

    // Status / Model Text (below capsule)
    let status_text = match status {
        AppStatus::Idle => selected_model.display_name(),
        AppStatus::Recording => "Listening...",
        AppStatus::Transcribing => "Processing...",
    };
    painter.text(
        Pos2::new(capsule_rect.center().x, capsule_rect.bottom() + 12.0),
        Align2::CENTER_CENTER,
        status_text,
        FontId::proportional(11.0),
        Color32::from_rgba_unmultiplied(200, 200, 200, 180),
    );

    // Settings Dropdown
    if settings_open {
        result.selected_model = draw_settings_dropdown(ui, settings_rect, selected_model);
    }

    result
}

fn draw_mic_and_waves(
    ui: &egui::Ui,
    center: Pos2,
    clip_rect: Rect,
    status: AppStatus,
    icon_scale: f32,
    scale1: f32,
    scale2: f32,
) {
    let painter = ui.painter().with_clip_rect(clip_rect);
    let mic_color = if status == AppStatus::Recording { 
        Color32::from_rgb(56, 189, 248) 
    } else { 
        Color32::GRAY 
    };

    // Waves
    let base_wave_radius = 12.0;
    if scale2 > 0.6 {
        let opacity = (0.6 - (scale2 / 10.0)).clamp(0.0, 1.0) * 200.0;
        painter.circle_stroke(center, base_wave_radius * scale2, Stroke::new(1.5, Color32::from_rgba_unmultiplied(56, 189, 248, opacity as u8)));
    }
    if scale1 > 0.6 {
        let opacity = (1.0 - (scale1 / 8.0)).clamp(0.0, 1.0) * 200.0;
        painter.circle_stroke(center, base_wave_radius * scale1, Stroke::new(1.5, Color32::from_rgba_unmultiplied(56, 189, 248, opacity as u8)));
    }

    // Mic Icon
    let s = icon_scale;
    painter.rect_stroke(
        Rect::from_center_size(center - Vec2::new(0.0, 2.0 * s), Vec2::new(5.0 * s, 10.0 * s)),
        Rounding::same(2.5 * s),
        Stroke::new(1.8, mic_color)
    );
    painter.line_segment(
        [center + Vec2::new(-5.0 * s, 0.0), center + Vec2::new(-5.0 * s, 3.5 * s)],
        Stroke::new(1.8, mic_color)
    );
    painter.line_segment(
        [center + Vec2::new(5.0 * s, 0.0), center + Vec2::new(5.0 * s, 3.5 * s)],
        Stroke::new(1.8, mic_color)
    );
    painter.line_segment(
        [center + Vec2::new(-5.0 * s, 3.5 * s), center + Vec2::new(5.0 * s, 3.5 * s)],
        Stroke::new(1.8, mic_color)
    );
    painter.line_segment(
        [center + Vec2::new(0.0, 3.5 * s), center + Vec2::new(0.0, 7.0 * s)],
        Stroke::new(1.8, mic_color)
    );
}

fn draw_settings_dropdown(
    ui: &mut egui::Ui,
    anchor_rect: Rect,
    selected_model: SpeechModel,
) -> Option<SpeechModel> {
    let mut new_model = None;
    let window_id = ui.id().with("settings_popup");
    
    egui::Area::new(window_id)
        .order(egui::Order::Foreground)
        .fixed_pos(anchor_rect.left_bottom() + Vec2::new(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .fill(Color32::from_rgb(31, 41, 55))
                .stroke(Stroke::new(1.0, Color32::from_rgb(75, 85, 99)))
                .rounding(Rounding::same(8.0))
                .show(ui, |ui| {
                    ui.set_min_width(120.0);
                    
                    let models = [
                        (SpeechModel::GroqWhisper, "Groq Whisper"),
                        (SpeechModel::Telephony, "Telephony"),
                        (SpeechModel::Chirp3, "Chirp 3"),
                    ];
                    
                    for (m, label) in models {
                        let is_selected = m.settings_choice() == selected_model.settings_choice();
                        let text_color = if is_selected { Color32::from_rgb(205, 132, 255) } else { Color32::WHITE };
                        
                        if ui.selectable_label(is_selected, RichText::new(label).color(text_color)).clicked() {
                            new_model = Some(m);
                        }
                    }
                });
        });
        
    new_model
}


pub fn apply_no_taskbar_style() -> bool {
    unsafe {
        let Some(hwnd) = indicator_hwnd() else {
            return false;
        };

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let app_window = WS_EX_APPWINDOW.0 as i32;
        let tool_window = WS_EX_TOOLWINDOW.0 as i32;
        let updated_ex_style = (ex_style & !app_window) | tool_window;

        if updated_ex_style != ex_style {
            let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, updated_ex_style);
            let _ = SetWindowPos(
                hwnd,
                HWND(0),
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE | windows::Win32::UI::WindowsAndMessaging::SWP_FRAMECHANGED,
            );
        }

        true
    }
}

pub fn persist_selected_speech_model(model: SpeechModel) {
    let normalized = model.settings_choice().api_name();
    env::set_var("GCP_SPEECH_MODEL", normalized);
    if let Err(e) = fs::write(SPEECH_MODEL_SETTINGS_FILE, format!("{}\n", normalized)) {
        eprintln!(
            "Warning: failed to save speech model to {}: {}",
            SPEECH_MODEL_SETTINGS_FILE, e
        );
    }
}
