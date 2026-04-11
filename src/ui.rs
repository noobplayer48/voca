use crate::api::SpeechModel;
use crate::types::AppStatus;
use eframe::egui;
use egui::{Align2, Button, Color32, FontId, RichText, Sense, Stroke, ViewportCommand};
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
    pub trigger_tx: mpsc::Sender<()>,
}

impl eframe::App for DictationIndicatorWrapper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        let audio_level = (self.audio_level.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0);
        let should_show = current_status != AppStatus::Idle || self.settings_open;

        if !self.style_applied {
            self.style_applied = apply_no_taskbar_style();
        }

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
            .frame(egui::Frame::none().fill(Color32::from_rgb(12, 14, 18)))
            .show(ctx, |ui| {
                if should_show {
                    let result = draw_popup_card(
                        ui,
                        current_status,
                        audio_level,
                        self.settings_open,
                        current_speech_model,
                    );

                    if result.toggle_settings {
                        self.settings_open = !self.settings_open;
                    }

                    if result.close_clicked {
                        self.settings_open = false;

                        // If we are currently recording or transcribing, send a signal to stop.
                        let current_status = self.status.read().map(|s| *s).unwrap_or(AppStatus::Idle);
                        if current_status != AppStatus::Idle {
                            let _ = self.trigger_tx.send(());
                        }

                        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(offscreen_position()));
                    }

                    if result.help_clicked {
                        eprintln!(
                            "Settings tip: choose Chirp 3 for general dictation or Telephony for phone-call audio."
                        );
                    }

                    if let Some(new_model) = result.selected_model {
                        if let Ok(mut model) = self.speech_model.write() {
                            *model = new_model;
                        }
                        persist_selected_speech_model(new_model);
                    }
                }
            });

        ctx.request_repaint_after(UI_REPAINT_INTERVAL);
    }
}

pub fn draw_popup_card(
    ui: &mut egui::Ui,
    status: AppStatus,
    audio_level: f32,
    settings_open: bool,
    selected_model: SpeechModel,
) -> PopupUiResult {
    let rect = ui.max_rect().shrink(4.0);
    let painter = ui.painter_at(rect);
    let rounding = egui::Rounding::same(12.0);
    let mut result = PopupUiResult::default();

    painter.rect_filled(
        rect,
        rounding,
        Color32::from_rgba_unmultiplied(28, 30, 36, 244),
    );
    painter.rect_stroke(
        rect,
        rounding,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 24)),
    );

    let header_y = rect.top() + 20.0;
    let drag_rect =
        egui::Rect::from_center_size(egui::pos2(rect.center().x, header_y), egui::vec2(42.0, 6.0));
    let drag_response = ui.interact(
        drag_rect.expand2(egui::vec2(8.0, 6.0)),
        ui.id().with("drag_handle"),
        Sense::click_and_drag(),
    ).on_hover_text("Drag to move");
    if drag_response.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    let drag_color = if drag_response.hovered() || drag_response.dragged() {
        Color32::from_rgba_unmultiplied(232, 236, 242, 220)
    } else {
        Color32::from_rgba_unmultiplied(215, 219, 226, 180)
    };
    painter.rect_filled(drag_rect, egui::Rounding::same(3.0), drag_color);

    let close_center = egui::pos2(rect.right() - 22.0, header_y);
    let close_rect = egui::Rect::from_center_size(close_center, egui::vec2(24.0, 24.0));
    let close_response = ui.interact(close_rect, ui.id().with("close_btn"), Sense::click()).on_hover_text("Hide indicator");
    if close_response.hovered() {
        painter.circle_filled(
            close_center,
            10.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 24),
        );
    }
    if close_response.clicked() {
        result.close_clicked = true;
    }
    painter.text(
        close_center,
        Align2::CENTER_CENTER,
        "x",
        FontId::proportional(22.0),
        Color32::from_rgba_unmultiplied(230, 233, 240, 205),
    );

    let divider_y = rect.top() + 40.0;
    painter.line_segment(
        [egui::pos2(rect.left(), divider_y), egui::pos2(rect.right(), divider_y)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 18)),
    );

    let icon_y = if settings_open { rect.top() + 116.0 } else { rect.top() + 92.0 };
    if draw_side_icon(
        ui,
        egui::pos2(rect.left() + 34.0, icon_y),
        "S",
        "settings_btn",
        settings_open,
        "Settings",
    ) {
        result.toggle_settings = true;
    }
    if settings_open {
        result.selected_model = draw_settings_panel(ui, rect, selected_model);
    } else {
        let _ = draw_mic_button(ui, egui::pos2(rect.center().x, icon_y), status, audio_level);
    }
    if draw_side_icon(
        ui,
        egui::pos2(rect.right() - 34.0, icon_y),
        "?",
        "help_btn",
        false,
        "Help",
    ) {
        result.help_clicked = true;
    }

    let status_text = if settings_open {
        "Settings"
    } else {
        match status {
            AppStatus::Idle => "Ready",
            AppStatus::Recording => "Listening...",
            AppStatus::Transcribing => "Processing...",
        }
    };
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 14.0),
        Align2::CENTER_CENTER,
        status_text,
        FontId::proportional(12.0),
        Color32::from_rgba_unmultiplied(215, 220, 231, 180),
    );

    result
}

