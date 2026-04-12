use async_stream::stream;
use base64::{
    engine::general_purpose::STANDARD as BASE64,
    engine::general_purpose::URL_SAFE_NO_PAD,
    Engine as _,
};
use prost::Message;
use reqwest::Client;
use rsa::{pkcs8::DecodePrivateKey, Pkcs1v15Sign, RsaPrivateKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedReceiver;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const STREAM_AUDIO_CHUNK_LIMIT_BYTES: usize = 14_400;
const STREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const STREAM_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static TOKEN_CACHE: OnceLock<Mutex<Option<CachedToken>>> = OnceLock::new();

pub mod streaming_proto {
    include!(concat!(env!("OUT_DIR"), "/google.cloud.speech.v2.rs"));
}

use streaming_proto::recognition_config::DecodingConfig;
use streaming_proto::streaming_recognition_features::EndpointingSensitivity;
use streaming_proto::streaming_recognize_request::StreamingRequest;
use streaming_proto::{
    explicit_decoding_config, ExplicitDecodingConfig, RecognitionConfig,
    StreamingRecognitionConfig, StreamingRecognitionFeatures, StreamingRecognizeRequest,
    StreamingRecognizeResponse,
};

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechModel {
    // Google Cloud Speech
    Chirp3,
    Telephony,
    TelephonyShort,
    // Groq Whisper
    GroqWhisper,
}

#[derive(Debug, Clone)]
pub struct StreamingTranscriptUpdate {
    pub transcript: String,
}

#[derive(Default)]
struct GrpcFrameDecoder {
    buffer: Vec<u8>,
}

impl GrpcFrameDecoder {
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    fn next_message(&mut self) -> Result<Option<Vec<u8>>, BoxError> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }

        if self.buffer[0] != 0 {
            return Err(other_error(
                "Compressed gRPC frames are not supported by this client.",
            ));
        }

        let payload_len =
            u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
        if self.buffer.len() < 5 + payload_len {
            return Ok(None);
        }

        let payload = self.buffer[5..5 + payload_len].to_vec();
        self.buffer.drain(..5 + payload_len);
        Ok(Some(payload))
    }
}

impl Default for SpeechModel {
    fn default() -> Self {
        Self::GroqWhisper
    }
}

impl SpeechModel {
    pub fn parse(value: &str) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "" | "chirp" | "chirp3" | "chirp_3" => Ok(Self::Chirp3),
            "telephony" | "phone" | "phone_call" => Ok(Self::Telephony),
            "telephony_short" | "phone_short" | "phone_call_short" => Ok(Self::TelephonyShort),
            "groq" | "groq_whisper" | "groq-whisper" | "groq whisper" => Ok(Self::GroqWhisper),
            other => Err(format!("Unsupported speech model `{}`", other)),
        }
    }

    pub fn api_name(self) -> &'static str {
        match self {
            Self::Chirp3 => "chirp_3",
            Self::Telephony => "telephony",
            Self::TelephonyShort => "telephony_short",
            Self::GroqWhisper => "whisper-large-v3-turbo",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chirp3 => "Chirp 3",
            Self::Telephony => "Telephony",
            Self::TelephonyShort => "Telephony Short",
            Self::GroqWhisper => "Groq Whisper",
        }
    }

    pub fn settings_choice(self) -> Self {
        match self {
            Self::TelephonyShort => Self::Telephony,
            other => other,
        }
    }

    pub fn preferred_sample_rate_hz(self) -> u32 {
        match self {
            Self::Chirp3 => 16_000,
            Self::Telephony | Self::TelephonyShort => 8_000,
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

fn streaming_client() -> Result<Client, BoxError> {
    Ok(Client::builder()
        .use_rustls_tls()
        .http2_adaptive_window(true)
        .http2_keep_alive_interval(STREAM_KEEPALIVE_INTERVAL)
        .http2_keep_alive_timeout(STREAM_KEEPALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(3600)) // 1 hour for long dictation sessions
        .build()?)
}

fn token_cache() -> &'static Mutex<Option<CachedToken>> {
    TOKEN_CACHE.get_or_init(|| Mutex::new(None))
}

fn current_unix_seconds() -> Result<u64, BoxError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn other_error(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::Other, message.into()).into()
}

