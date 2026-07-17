

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
    Ocr,
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