pub fn draw_side_icon(
    ui: &mut egui::Ui,
    center: egui::Pos2,
    icon: &str,
    id_source: &str,
    selected: bool,
    tooltip: &str,
) -> bool {
    let rect = egui::Rect::from_center_size(center, egui::vec2(28.0, 28.0));
    let response = ui.interact(rect, ui.id().with(id_source), Sense::click()).on_hover_text(tooltip);
    let painter = ui.painter();

    let stroke_color = if selected {
        Color32::from_rgb(205, 132, 255)
    } else if response.hovered() {
        Color32::from_rgba_unmultiplied(245, 247, 252, 255)
    } else {
        Color32::from_rgba_unmultiplied(245, 247, 252, 220)
    };

    if response.hovered() || selected {
        painter.circle_filled(
            center,
            12.5,
            if selected {
                Color32::from_rgba_unmultiplied(205, 132, 255, 40)
            } else {
                Color32::from_rgba_unmultiplied(255, 255, 255, 20)
            },
        );
    }

    painter.circle_stroke(center, 11.5, Stroke::new(1.5, stroke_color));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        icon,
        FontId::proportional(16.0),
        stroke_color,
    );

    response.clicked()
}

pub fn draw_settings_panel(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    selected_model: SpeechModel,
) -> Option<SpeechModel> {
    let normalized_model = selected_model.settings_choice();
    let panel_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 18.0, rect.top() + 54.0),
        egui::pos2(rect.right() - 18.0, rect.bottom() - 36.0),
    );
    let mut new_model = None;

    ui.allocate_ui_at_rect(panel_rect, |ui| {
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Speech Model")
                    .size(15.0)
                    .strong()
                    .color(Color32::from_rgb(239, 241, 246)),
            );
            ui.add_space(10.0);

            if model_choice_button(
                ui,
                "Chirp 3",
                "General dictation",
                normalized_model == SpeechModel::Chirp3,
            ) {
                new_model = Some(SpeechModel::Chirp3);
            }

            ui.add_space(8.0);

            if model_choice_button(
                ui,
                "Telephony",
                "Phone-call audio",
                normalized_model == SpeechModel::Telephony,
            ) {
                new_model = Some(SpeechModel::Telephony);
            }

            ui.add_space(8.0);

            if model_choice_button(
                ui,
                "Groq Whisper",
                "Whisper-Large-v3-Turbo",
                normalized_model == SpeechModel::GroqWhisper,
            ) {
                new_model = Some(SpeechModel::GroqWhisper);
            }

            ui.add_space(8.0);
            ui.label(
                RichText::new("Applies to the next recording")
                    .size(11.0)
                    .color(Color32::from_rgba_unmultiplied(215, 220, 231, 170)),
            );
        });
    });

    new_model
}