async fn get_google_token() -> Result<String, BoxError> {
    let now = current_unix_seconds()?;
    if let Ok(cache_guard) = token_cache().lock() {
        if let Some(cached) = cache_guard.as_ref() {
            if cached.expires_at_unix > now + 60 {
                return Ok(cached.access_token.clone());
            }
        }
    }

    let credential_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .map_err(|_| other_error("missing GOOGLE_APPLICATION_CREDENTIALS variable"))?;
    let key_file = std::fs::read_to_string(credential_path)?;
    let sa_json: Value = serde_json::from_str(&key_file)?;

    let client_email = sa_json
        .get("client_email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| other_error("No client_email inside JSON"))?;
    let private_key_pem = sa_json
        .get("private_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| other_error("No private_key inside JSON"))?;

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);

    let now = current_unix_seconds()?;
    let exp = now + 3600;

    let claim = json!({
        "iss": client_email,
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "aud": "https://oauth2.googleapis.com/token",
        "exp": exp,
        "iat": now
    });

    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_string(&claim)?);
    let message = format!("{}.{}", header, payload);

    let private_key = RsaPrivateKey::from_pkcs8_pem(private_key_pem)?;

    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    let hashed = hasher.finalize();

    let signature = private_key.sign(Pkcs1v15Sign::new::<Sha256>(), &hashed)?;
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature);
    let jwt = format!("{}.{}", message, sig_b64);

    let res = shared_client()
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .await?;

    let res_str = res.text().await?;
    let token_json: Value = serde_json::from_str(&res_str)?;

    let access_token = token_json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| other_error("No access token returned"))?;
    let expires_in = token_json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);

    if let Ok(mut cache_guard) = token_cache().lock() {
        *cache_guard = Some(CachedToken {
            access_token: access_token.to_string(),
            expires_at_unix: now + expires_in,
        });
    }

    Ok(access_token.to_string())
}

fn recognizer_path(project_id: &str, region: &str) -> String {
    format!("projects/{}/locations/{}/recognizers/_", project_id, region)
}

fn join_transcript_segments(finalized_segments: &[String], interim_segments: &[String]) -> String {
    let mut segments = Vec::with_capacity(finalized_segments.len() + interim_segments.len());
    segments.extend(finalized_segments.iter().map(|segment| segment.trim().to_string()));
    segments.extend(interim_segments.iter().map(|segment| segment.trim().to_string()));
    segments.retain(|segment| !segment.is_empty());
    segments.join(" ")
}

fn encode_grpc_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn streaming_url(region: &str) -> String {
    format!(
        "https://{}-speech.googleapis.com/google.cloud.speech.v2.Speech/StreamingRecognize",
        region
    )
}

