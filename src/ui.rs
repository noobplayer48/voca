#[path = "audio_bars_ui.rs"]
pub mod audio_bars_ui;

use crate::types::{AppStatus, TriggerEvent};
use eframe::egui;
use egui::{Color32, FontId, RichText, Sense, ViewportCommand};
use std::sync::{atomic::Ordering, Arc, RwLock, atomic::AtomicU32, mpsc};
use std::fs;
use crate::indicator_hwnd;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::Win32::Foundation::HWND;
use crate::UI_REPAINT_INTERVAL;
use crate::utils::{top_center_position, offscreen_position, move_indicator_to_current_virtual_desktop};

pub struct DictationIndicatorWrapper {
    pub status: Arc<RwLock<AppStatus>>,
    pub language: Arc<RwLock<String>>,
    pub audio_level: Arc<AtomicU32>,
    pub was_visible: bool,
    pub style_applied: bool,
    pub visibility_initialized: bool,
    pub settings_open: bool,
    pub _tray_icon: tray_icon::TrayIcon,
    pub tray_lang_hi: tray_icon::menu::CheckMenuItem,
    pub tray_lang_en: tray_icon::menu::CheckMenuItem,
    pub tray_quit_id: tray_icon::menu::MenuId,
    pub tray_settings_id: tray_icon::menu::MenuId,
    pub trigger_tx: mpsc::Sender<TriggerEvent>,
    pub twinkle_vad_state: audio_bars_ui::TwinkleVadState,
    pub ocr_triggered: Arc<std::sync::atomic::AtomicBool>,
    pub ocr_state: crate::ocr::OcrState,
    pub ocr_tx: mpsc::Sender<Result<String, String>>,
    pub ocr_rx: mpsc::Receiver<Result<String, String>>,
    pub show_settings_window: bool,
    pub groq_api_key_input: String,
    pub language_toast: Arc<RwLock<Option<(String, std::time::Instant)>>>,
}