pub fn model_choice_button(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    selected: bool,
) -> bool {
    let label = format!("{}\n{}", title, subtitle);
    let fill = if selected {
        Color32::from_rgba_unmultiplied(205, 132, 255, 70)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 12)
    };
    let stroke = if selected {
        Stroke::new(1.2, Color32::from_rgb(205, 132, 255))
    } else {
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 22))
    };

    ui.add_sized(
        [170.0, 40.0],
        Button::new(
            RichText::new(label)
                .size(12.0)
                .color(Color32::from_rgb(239, 241, 246)),
        )
        .fill(fill)
        .stroke(stroke),
    )
    .clicked()
}

pub fn draw_mic_button(
    ui: &mut egui::Ui,
    center: egui::Pos2,
    status: AppStatus,
    audio_level: f32,
) -> egui::Response {
    let rect = egui::Rect::from_center_size(center, egui::vec2(70.0, 70.0));
    let response = ui.interact(rect, ui.id().with("mic_btn"), Sense::click()).on_hover_text("Press F11 to toggle dictation");
    let painter = ui.painter();

    if status == AppStatus::Recording {
        let glow_alpha = (70.0 + audio_level * 170.0).clamp(0.0, 255.0) as u8;
        let outer_alpha = (20.0 + audio_level * 90.0).clamp(0.0, 255.0) as u8;
        painter.circle_stroke(
            center,
            38.0 + audio_level * 7.0,
            Stroke::new(2.0, Color32::from_rgba_unmultiplied(205, 132, 255, glow_alpha)),
        );
        painter.circle_stroke(
            center,
            45.0 + audio_level * 10.0,
            Stroke::new(1.2, Color32::from_rgba_unmultiplied(205, 132, 255, outer_alpha)),
        );
    }

    painter.circle_filled(
        center,
        33.0,
        Color32::from_rgba_unmultiplied(52, 54, 60, 245),
    );
    painter.circle_stroke(
        center,
        33.0,
        Stroke::new(
            1.0,
            if response.hovered() {
                Color32::from_rgba_unmultiplied(255, 255, 255, 60)
            } else {
                Color32::from_rgba_unmultiplied(255, 255, 255, 28)
            },
        ),
    );

    let mic_color = match status {
        AppStatus::Recording => Color32::from_rgb(205, 132, 255),
        AppStatus::Transcribing => Color32::from_rgb(174, 177, 188),
        AppStatus::Idle => Color32::from_rgb(174, 177, 188),
    };

    let body =
        egui::Rect::from_center_size(egui::pos2(center.x, center.y - 2.0), egui::vec2(8.0, 12.0));
    painter.rect_stroke(body, egui::Rounding::same(3.0), Stroke::new(1.8, mic_color));
    painter.line_segment(
        [egui::pos2(center.x, center.y + 4.0), egui::pos2(center.x, center.y + 9.0)],
        Stroke::new(1.8, mic_color),
    );
    painter.line_segment(
        [
            egui::pos2(center.x - 5.0, center.y + 9.5),
            egui::pos2(center.x + 5.0, center.y + 9.5),
        ],
        Stroke::new(1.8, mic_color),
    );

    if status == AppStatus::Recording {
        let pattern = [0.22, 0.45, 0.72, 1.0, 0.72, 0.45, 0.22];
        let bar_count = pattern.len();
        let bar_width = 3.0;
        let spacing = 4.0;
        let total_width = bar_count as f32 * bar_width + (bar_count as f32 - 1.0) * spacing;
        let start_x = center.x - total_width * 0.5;
        let baseline_y = center.y + 26.0;
        let bar_color = Color32::from_rgba_unmultiplied(205, 132, 255, 220);

        for (idx, weight) in pattern.iter().enumerate() {
            let height = 2.0 + 13.0 * audio_level * *weight;
            let x = start_x + idx as f32 * (bar_width + spacing);
            let bar_rect = egui::Rect::from_min_max(
                egui::pos2(x, baseline_y - height),
                egui::pos2(x + bar_width, baseline_y),
            );
            painter.rect_filled(bar_rect, egui::Rounding::same(1.0), bar_color);
        }
    }

    response
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
