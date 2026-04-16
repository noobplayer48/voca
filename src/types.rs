use crate::api::SpeechModel;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum AppStatus {
    Idle,
    Recording,
    Transcribing,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TriggerEvent {
    Transcribe,
    Translate,
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
    pub is_translation: bool,
}