impl eframe::App for DictationIndicatorWrapper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::TRANSPARENT;
        visuals.window_fill = Color32::TRANSPARENT;
        ctx.set_visuals(visuals);

        let _ = crate::EGUI_CONTEXT.set(ctx.clone());

        // Handle OCR Trigger — spawn a thread that does capture + selection + OCR
        if self.ocr_triggered.swap(false, Ordering::Relaxed) {
            let ocr_tx = self.ocr_tx.clone();
            let ctx_clone = ctx.clone();
            std::thread::spawn(move || {
                // run_ocr_capture blocks until user finishes or cancels selection
                let Some(cropped) = crate::ocr::run_ocr_capture() else {
                    return; // User cancelled
                };

                // Encode to PNG
                let mut png_bytes = Vec::new();
                let mut cursor = std::io::Cursor::new(&mut png_bytes);
                if let Err(e) = image::write_buffer_with_format(
                    &mut cursor,
                    &cropped,
                    cropped.width(),
                    cropped.height(),
                    image::ColorType::Rgba8,
                    image::ImageFormat::Png,
                ) {
                    let _ = ocr_tx.send(Err(format!("Encode error: {}", e)));
                    ctx_clone.request_repaint();
                    return;
                }

                let rt = match tokio::runtime::Runtime::new() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ocr_tx.send(Err(format!("Runtime error: {}", e)));
                        ctx_clone.request_repaint();
                        return;
                    }
                };

                match rt.block_on(crate::api::ocr_groq(png_bytes)) {
                    Ok(text) => {
                        let _ = ocr_tx.send(Ok(text));
                    }
                    Err(e) => {
                        let _ = ocr_tx.send(Err(e.to_string()));
                    }
                }
                ctx_clone.request_repaint();
            });
        }

        // Handle OCR completions
        while let Ok(res) = self.ocr_rx.try_recv() {
            match res {
                Ok(text) => {
                    ctx.output_mut(|o| o.copied_text = text);
                    self.ocr_state.status_message = Some(("Copied to clipboard!".to_string(), std::time::Instant::now()));
                }
                Err(err) => {
                    self.ocr_state.status_message = Some((format!("OCR failed: {}", err), std::time::Instant::now()));
                }
            }
        }

        // Clear status message after 3 seconds
        if let Some((_, time)) = &self.ocr_state.status_message {
            if time.elapsed() > std::time::Duration::from_secs(3) {
                self.ocr_state.status_message = None;
            }
        }

        let current_status = self
            .status
            .read()
            .map(|status| *status)
            .unwrap_or(AppStatus::Idle);
        let toast_active = self.language_toast.read()
            .map(|t| t.as_ref().map(|(_, when)| when.elapsed().as_secs_f32() < 1.5).unwrap_or(false))
            .unwrap_or(false);
        let should_show = current_status != AppStatus::Idle || toast_active;

        if should_show {
            ctx.request_repaint();
        }

        // Continuously enforce taskbar hiding (eframe/winit sometimes internally resets styles)
        apply_no_taskbar_style();

        if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id == self.tray_quit_id {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else if event.id == self.tray_settings_id {
                self.show_settings_window = true;
                ctx.request_repaint();
            } else if event.id == *self.tray_lang_hi.id() {
                if let Ok(mut lang) = self.language.write() {
                    *lang = "hi".to_string();
                }
                persist_selected_language("hi");
            } else if event.id == *self.tray_lang_en.id() {
                if let Ok(mut lang) = self.language.write() {
                    *lang = "en".to_string();
                }
                persist_selected_language("en");
            }
        }

        // Sync language tray items
        let synced_lang = self
            .language
            .read()
            .map(|l| l.clone())
            .unwrap_or_else(|_| "hi".to_string());
        let hi_checked = synced_lang == "hi";
        let en_checked = synced_lang == "en";
        if self.tray_lang_hi.is_checked() != hi_checked {
            self.tray_lang_hi.set_checked(hi_checked);
        }
        if self.tray_lang_en.is_checked() != en_checked {
            self.tray_lang_en.set_checked(en_checked);
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
                    let vad_score = self.audio_level.load(Ordering::Relaxed) as f32 / 1000.0;
                    static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                    let start = START_TIME.get_or_init(std::time::Instant::now);
                    let now = std::time::Instant::now()
                        .duration_since(*start)
                        .as_secs_f32();
                    let rect = ui.max_rect();
                    let response = ui.interact(rect, ui.id().with("window_drag"), Sense::click_and_drag());
                    if response.drag_started() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                    audio_bars_ui::draw_twinkle_vad(ui, rect, vad_score, now, &mut self.twinkle_vad_state);
                }

                // Language toast: show briefly after double-press toggle
                if let Ok(toast) = self.language_toast.read() {
                    if let Some((lang, when)) = toast.as_ref() {
                        let elapsed = when.elapsed().as_secs_f32();
                        if elapsed < 1.5 {
                            let alpha = if elapsed < 1.0 { 1.0 } else { 1.0 - (elapsed - 1.0) / 0.5 };
                            let a = (alpha * 255.0) as u8;
                            let label = if lang == "hi" { "Hindi" } else { "English" };
                            let center = ui.max_rect().center();
                            let text_pos = egui::Pos2::new(center.x, ui.max_rect().max.y - 16.0);
                            ui.painter().text(
                                text_pos,
                                egui::Align2::CENTER_CENTER,
                                label,
                                FontId::proportional(13.0),
                                Color32::from_rgba_unmultiplied(205, 132, 255, a),
                            );
                            ctx.request_repaint();
                        }
                    }
                }
            });

        // Settings Viewport
        if self.show_settings_window {
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("voca_settings_window"),
                egui::ViewportBuilder::default()
                    .with_title("Voca Settings")
                    .with_inner_size([400.0, 150.0])
                    .with_resizable(false)
                    .with_always_on_top(),
                |ctx, _class| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.heading("Settings");
                        ui.add_space(10.0);
                        
                        ui.horizontal(|ui| {
                            ui.label("Groq API Key:");
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut self.groq_api_key_input)
                                    .password(true)
                                    .desired_width(250.0)
                            );
                            
                            // Try to paste from clipboard if right clicked (since typical right click menu doesn't exist by default)
                            if response.secondary_clicked() {
                                if let Some(clipboard) = ctx.output(|o| Some(o.copied_text.clone())) {
                                    if !clipboard.is_empty() {
                                         self.groq_api_key_input = clipboard;
                                    }
                                }
                            }
                        });
                        
                        ui.add_space(10.0);
                        ui.label(RichText::new("The API key is saved locally in voca-groq-api-key.txt").color(Color32::GRAY).size(11.0));
                        ui.add_space(15.0);

                        ui.horizontal(|ui| {
                            if ui.button("Save & Close").clicked() {
                                let _ = std::fs::write("voca-groq-api-key.txt", self.groq_api_key_input.trim());
                                self.show_settings_window = false;
                            }
                            if ui.button("Cancel").clicked() {
                                self.show_settings_window = false;
                            }
                        });
                    });

                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.show_settings_window = false;
                    }
                },
            );
        }

        ctx.request_repaint_after(UI_REPAINT_INTERVAL);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }
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


pub fn persist_selected_language(lang: &str) {
    if let Err(e) = fs::write("voca-language.txt", format!("{}\n", lang)) {
        eprintln!(
            "Warning: failed to save language to voca-language.txt: {}",
            e
        );
    }
}
