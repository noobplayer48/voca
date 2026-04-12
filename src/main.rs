#![windows_subsystem = "windows"]

mod api;
mod audio;
mod hook;
mod types;
mod logic;
mod ui;
mod utils;

use crate::api::SpeechModel;
use crate::types::AppStatus;
use crate::ui::DictationIndicatorWrapper;
use dotenv::dotenv;
use eframe::egui;
use std::env;
use std::fs;
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
pub const SPEECH_MODEL_SETTINGS_FILE: &str = "voca-speech-model.txt";
pub const DEFAULT_ASIA_SPEECH_REGION: &str = "asia-southeast1";

fn main() -> Result<(), eframe::Error> {
    dotenv().ok();
    
    let gcp_project_id = match env::var("GCP_PROJECT_ID") {
        Ok(val) => val,
        Err(_) => {
            eprintln!("Error: GCP_PROJECT_ID environment variable is not set.");
            std::thread::sleep(std::time::Duration::from_secs(5));
            return Ok(());
        }
    };

    if env::var("GROQ_API_KEY").is_err() {
        eprintln!("Warning: GROQ_API_KEY is not set. Groq Whisper features will be unavailable.");
    }
    let speech_model = load_selected_speech_model();
    let speech_region = env::var("GCP_SPEECH_REGION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ASIA_SPEECH_REGION.to_string());

    println!("========================================");
    println!("Voca Dictation Service Running!      ");
    println!("Press F11 to toggle dictation ON/OFF.   ");
    println!("Speech model: {}", speech_model.display_name());
    println!("Speech transport: bidirectional gRPC streaming");
    println!("Speech region: {}", speech_region);
    println!("========================================\n");

    let status = Arc::new(RwLock::new(AppStatus::Idle));
    let speech_model_state = Arc::new(RwLock::new(speech_model));
    let audio_level = Arc::new(AtomicU32::new(0));

    // 1. Start the Windows hook loop in a separate thread.
    let (tx, rx) = mpsc::channel();
    hook::set_trigger_sender(tx.clone());
    thread::spawn(|| {
        hook::start_hook_loop();
    });

    // 2. Start dictation logic thread.
    logic::start_logic_thread(
        rx,
        status.clone(),
        speech_model_state.clone(),
        audio_level.clone(),
        gcp_project_id,
        speech_region,
    );

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
    
    let models_to_show = vec![
        SpeechModel::GroqWhisper,
        SpeechModel::Telephony,
        SpeechModel::Chirp3,
    ];

    let mut tray_models = Vec::new();
    for m in models_to_show {
        let is_selected = m == speech_model;
        let item = tray_icon::menu::CheckMenuItem::new(m.display_name(), true, is_selected, None);
        let _ = tray_menu.append(&item);
        tray_models.push((m, item));
    }

    let _ = tray_menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    let quit_i = tray_icon::menu::MenuItem::new("Quit Voca", true, None);
    let tray_quit_id = quit_i.id().clone();
    let _ = tray_menu.append(&quit_i);

    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..(16 * 16) {
        rgba.extend_from_slice(&[205, 132, 255, 255]);
    }
    let tray_icon = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Voca Dictation")
        .with_icon(tray_icon::Icon::from_rgba(rgba, 16, 16).unwrap())
        .build()
        .unwrap();

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |_cc| {
            Box::new(DictationIndicatorWrapper {
                status: status.clone(),
                speech_model: speech_model_state.clone(),
                audio_level: audio_level.clone(),
                was_visible: false,
                style_applied: false,
                visibility_initialized: false,
                settings_open: false,
                _tray_icon: tray_icon,
                tray_models,
                tray_quit_id,
                trigger_tx: tx.clone(),
                icon_scale: 1.0,
                wave_scale1: 0.5,
                wave_scale2: 0.5,
            })
        }),
    )
}

fn load_selected_speech_model() -> SpeechModel {
    if let Ok(saved_value) = fs::read_to_string(SPEECH_MODEL_SETTINGS_FILE) {
        match SpeechModel::parse(&saved_value) {
            Ok(model) => return model.settings_choice(),
            Err(e) => eprintln!(
                "Warning: failed to parse {}: {}. Falling back to environment/default.",
                SPEECH_MODEL_SETTINGS_FILE, e
            ),
        }
    }

    match env::var("GCP_SPEECH_MODEL") {
        Ok(value) => match SpeechModel::parse(&value) {
            Ok(model) => model.settings_choice(),
            Err(e) => {
                eprintln!("Warning: {}. Falling back to Groq Whisper.", e);
                SpeechModel::default()
            }
        },
        Err(_) => SpeechModel::default(),
    }
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
