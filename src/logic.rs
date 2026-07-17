use crate::api::{self, SpeechModel};
use crate::audio::AudioRecorder;
use crate::types::{AppStatus, TranscriptionEvent, TranscriptionEventKind, TriggerEvent};

use std::sync::{Arc, RwLock, atomic::AtomicU32, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(220);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(30);

pub fn start_logic_thread(
    trigger_rx: mpsc::Receiver<TriggerEvent>,
    trigger_tx: mpsc::Sender<TriggerEvent>,
    status: Arc<RwLock<AppStatus>>,
    speech_model_state: Arc<RwLock<SpeechModel>>,
    language_state: Arc<RwLock<String>>,
    audio_level: Arc<AtomicU32>,
    ocr_triggered: Arc<std::sync::atomic::AtomicBool>,
    language_toast: Arc<RwLock<Option<(String, Instant)>>>,
) {
    thread::spawn(move || {
        let initial_model = *speech_model_state.read().unwrap();
        let mut recorder = AudioRecorder::new(audio_level.clone(), initial_model.preferred_sample_rate_hz());
        recorder.set_vad_trigger(trigger_tx);
        let mut is_recording = false;
        let mut recording_start_time = Instant::now();

        // Store VAD score into audio_level so the UI can read it for the twinkle animation
        let vad_audio_level = audio_level.clone();

        let (transcript_tx, transcript_rx) = mpsc::channel::<TranscriptionEvent>();
        let mut typed_words: Vec<String> = Vec::new();
        let mut last_hotkey_press = Instant::now() - HOTKEY_DEBOUNCE;
        let mut active_session_id: u64 = 0;
        let mut session_is_translation: bool = false;
        let mut transcription_inflight_for: Option<u64> = None;

        loop {
            while let Ok(event) = transcript_rx.try_recv() {
                if event.session_id != active_session_id {
                    continue;
                }

                match event.kind {
                    TranscriptionEventKind::Transcript { transcript } => {
                        if transcript.is_empty() {
                            continue;
                        }
                        append_new_words(&mut typed_words, &transcript);
                    }
                    TranscriptionEventKind::Completed => {
                        transcription_inflight_for = None;
                        reset_app_state(&mut typed_words, &status);
                    }
                    TranscriptionEventKind::Failed(error) => {
                        eprintln!("[-] Transcription Error: {}", error);
                        transcription_inflight_for = None;
                        reset_app_state(&mut typed_words, &status);
                    }
                }
            }

            match trigger_rx.recv_timeout(HOTKEY_POLL_INTERVAL) {
                Ok(trigger_event) => {
                    if trigger_event == TriggerEvent::Ocr {
                        ocr_triggered.store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Some(ctx) = crate::EGUI_CONTEXT.get() {
                            ctx.request_repaint();
                        }
                        continue;
                    }

                    if last_hotkey_press.elapsed() < HOTKEY_DEBOUNCE {
                        continue;
                    }
                    last_hotkey_press = Instant::now();

                    if !is_recording {
                        if transcription_inflight_for.is_some() {
                            println!("[*] Still processing previous recording...");
                            continue;
                        }

                        let current_session_model = speech_model_state
                            .read()
                            .map(|model| *model)
                            .unwrap_or_default()
                            .settings_choice();
                        recorder.set_preferred_sample_rate_hz(
                            current_session_model.preferred_sample_rate_hz(),
                        );
                        session_is_translation = trigger_event == TriggerEvent::Translate;
                        println!("[*] Started recording ({:?})...", trigger_event);
                        println!("[*] Using speech model: {}", current_session_model.api_name());
                        
                        let (audio_chunk_tx, _audio_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
                        if let Err(e) = recorder.start_streaming(audio_chunk_tx) {
                            eprintln!("Failed to start recording: {}", e);
                        } else {
                            active_session_id = active_session_id.wrapping_add(1);
                            is_recording = true;
                            recording_start_time = Instant::now();
                            transcription_inflight_for = Some(active_session_id);
                            typed_words.clear();
                            let sample_rate_hz = recorder
                                .sample_rate_hz()
                                .unwrap_or(current_session_model.preferred_sample_rate_hz());
                            println!("[*] Live stream sample rate: {} Hz", sample_rate_hz);
                            
                            if let Ok(mut s) = status.write() {
                                *s = AppStatus::Recording;
                            }
                        }
                    } else {
                        // Check if this is a quick double press to toggle language
                        if recording_start_time.elapsed() < Duration::from_millis(500) {
                            let mut new_lang = String::new();
                            if let Ok(mut lang) = language_state.write() {
                                if *lang == "hi" {
                                    *lang = "en".to_string();
                                } else {
                                    *lang = "hi".to_string();
                                }
                                new_lang = lang.clone();
                            }
                            if !new_lang.is_empty() {
                                crate::ui::persist_selected_language(&new_lang);
                                if let Ok(mut toast) = language_toast.write() {
                                    *toast = Some((new_lang.clone(), Instant::now()));
                                }
                                println!("[*] Quick double-press detected: Toggled language to {}", new_lang);
                            }
                            
                            // Stop old recording, discard audio
                            let _ = recorder.stop();
                            
                            // Immediately restart recording with new language
                            let current_session_model = speech_model_state
                                .read()
                                .map(|model| *model)
                                .unwrap_or_default()
                                .settings_choice();
                            recorder.set_preferred_sample_rate_hz(
                                current_session_model.preferred_sample_rate_hz(),
                            );
                            session_is_translation = false; // toggling language is always for transcription
                            println!("[*] Restarting recording with language: {}", new_lang);
                            
                            let (audio_chunk_tx, _audio_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
                            if let Err(e) = recorder.start_streaming(audio_chunk_tx) {
                                eprintln!("Failed to restart recording: {}", e);
                                is_recording = false;
                                transcription_inflight_for = None;
                                reset_app_state(&mut typed_words, &status);
                            } else {
                                active_session_id = active_session_id.wrapping_add(1);
                                is_recording = true;
                                recording_start_time = Instant::now();
                                transcription_inflight_for = Some(active_session_id);
                                typed_words.clear();
                                // Status stays Recording — no flicker
                            }
                        } else {
                            println!("[*] Stopped recording. Processing transcription...");
                            is_recording = false;

                            if let Ok(mut s) = status.write() {
                                *s = AppStatus::Transcribing;
                            }

                            match recorder.stop() {
                                Ok(wav_bytes) => {
                                    if session_is_translation {
                                        spawn_groq_translate_task(
                                            active_session_id,
                                            wav_bytes,
                                            transcript_tx.clone(),
                                        );
                                    } else {
                                        let current_lang = language_state.read().unwrap().clone();
                                        spawn_groq_transcribe_task(
                                            active_session_id,
                                            wav_bytes,
                                            current_lang,
                                            transcript_tx.clone(),
                                        );
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Failed to stop recording: {}", e);
                                    transcription_inflight_for = None;
                                    reset_app_state(&mut typed_words, &status);
                                }
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }

            // Update audio_level with VAD score for UI twinkle animation
            if is_recording {
                vad_audio_level.store((recorder.get_vad_score() * 1000.0) as u32, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
}

fn reset_app_state(typed_words: &mut Vec<String>, status: &RwLock<AppStatus>) {
    typed_words.clear();
    if let Ok(mut s) = status.write() {
        *s = AppStatus::Idle;
    }
}

fn append_new_words(typed_words: &mut Vec<String>, transcript: &str) {
    let recognized_words: Vec<String> = transcript
        .split_whitespace()
        .map(|word| word.trim().to_string())
        .filter(|word| !word.is_empty())
        .collect();

    if recognized_words.is_empty() {
        return;
    }

    if typed_words.is_empty() {
        let mut text = recognized_words.join(" ");
        text.push(' ');
        paste_via_send_input(&text);
        *typed_words = recognized_words;
        return;
    }

    let mut overlap = typed_words.len().min(recognized_words.len());
    while overlap > 0 {
        if typed_words[typed_words.len() - overlap..] == recognized_words[..overlap] {
            break;
        }
        overlap -= 1;
    }

    if overlap == 0 {
        if recognized_words.len() > typed_words.len() {
            overlap = typed_words.len();
        } else {
            return;
        }
    }

    let new_words = &recognized_words[overlap..];
    if new_words.is_empty() {
        return;
    }

    let mut to_type = new_words.join(" ");
    to_type.push(' ');
    paste_via_send_input(&to_type);
    typed_words.extend(new_words.iter().cloned());
}

fn paste_via_send_input(text: &str) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VIRTUAL_KEY,
    };
    use std::mem::size_of;

    let utf16: Vec<u16> = text.encode_utf16().collect();
    if utf16.is_empty() {
        return;
    }

    let mut inputs = Vec::with_capacity(utf16.len() * 2);

    for &code in &utf16 {
        // Key down
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: code,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        
        // Key up
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: code,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn spawn_groq_transcribe_task(
    session_id: u64,
    wav_bytes: Vec<u8>,
    lang: String,
    tx: mpsc::Sender<TranscriptionEvent>,
) {
    thread::spawn(move || {
        let transcript = match tokio::runtime::Runtime::new() {
            Ok(rt) => match rt.block_on(api::transcribe_groq(wav_bytes, &lang)) {
                Ok(text) => Ok(text),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(format!("Failed to create async runtime: {}", e)),
        };

        match transcript {
            Ok(text) => {
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Transcript { transcript: text },
                });
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Completed,
                });
            }
            Err(error) => {
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Failed(error),
                });
            }
        }
    });
}

fn spawn_groq_translate_task(
    session_id: u64,
    wav_bytes: Vec<u8>,
    tx: mpsc::Sender<TranscriptionEvent>,
) {
    thread::spawn(move || {
        let transcript = match tokio::runtime::Runtime::new() {
            Ok(rt) => match rt.block_on(api::translate_groq(wav_bytes)) {
                Ok(text) => Ok(text),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(format!("Failed to create async runtime: {}", e)),
        };

        match transcript {
            Ok(text) => {
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Transcript { transcript: text },
                });
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Completed,
                });
            }
            Err(error) => {
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Failed(error),
                });
            }
        }
    });
}
