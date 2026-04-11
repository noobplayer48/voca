use std::sync::{Arc, RwLock, atomic::AtomicU32};
use crate::api::SpeechModel;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum AppStatus {
    Idle,
    Recording,
    Transcribing,
}

pub struct AppState {
    pub status: Arc<RwLock<AppStatus>>,
    pub speech_model: Arc<RwLock<SpeechModel>>,
    pub audio_level: Arc<AtomicU32>,
}

pub enum TranscriptionEventKind {
    Transcript { transcript: String },
    Completed,
    Failed(String),
}

pub struct TranscriptionEvent {
    pub session_id: u64,
    pub kind: TranscriptionEventKind,
}

pub struct PendingFallback {
    pub session_id: u64,
    pub wav_bytes: Vec<u8>,
    pub speech_model: SpeechModel,
}
