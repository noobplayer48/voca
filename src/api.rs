use base64::{
    engine::general_purpose::STANDARD as BASE64,
    Engine as _,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechModel {
    // Groq Whisper
    GroqWhisper,
}

impl Default for SpeechModel {
    fn default() -> Self {
        Self::GroqWhisper
    }
}

impl SpeechModel {
    pub fn parse(value: &str) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase().replace("-", "_");
        match normalized.as_str() {
            "groq" | "groq_whisper" | "groq whisper" => Ok(Self::GroqWhisper),
            other => Err(format!("Unsupported speech model `{}`", other)),
        }
    }

    pub fn api_name(self) -> &'static str {
        match self {
            Self::GroqWhisper => "whisper-large-v3",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::GroqWhisper => "Groq Whisper",
        }
    }

    pub fn settings_choice(self) -> Self {
        self
    }

    pub fn preferred_sample_rate_hz(self) -> u32 {
        match self {
            Self::GroqWhisper => 16_000,
        }
    }
}

fn shared_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .use_rustls_tls()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

fn other_error(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::Other, message.into()).into()
}

pub fn get_groq_api_key() -> Result<String, BoxError> {
    if let Ok(key) = std::fs::read_to_string("voca-groq-api-key.txt") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    Err(other_error("Groq API Key not found. Please set it in the Settings menu."))
}

pub async fn transcribe_groq(
    audio_bytes: Vec<u8>,
    lang: &str,
) -> Result<String, BoxError> {
    let api_key = get_groq_api_key()?;
    
    if api_key.is_empty() {
        return Err(other_error("Groq API Key is empty"));
    }

    let file_part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", "whisper-large-v3")
        .text("response_format", "json");

    if lang == "hi" {
        form = form.text("language", "hi")
            .text("prompt", "यह ऑडियो हिंदी में है।");
    } else if lang == "en" {
        form = form.text("language", "en")
            .text("prompt", "This audio is in English script.");
    }

    let res = shared_client()
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(other_error(format!(
            "Groq API Error {}: {}",
            res.status(),
            res.text().await?
        )));
    }

    let json_body: Value = res.json().await?;
    if let Some(text) = json_body.get("text").and_then(|t| t.as_str()) {
        Ok(text.trim().to_string())
    } else {
        Ok(String::new())
    }
}

pub async fn translate_groq(
    audio_bytes: Vec<u8>,
) -> Result<String, BoxError> {
    let api_key = get_groq_api_key()?;
    
    if api_key.is_empty() {
        return Err(other_error("Groq API Key is empty"));
    }

    let file_part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", "whisper-large-v3")
        .text("response_format", "json");

    let res = shared_client()
        .post("https://api.groq.com/openai/v1/audio/translations")
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(other_error(format!(
            "Groq Translation API Error {}: {}",
            res.status(),
            res.text().await?
        )));
    }

    let json_body: Value = res.json().await?;
    if let Some(text) = json_body.get("text").and_then(|t| t.as_str()) {
        Ok(text.trim().to_string())
    } else {
        Ok(String::new())
    }
}

pub async fn ocr_groq(
    image_png_bytes: Vec<u8>,
) -> Result<String, BoxError> {
    let api_key = get_groq_api_key()?;

    if api_key.is_empty() {
        return Err(other_error("Groq API Key is empty"));
    }

    let base64_image = BASE64.encode(&image_png_bytes);
    let image_data_url = format!("data:image/png;base64,{}", base64_image);

    let payload = json!({
        "model": "meta-llama/llama-4-scout-17b-16e-instruct",
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "Read all the text present in the image and transcribe it accurately. Output ONLY the transcribed text, maintaining its layout if possible. Do NOT include any explanations, greetings, warnings, or markdown code-block wrapping. Just output the raw transcribed characters."
                    },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": image_data_url
                        }
                    }
                ]
            }
        ]
    });

    let res = shared_client()
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(other_error(format!(
            "Groq OCR API Error {}: {}",
            res.status(),
            res.text().await?
        )));
    }

    let json_body: Value = res.json().await?;
    if let Some(text) = json_body.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str()) {
        Ok(text.trim().to_string())
    } else {
        Ok(String::new())
    }
}
