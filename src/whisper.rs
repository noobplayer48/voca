use hound::WavReader;
use std::io::Cursor;
use tokio::sync::mpsc::UnboundedReceiver;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// whisper.cpp always needs 16 kHz f32 mono
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

// VAD tuning
const RMS_WINDOW_SAMPLES: usize = 1_600;   // 100 ms window for energy check
const RMS_SPEECH_THRESHOLD: f32 = 0.015;   // below this → silence
const SILENCE_TRIGGER_SAMPLES: usize = 8_000;  // 500 ms silence → flush segment
const MIN_SEGMENT_SAMPLES: usize = 4_000;       // 250 ms minimum — skip noise bursts
const MAX_SEGMENT_SAMPLES: usize = 16_000 * 28; // 28 s hard cap (whisper max is 30 s)

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Real-time VAD-chunked transcription. Mirrors the signature of
/// `api::stream_transcribe` so `logic.rs` can call both the same way.
pub async fn stream_transcribe<F>(
    mut audio_rx: UnboundedReceiver<Vec<u8>>,
    model_path: &str,
    mut on_update: F,
) -> Result<(), BoxError>
where
    F: FnMut(String) -> Result<(), BoxError>,
{
    let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .map_err(|e| format!("Failed to load whisper model at `{}`: {}", model_path, e))?;

    let mut sample_buf: Vec<f32> = Vec::new();
    let mut consecutive_silence: usize = 0;
    let mut has_speech = false;
    let mut finalized = String::new();

    while let Some(bytes) = audio_rx.recv().await {
        let new_samples = i16_bytes_to_f32(&bytes);

        for chunk in new_samples.chunks(RMS_WINDOW_SAMPLES) {
            let rms = compute_rms(chunk);
            sample_buf.extend_from_slice(chunk);

            if rms >= RMS_SPEECH_THRESHOLD {
                consecutive_silence = 0;
                has_speech = true;
            } else {
                consecutive_silence += chunk.len();
            }

            let silence_triggered = consecutive_silence >= SILENCE_TRIGGER_SAMPLES;
            let overflow = sample_buf.len() >= MAX_SEGMENT_SAMPLES;

            if has_speech && (silence_triggered || overflow) {
                // Trim trailing silence before inference to cut latency
                let active_end = trailing_speech_end(&sample_buf, RMS_SPEECH_THRESHOLD);
                let segment = &sample_buf[..active_end];

                if segment.len() >= MIN_SEGMENT_SAMPLES {
                    if let Ok(text) = run_inference(&ctx, segment) {
                        let text = clean_whisper_output(&text);
                        if !text.is_empty() {
                            if !finalized.is_empty() {
                                finalized.push(' ');
                            }
                            finalized.push_str(&text);
                            on_update(finalized.clone())?;
                        }
                    }
                }

                sample_buf.clear();
                consecutive_silence = 0;
                has_speech = false;
            }
        }
    }

    // Flush whatever remains when the channel closes (recording stopped)
    if has_speech && sample_buf.len() >= MIN_SEGMENT_SAMPLES {
        let active_end = trailing_speech_end(&sample_buf, RMS_SPEECH_THRESHOLD);
        if let Ok(text) = run_inference(&ctx, &sample_buf[..active_end]) {
            let text = clean_whisper_output(&text);
            if !text.is_empty() {
                if !finalized.is_empty() {
                    finalized.push(' ');
                }
                finalized.push_str(&text);
                on_update(finalized)?;
            }
        }
    }

    Ok(())
}

/// Batch transcription of a complete WAV file — used as the fallback path.
pub fn transcribe_wav(wav_bytes: &[u8], model_path: &str) -> Result<String, BoxError> {
    let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .map_err(|e| format!("Failed to load whisper model: {}", e))?;

    let mut reader = WavReader::new(Cursor::new(wav_bytes))?;
    let spec = reader.spec();

    // Collect mono i16 samples
    let raw: Vec<i16> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .step_by(spec.channels as usize) // downmix to mono by taking ch0
            .filter_map(|s| s.ok())
            .collect(),
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .step_by(spec.channels as usize)
            .filter_map(|s| s.ok())
            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect(),
    };

    let mut f32_samples: Vec<f32> = raw.iter().map(|&s| s as f32 / i16::MAX as f32).collect();

    // Resample to 16 kHz if necessary (linear interpolation — good enough for speech)
    if spec.sample_rate != WHISPER_SAMPLE_RATE {
        f32_samples = resample_linear(&f32_samples, spec.sample_rate, WHISPER_SAMPLE_RATE);
    }

    run_inference(&ctx, &f32_samples)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn run_inference(ctx: &WhisperContext, samples: &[f32]) -> Result<String, BoxError> {
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_no_context(true); // each segment is independent in real-time mode

    state.full(params, samples).map_err(|e| e.to_string())?;

    let n = state.full_n_segments();
    let mut out = String::new();
    for i in 0..n {
        if let Some(segment) = state.get_segment(i) {
            if let Ok(seg) = segment.to_str() {
                let trimmed = seg.trim();
                if !trimmed.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(trimmed);
                }
            }
        }
    }
    Ok(out)
}

/// whisper.cpp emits hallucination tokens like `[BLANK_AUDIO]`, `(Music)`, etc.
/// Strip them so they don't pollute the transcript.
fn clean_whisper_output(text: &str) -> String {
    text.split_whitespace()
        .filter(|token| {
            let t = token.trim();
            !(t.starts_with('[') && t.ends_with(']'))
                && !(t.starts_with('(') && t.ends_with(')'))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Walk backwards to find where speech actually ends, so we don't feed
/// 500 ms of silence into every inference call.
fn trailing_speech_end(samples: &[f32], threshold: f32) -> usize {
    let window = RMS_WINDOW_SAMPLES;
    let mut end = samples.len();
    while end >= window {
        let rms = compute_rms(&samples[end - window..end]);
        if rms >= threshold {
            break;
        }
        end -= window;
    }
    end.max(MIN_SEGMENT_SAMPLES).min(samples.len())
}

fn i16_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| {
            let s = i16::from_le_bytes([b[0], b[1]]);
            s as f32 / i16::MAX as f32
        })
        .collect()
}

fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_hz as f64 / to_hz as f64;
    let out_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let lo = pos.floor() as usize;
        let hi = (lo + 1).min(input.len() - 1);
        let t = pos.fract() as f32;
        out.push(input[lo] * (1.0 - t) + input[hi] * t);
    }
    out
}
