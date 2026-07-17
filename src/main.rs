#![windows_subsystem = "windows"]

mod api;
mod audio;
mod hook;
mod types;
mod logic;
mod ui;
mod utils;
mod ocr;

pub static EGUI_CONTEXT: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

use crate::types::AppStatus;
use crate::ui::DictationIndicatorWrapper;

use eframe::egui;
use image::GenericImageView;
use std::sync::{
    atomic::AtomicU32,
    mpsc,
    Arc, RwLock,
};
use std::thread;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

pub const WINDOW_TITLE: &str = "Voca Indicator";
pub const INDICATOR_WIDTH: f32 = 180.0;
pub const INDICATOR_HEIGHT: f32 = 140.0;
pub const INDICATOR_TOP_MARGIN: f32 = 50.0;
pub const UI_REPAINT_INTERVAL: Duration = Duration::from_millis(80);
pub const DEFAULT_ASIA_SPEECH_REGION: &str = "asia-southeast1";

fn main() -> Result<(), eframe::Error> {
    let selected_language = load_selected_language();

    println!("========================================");
    println!("Voca Dictation Service Running!      ");
    println!("Press F11 to toggle dictation ON/OFF.   ");
    println!("Speech model: Groq Whisper");
    println!("========================================\n");

    let status = Arc::new(RwLock::new(AppStatus::Idle));
    let language_state = Arc::new(RwLock::new(selected_language.clone()));
    let audio_level = Arc::new(AtomicU32::new(0));

    use crate::types::TriggerEvent;
    // 1. Start the Windows hook loop in a separate thread.
    let (tx, rx) = mpsc::channel::<TriggerEvent>();
    hook::set_trigger_sender(tx.clone());
    thread::spawn(|| {
        if let Err(e) = hook::start_hook_loop() {
            eprintln!("Error in Windows hook loop: {:?}", e);
        }
    });

    let ocr_triggered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let language_toast: Arc<std::sync::RwLock<Option<(String, std::time::Instant)>>> =
        Arc::new(std::sync::RwLock::new(None));

    // 2. Start dictation logic thread.
    logic::start_logic_thread(
        rx,
        tx.clone(),
        status.clone(),
        language_state.clone(),
        audio_level.clone(),
        ocr_triggered.clone(),
        language_toast.clone(),
    );

    // Remove the separate VAD thread — VAD is now inside AudioRecorder.

    // 3. Start UI window on main thread.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_always_on_top()
            .with_decorations(false)
            .with_transparent(true)
            .with_mouse_passthrough(false)
            .with_resizable(false)
            .with_inner_size([INDICATOR_WIDTH, INDICATOR_HEIGHT])
            .with_min_inner_size([INDICATOR_WIDTH, INDICATOR_HEIGHT])
            .with_max_inner_size([INDICATOR_WIDTH, INDICATOR_HEIGHT])
            .with_visible(true),
        persist_window: false,
        ..Default::default()
    };

    let tray_menu = tray_icon::menu::Menu::new();

    // Language selection items
    let lang_hi_item = tray_icon::menu::CheckMenuItem::new("Hindi (hi)", true, selected_language == "hi", None);
    let lang_en_item = tray_icon::menu::CheckMenuItem::new("English (en)", true, selected_language == "en", None);
    let _ = tray_menu.append(&lang_hi_item);
    let _ = tray_menu.append(&lang_en_item);

    let _ = tray_menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    let settings_i = tray_icon::menu::MenuItem::new("Settings", true, None);
    let tray_settings_id = settings_i.id().clone();
    let _ = tray_menu.append(&settings_i);
    
    let quit_i = tray_icon::menu::MenuItem::new("Quit Voca", true, None);
    let tray_quit_id = quit_i.id().clone();
    let _ = tray_menu.append(&quit_i);

    let tray_icon = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Voca Dictation")
        .with_icon(load_icon_from_file())
        .build()
        .unwrap();

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |_cc| {
            let ocr_triggered_captured = ocr_triggered.clone();
            let (ocr_tx, ocr_rx) = std::sync::mpsc::channel();
            Box::new(DictationIndicatorWrapper {
                status: status.clone(),
                language: language_state.clone(),
                audio_level: audio_level.clone(),
                was_visible: false,
                style_applied: false,
                visibility_initialized: false,
                settings_open: false,
                _tray_icon: tray_icon,
                tray_lang_hi: lang_hi_item,
                tray_lang_en: lang_en_item,
                tray_quit_id,
                tray_settings_id,
                trigger_tx: tx.clone(),
                twinkle_vad_state: ui::audio_bars_ui::TwinkleVadState::default(),
                show_settings_window: false,
                groq_api_key_input: std::fs::read_to_string("voca-groq-api-key.txt").unwrap_or_default().trim().to_string(),
                ocr_triggered: ocr_triggered_captured,
                ocr_state: crate::ocr::OcrState::default(),
                ocr_tx,
                ocr_rx,
                language_toast: language_toast.clone(),
            })
        }),
    )
}

fn load_selected_language() -> String {
    if let Ok(lang) = std::fs::read_to_string("voca-language.txt") {
        let lang = lang.trim().to_ascii_lowercase();
        if lang == "en" {
            return "en".to_string();
        }
    }
    "hi".to_string()
}

pub fn indicator_hwnd() -> Option<HWND> {
    unsafe {
        let title_wide: Vec<u16> = WINDOW_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = FindWindowW(None, PCWSTR(title_wide.as_ptr()));
        if hwnd.0 == 0 {
            None
        } else {
            Some(hwnd)
        }
    }
}

fn load_icon_from_file() -> tray_icon::Icon {
    let icon_data = include_bytes!("../wave-sound (1).png");
    if let Ok(mut img) = image::load_from_memory(icon_data) {
        // Trim transparency to make the icon look "bigger" in the tray
        let (width, height) = img.dimensions();
        let mut min_x = width;
        let mut max_x = 0;
        let mut min_y = height;
        let mut max_y = 0;

        for (x, y, pixel) in img.pixels() {
            if pixel[3] > 10 { // Significant opacity
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }

        let trimmed = if max_x >= min_x && max_y >= min_y {
            let trim_width = max_x - min_x + 1;
            let trim_height = max_y - min_y + 1;
            img.crop(min_x, min_y, trim_width, trim_height)
        } else {
            img
        };

        let (w, h) = trimmed.dimensions();
        let rgba = trimmed.to_rgba8().into_raw();
        tray_icon::Icon::from_rgba(rgba, w, h).unwrap_or_else(|_| fallback_icon())
    } else {
        fallback_icon()
    }
}

fn fallback_icon() -> tray_icon::Icon {
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..(16 * 16) {
        rgba.extend_from_slice(&[205, 132, 255, 255]);
    }
    tray_icon::Icon::from_rgba(rgba, 16, 16).unwrap()
}
