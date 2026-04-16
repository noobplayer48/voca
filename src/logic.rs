use crate::api::{self, SpeechModel, StreamingTranscriptUpdate};
use crate::audio::AudioRecorder;
use crate::types::{AppStatus, TranscriptionEvent, TranscriptionEventKind, PendingFallback, TriggerEvent};
use enigo::{Enigo, KeyboardControllable};
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
    audio_level: Arc<AtomicU32>,
    gcp_project_id: String,
    speech_region: String,
) {
    thread::spawn(move || {
        let initial_model = *speech_model_state.read().unwrap();
        let mut recorder = AudioRecorder::new(audio_level.clone(), initial_model.preferred_sample_rate_hz());
        recorder.set_vad_trigger(trigger_tx);
        let mut current_session_model = initial_model;
        let mut is_recording = false;
        let mut enigo = Enigo::new();
        let (transcript_tx, transcript_rx) = mpsc::channel::<TranscriptionEvent>();
        let mut typed_words: Vec<String> = Vec::new();
        let mut last_hotkey_press = Instant::now() - HOTKEY_DEBOUNCE;
        let mut active_session_id: u64 = 0;
        let mut session_is_translation: bool = false;
        let mut transcription_inflight_for: Option<u64> = None;
        let mut stream_failed_for: Option<u64> = None;
        let mut pending_fallback: Option<PendingFallback> = None;

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
                        append_new_words(&mut enigo, &mut typed_words, &transcript);
                    }
                    TranscriptionEventKind::Completed => {
                        transcription_inflight_for = None;
                        pending_fallback = None;

                        if is_recording {
                            eprintln!("[-] Streaming session closed before recording stopped. Falling back when you stop recording.");
                            stream_failed_for = Some(event.session_id);
                            continue;
                        }

                        stream_failed_for = None;
                        reset_app_state(&mut typed_words, &status);
                    }
                    TranscriptionEventKind::Failed(error) => {
                        eprintln!("[-] Streaming Error: {}", error);
                        transcription_inflight_for = None;
                        stream_failed_for = Some(event.session_id);

                        if is_recording {
                            continue;
                        }

                        if let Some(fallback) = pending_fallback
                            .take()
                            .filter(|fallback| fallback.session_id == event.session_id)
                        {
                            println!("[*] Streaming failed after stop. Falling back to full-file transcription...");
                            transcription_inflight_for = Some(event.session_id);
                            stream_failed_for = None;
                            spawn_fallback_transcription_task(
                                event.session_id,
                                fallback.wav_bytes,
                                gcp_project_id.clone(),
                                speech_region.clone(),
                                fallback.speech_model,
                                transcript_tx.clone(),
                            );
                        } else {
                            reset_app_state(&mut typed_words, &status);
                        }
                    }
                }
            }

            match trigger_rx.recv_timeout(HOTKEY_POLL_INTERVAL) {
                Ok(trigger_event) => {
                    if last_hotkey_press.elapsed() < HOTKEY_DEBOUNCE {
                        continue;
                    }
                    last_hotkey_press = Instant::now();

                    if !is_recording {
                        if transcription_inflight_for.is_some() {
                            println!("[*] Still processing previous recording...");
                            continue;
                        }

                        current_session_model = speech_model_state
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
                        let (audio_chunk_tx, audio_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
                        if let Err(e) = recorder.start_streaming(audio_chunk_tx) {
                            eprintln!("Failed to start recording: {}", e);
                        } else {
                            active_session_id = active_session_id.wrapping_add(1);
                            is_recording = true;
                            transcription_inflight_for = Some(active_session_id);
                            stream_failed_for = None;
                            pending_fallback = None;
                            typed_words.clear();
                            let sample_rate_hz = recorder
                                .sample_rate_hz()
                                .unwrap_or(current_session_model.preferred_sample_rate_hz());
                            println!("[*] Live stream sample rate: {} Hz", sample_rate_hz);
                            
                            if current_session_model == SpeechModel::GroqWhisper || session_is_translation {
                                // Groq doesn't stream audio natively over gRPC like GCP. 
                                // Also, translation currently uses full-file fallback.
                                stream_failed_for = Some(active_session_id);
                            } else {
                                spawn_streaming_session(
                                    active_session_id,
                                    audio_chunk_rx,
                                    gcp_project_id.clone(),
                                    speech_region.clone(),
                                    current_session_model,
                                    sample_rate_hz,
                                    transcript_tx.clone(),
                                );
                            }
                            if let Ok(mut s) = status.write() {
                                *s = AppStatus::Recording;
                            }
                        }
                    } else {
                        println!("[*] Stopped recording. Finalizing live stream...");
                        is_recording = false;

                        if let Ok(mut s) = status.write() {
                            *s = AppStatus::Transcribing;
                        }

                        match recorder.stop() {
                            Ok(wav_bytes) => {
                                if stream_failed_for == Some(active_session_id)
                                    || transcription_inflight_for != Some(active_session_id)
                                {
                                    println!("[*] Live streaming was unavailable. Using full-file fallback...");
                                    transcription_inflight_for = Some(active_session_id);
                                    stream_failed_for = None;
                                    if current_session_model == SpeechModel::GroqWhisper || session_is_translation {
                                        if session_is_translation {
                                            spawn_groq_translate_task(
                                                active_session_id,
                                                wav_bytes,
                                                transcript_tx.clone(),
                                            );
                                        } else {
                                            spawn_groq_fallback_task(
                                                active_session_id,
                                                wav_bytes,
                                                transcript_tx.clone(),
                                            );
                                        }
                                    } else {
                                        spawn_fallback_transcription_task(
                                            active_session_id,
                                            wav_bytes,
                                            gcp_project_id.clone(),
                                            speech_region.clone(),
                                            current_session_model,
                                            transcript_tx.clone(),
                                        );
                                    }
                                } else {
                                    pending_fallback = Some(PendingFallback {
                                        session_id: active_session_id,
                                        wav_bytes,
                                        speech_model: current_session_model,
                                        is_translation: session_is_translation,
                                    });
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to stop recording: {}", e);
                                transcription_inflight_for = None;
                                stream_failed_for = None;
                                pending_fallback = None;
                                reset_app_state(&mut typed_words, &status);
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
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

fn append_new_words(enigo: &mut Enigo, typed_words: &mut Vec<String>, transcript: &str) {
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
        enigo.key_sequence(&text);
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
    enigo.key_sequence(&to_type);
    typed_words.extend(new_words.iter().cloned());
}

fn spawn_streaming_session(
    session_id: u64,
    audio_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    project_id: String,
    speech_region: String,
    speech_model: SpeechModel,
    sample_rate_hz: u32,
    tx: mpsc::Sender<TranscriptionEvent>,
) {
    thread::spawn(move || {
        let stream_result = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(api::stream_transcribe(
                audio_rx,
                &project_id,
                &speech_region,
                speech_model,
                sample_rate_hz,
                |update: StreamingTranscriptUpdate| {
                    tx.send(TranscriptionEvent {
                        session_id,
                        kind: TranscriptionEventKind::Transcript {
                            transcript: update.transcript,
                        },
                    })
                    .map_err(|error| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            format!("Failed to deliver streaming transcript update: {}", error),
                        )
                        .into()
                    })
                },
            )),
            Err(e) => Err(format!("Failed to create async runtime: {}", e).into()),
        };

        let kind = match stream_result {
            Ok(()) => TranscriptionEventKind::Completed,
            Err(error) => TranscriptionEventKind::Failed(error.to_string()),
        };

        let _ = tx.send(TranscriptionEvent { session_id, kind });
    });
}

fn spawn_fallback_transcription_task(
    session_id: u64,
    wav_bytes: Vec<u8>,
    project_id: String,
    speech_region: String,
    speech_model: SpeechModel,
    tx: mpsc::Sender<TranscriptionEvent>,
) {
    thread::spawn(move || {
        let transcript = match tokio::runtime::Runtime::new() {
            Ok(rt) => match rt.block_on(api::transcribe(
                wav_bytes,
                &project_id,
                &speech_region,
                speech_model,
            )) {
                Ok(text) => Ok(text),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(format!("Failed to create async runtime: {}", e)),
        };

        match transcript {
            Ok(text) => {
                let _ = tx.send(TranscriptionEvent {
                    session_id,
                    kind: TranscriptionEventKind::Transcript {
                        transcript: text,
                    },
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

fn spawn_groq_fallback_task(
    session_id: u64,
    wav_bytes: Vec<u8>,
    tx: mpsc::Sender<TranscriptionEvent>,
) {
    thread::spawn(move || {
        let transcript = match tokio::runtime::Runtime::new() {
            Ok(rt) => match rt.block_on(api::transcribe_groq(wav_bytes)) {
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