pub async fn stream_transcribe<F>(
    audio_rx: UnboundedReceiver<Vec<u8>>,
    project_id: &str,
    region: &str,
    model: SpeechModel,
    sample_rate_hz: u32,
    mut on_update: F,
) -> Result<(), BoxError>
where
    F: FnMut(StreamingTranscriptUpdate) -> Result<(), BoxError>,
{
    let token_str = get_google_token().await?;
    let recognizer = recognizer_path(project_id, region);

    let config_request = StreamingRecognizeRequest {
        recognizer: recognizer.clone(),
        streaming_request: Some(StreamingRequest::StreamingConfig(
            StreamingRecognitionConfig {
                config: Some(RecognitionConfig {
                    decoding_config: Some(DecodingConfig::ExplicitDecodingConfig(
                        ExplicitDecodingConfig {
                            encoding: explicit_decoding_config::AudioEncoding::Linear16 as i32,
                            sample_rate_hertz: sample_rate_hz as i32,
                            audio_channel_count: 1,
                        },
                    )),
                    model: model.api_name().to_string(),
                    language_codes: vec!["en-US".to_string()],
                }),
                streaming_features: Some(StreamingRecognitionFeatures {
                    enable_voice_activity_events: true,
                    interim_results: true,
                    endpointing_sensitivity: EndpointingSensitivity::Short as i32,
                }),
            },
        )),
    };

    let initial_frame = encode_grpc_frame(&config_request.encode_to_vec());
    let outbound = stream! {
        yield Ok::<Vec<u8>, io::Error>(initial_frame);
        let mut audio_rx = audio_rx;

        while let Some(chunk) = audio_rx.recv().await {
            for chunk_slice in chunk.chunks(STREAM_AUDIO_CHUNK_LIMIT_BYTES) {
                if chunk_slice.is_empty() {
                    continue;
                }

                let audio_request = StreamingRecognizeRequest {
                    recognizer: String::new(),
                    streaming_request: Some(StreamingRequest::Audio(chunk_slice.to_vec())),
                };
                yield Ok::<Vec<u8>, io::Error>(encode_grpc_frame(&audio_request.encode_to_vec()));
            }
        }
    };

    let mut response = streaming_client()?
        .post(streaming_url(region))
        .header("Authorization", format!("Bearer {}", token_str))
        .header("Content-Type", "application/grpc")
        .header("TE", "trailers")
        .header("x-goog-request-params", format!("recognizer={}", recognizer))
        .body(reqwest::Body::wrap_stream(outbound))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(other_error(format!(
            "Google streaming API Error {}: {}",
            response.status(),
            response.text().await?
        )));
    }

    let mut decoder = GrpcFrameDecoder::default();
    let mut finalized_segments: Vec<String> = Vec::new();
    let mut last_emitted_transcript = String::new();

    while let Some(chunk) = response.chunk().await? {
        decoder.push(chunk.as_ref());

        while let Some(message_bytes) = decoder.next_message()? {
            let message = StreamingRecognizeResponse::decode(message_bytes.as_slice())?;
            if message.results.is_empty() {
                continue;
            }

            let mut interim_segments = Vec::new();

            for result in message.results {
                let transcript = result
                    .alternatives
                    .into_iter()
                    .next()
                    .map(|alternative| alternative.transcript)
                    .unwrap_or_default();
                let normalized = transcript.trim();

                if normalized.is_empty() {
                    continue;
                }

                if result.is_final {
                    finalized_segments.push(normalized.to_string());
                } else {
                    interim_segments.push(normalized.to_string());
                }
            }

            let combined_transcript =
                join_transcript_segments(&finalized_segments, &interim_segments);
            if combined_transcript.is_empty() || combined_transcript == last_emitted_transcript {
                continue;
            }

            on_update(StreamingTranscriptUpdate {
                transcript: combined_transcript.clone(),
            })?;
            last_emitted_transcript = combined_transcript;
        }
    }

    Ok(())
}

pub async fn transcribe(
    audio_bytes: Vec<u8>,
    project_id: &str,
    region: &str,
    model: SpeechModel,
) -> Result<String, BoxError> {
    let token_str = get_google_token().await?;
    let base64_audio = BASE64.encode(&audio_bytes);

    let payload = json!({
        "config": {
            "model": model.api_name(),
            "languageCodes": ["en-US"],
            "autoDecodingConfig": {}
        },
        "content": base64_audio
    });

    let url = format!(
        "https://{}-speech.googleapis.com/v2/projects/{}/locations/{}/recognizers/_:recognize",
        region, project_id, region
    );

    let res = shared_client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", token_str))
        .json(&payload)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(other_error(format!(
            "Google API Error {}: {}",
            res.status(),
            res.text().await?
        )));
    }

    let txt = res.text().await?;

    if let Ok(json_body) = serde_json::from_str::<Value>(&txt) {
        if let Some(results) = json_body.get("results").and_then(|r| r.as_array()) {
            let mut full_transcript = String::new();
            for result in results {
                if let Some(alternatives) = result.get("alternatives").and_then(|a| a.as_array()) {
                    if let Some(first_alt) = alternatives.first() {
                        if let Some(t) = first_alt.get("transcript").and_then(|t| t.as_str()) {
                            full_transcript.push_str(t);
                            full_transcript.push(' ');
                        }
                    }
                }
            }
            return Ok(full_transcript.trim_end().to_string());
        }
    }

    Ok(String::new())
}

pub async fn transcribe_groq(
    audio_bytes: Vec<u8>,
) -> Result<String, BoxError> {
    let api_key = std::env::var("GROQ_API_KEY")
        .map(|k| k.trim().to_string())
        .map_err(|_| {
            eprintln!("[-] Error: GROQ_API_KEY not found in environment variables.");
            other_error("Groq API Key (GROQ_API_KEY) missing")
        })?;
    
    if api_key.is_empty() {
        return Err(other_error("Groq API Key is empty"));
    }

    let file_part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", SpeechModel::GroqWhisper.api_name())
        .text("response_format", "json");

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
