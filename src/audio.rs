use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{WavSpec, WavWriter};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex, mpsc,
};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use earshot::Detector;

const STREAM_CHUNK_DURATION: Duration = Duration::from_millis(100);
const VAD_SAMPLE_RATE: u32 = 16_000;
const VAD_FRAME_SIZE: usize = 256;
const VAD_THRESHOLD: f32 = 0.4;
const SILENCE_THRESHOLD_FRAMES: u32 = 65; // 2 seconds at 16k/256

struct LiveStreamSink {
    sender: UnboundedSender<Vec<u8>>,
    pending_samples: Vec<i16>,
    chunk_samples: usize,
}

impl LiveStreamSink {
    fn new(sender: UnboundedSender<Vec<u8>>, sample_rate: u32) -> Self {
        let chunk_samples =
            ((sample_rate as f64) * STREAM_CHUNK_DURATION.as_secs_f64()).round() as usize;

        Self {
            sender,
            pending_samples: Vec::new(),
            chunk_samples: chunk_samples.max(1),
        }
    }
}

struct VadState {
    detector: Detector,
    silence_frames: u32,
    buffer: Vec<i16>,
    trigger_tx: mpsc::Sender<()>,
    active: bool,
    downsample_ratio: u32,
}

pub struct AudioRecorder {
    stream: Option<cpal::Stream>,
    buffer: Arc<Mutex<Vec<i16>>>,
    spec: Option<WavSpec>,
    input_level: Arc<AtomicU32>,
    preferred_sample_rate_hz: u32,
    live_stream_sink: Option<Arc<Mutex<LiveStreamSink>>>,
    vad_state: Option<Arc<Mutex<VadState>>>,
}

impl AudioRecorder {
    pub fn new(input_level: Arc<AtomicU32>, preferred_sample_rate_hz: u32) -> Self {
        Self {
            stream: None,
            buffer: Arc::new(Mutex::new(Vec::new())),
            spec: None,
            input_level,
            preferred_sample_rate_hz,
            live_stream_sink: None,
            vad_state: None,
        }
    }

    pub fn set_vad_trigger(&mut self, trigger_tx: mpsc::Sender<()>) {
        self.vad_state = Some(Arc::new(Mutex::new(VadState {
            detector: Detector::default(),
            silence_frames: 0,
            buffer: Vec::with_capacity(VAD_FRAME_SIZE),
            trigger_tx,
            active: false,
            downsample_ratio: 1,
        })));
    }

    pub fn start_streaming(
        &mut self,
        audio_sender: UnboundedSender<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.start_inner(Some(audio_sender))
    }

    fn start_inner(
        &mut self,
        audio_sender: Option<UnboundedSender<Vec<u8>>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.input_level.store(0, Ordering::Relaxed);
        self.live_stream_sink = None;

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No input device found")?;

        // Prefer a model-appropriate sample rate, but gracefully fall back if the device can't do it.
        let mut selected_config = None;
        if let Ok(supported_configs) = device.supported_input_configs() {
            for config_range in supported_configs {
                if config_range.channels() <= 2
                    && config_range.min_sample_rate().0 <= self.preferred_sample_rate_hz
                    && config_range.max_sample_rate().0 >= self.preferred_sample_rate_hz
                {
                    selected_config = Some(
                        config_range.with_sample_rate(cpal::SampleRate(self.preferred_sample_rate_hz)),
                    );
                    break;
                }
            }
        }

        let config = selected_config.unwrap_or(device.default_input_config()?);
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // Output WAV is always mono.
        self.spec = Some(WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        });

        self.live_stream_sink = audio_sender.map(|sender| {
            Arc::new(Mutex::new(LiveStreamSink::new(sender, sample_rate)))
        });

        let buf_clone = self.buffer.clone();
        let level_f32 = self.input_level.clone();
        let level_i16 = self.input_level.clone();
        let level_u16 = self.input_level.clone();
        let stream_sink_f32 = self.live_stream_sink.clone();
        let stream_sink_i16 = self.live_stream_sink.clone();
        let stream_sink_u16 = self.live_stream_sink.clone();
        
        let vad_state_f32 = self.vad_state.clone();
        let vad_state_i16 = self.vad_state.clone();
        let vad_state_u16 = self.vad_state.clone();
        
        if let Some(ref vs) = self.vad_state {
            if let Ok(mut guard) = vs.lock() {
                guard.silence_frames = 0;
                guard.buffer.clear();
                guard.active = sample_rate >= VAD_SAMPLE_RATE; // Support anything above 16k
                guard.downsample_ratio = (sample_rate as f32 / VAD_SAMPLE_RATE as f32).round() as u32;
                
                if !guard.active || guard.downsample_ratio == 0 {
                    guard.active = false;
                    eprintln!("Warning: VAD disabled because sample rate is too low: {} Hz", sample_rate);
                } else if guard.downsample_ratio > 1 {
                    println!("[*] VAD: Using {}:1 downsampler for {} Hz input", guard.downsample_ratio, sample_rate);
                }
            }
        }

        let err_fn = move |err| {
            eprintln!("an error occurred on the audio stream: {}", err);
        };

        let sample_format = config.sample_format();
        let config_into = config.into();

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config_into,
                move |data: &[f32], _: &_| {
                    write_input_data(
                        data,
                        channels,
                        &buf_clone,
                        &level_f32,
                        stream_sink_f32.as_ref(),
                        vad_state_f32.as_ref(),
                    )
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config_into,
                move |data: &[i16], _: &_| {
                    write_input_data(
                        data,
                        channels,
                        &buf_clone,
                        &level_i16,
                        stream_sink_i16.as_ref(),
                        vad_state_i16.as_ref(),
                    )
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_input_stream(
                &config_into,
                move |data: &[u16], _: &_| {
                    write_input_data(
                        data,
                        channels,
                        &buf_clone,
                        &level_u16,
                        stream_sink_u16.as_ref(),
                        vad_state_u16.as_ref(),
                    )
                },
                err_fn,
                None,
            )?,
            _ => return Err("Unsupported sample format".into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    pub fn set_preferred_sample_rate_hz(&mut self, preferred_sample_rate_hz: u32) {
        self.preferred_sample_rate_hz = preferred_sample_rate_hz;
    }

    pub fn sample_rate_hz(&self) -> Option<u32> {
        self.spec.as_ref().map(|spec| spec.sample_rate)
    }

    pub fn stop(&mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        if let Some(stream) = self.stream.take() {
            stream.pause()?;
        }

        if let Some(live_stream_sink) = self.live_stream_sink.take() {
            flush_live_stream_sink(&live_stream_sink);
        }

        let spec = self
            .spec
            .take()
            .ok_or("Recorder is not active. Start recording first.")?;
        let data = self.buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.input_level.store(0, Ordering::Relaxed);
        encode_wav(spec, &data)
    }
}

fn encode_wav(spec: WavSpec, samples: &[i16]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut wav_cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = WavWriter::new(&mut wav_cursor, spec)?;
        for &sample in samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
    }
    Ok(wav_cursor.into_inner())
}

fn write_input_data<T>(
    input: &[T],
    channels: u16,
    writer: &Arc<Mutex<Vec<i16>>>,
    input_level: &Arc<AtomicU32>,
    live_stream_sink: Option<&Arc<Mutex<LiveStreamSink>>>,
    vad_state: Option<&Arc<Mutex<VadState>>>,
)
where
    T: cpal::Sample,
    i16: cpal::FromSample<T>,
    i32: cpal::FromSample<T>,
{
    let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
    let mut peak: u16 = 0;
    let host_channels = channels as usize;
    let mut mono_samples = Vec::with_capacity(input.len() / host_channels + 1);
    let mut i = 0;
    while i + host_channels <= input.len() {
        let frame = &input[i..i + host_channels];
        let sum: i32 = frame.iter().map(|&s| s.to_sample::<i16>() as i32).sum();
        let mono_sample_i16 = (sum / host_channels as i32) as i16;
        
        guard.push(mono_sample_i16);
        mono_samples.push(mono_sample_i16);
        peak = peak.max(mono_sample_i16.unsigned_abs());
        i += host_channels;
    }
    drop(guard);

    if let Some(live_stream_sink) = live_stream_sink {
        push_live_stream_samples(live_stream_sink, &mono_samples);
    }

    if let Some(vs) = vad_state {
        process_vad(vs, &mono_samples);
    }

    let instant_level = peak as f32 / i16::MAX as f32;
    let prev = input_level.load(Ordering::Relaxed) as f32 / 1000.0;
    let smoothed = if instant_level > prev {
        prev * 0.35 + instant_level * 0.65
    } else {
        prev * 0.90 + instant_level * 0.10
    };
    let scaled = (smoothed.clamp(0.0, 1.0) * 1000.0) as u32;
    input_level.store(scaled, Ordering::Relaxed);
}

fn push_live_stream_samples(live_stream_sink: &Arc<Mutex<LiveStreamSink>>, samples: &[i16]) {
    if samples.is_empty() {
        return;
    }

    let mut sink = live_stream_sink.lock().unwrap_or_else(|e| e.into_inner());
    sink.pending_samples.extend_from_slice(samples);

    while sink.pending_samples.len() >= sink.chunk_samples {
        let chunk_samples = sink.chunk_samples;
        let chunk: Vec<i16> = sink.pending_samples.drain(..chunk_samples).collect();
        if sink.sender.send(samples_to_bytes(&chunk)).is_err() {
            sink.pending_samples.clear();
            break;
        }
    }
}

fn flush_live_stream_sink(live_stream_sink: &Arc<Mutex<LiveStreamSink>>) {
    let mut sink = live_stream_sink.lock().unwrap_or_else(|e| e.into_inner());
    if sink.pending_samples.is_empty() {
        return;
    }

    let final_chunk = samples_to_bytes(&sink.pending_samples);
    let _ = sink.sender.send(final_chunk);
    sink.pending_samples.clear();
}

fn samples_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn process_vad(vad_state: &Arc<Mutex<VadState>>, samples: &[i16]) {
    let mut guard = match vad_state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if !guard.active {
        return;
    }

    let ratio = guard.downsample_ratio;
    for (i, &sample) in samples.iter().enumerate() {
        if i as u32 % ratio == 0 {
            guard.buffer.push(sample);
        }
    }

    while guard.buffer.len() >= VAD_FRAME_SIZE {
        let frame: Vec<i16> = guard.buffer.drain(..VAD_FRAME_SIZE).collect();
        let score = guard.detector.predict_i16(&frame);

        if score >= VAD_THRESHOLD {
            guard.silence_frames = 0;
            guard.active = true; // Re-arm if we were deactivated
        } else {
            if guard.active {
                guard.silence_frames += 1;
                if guard.silence_frames >= SILENCE_THRESHOLD_FRAMES {
                    println!("🔴 VAD: Silence detected — stopping transcription");
                    let _ = guard.trigger_tx.send(());
                    guard.silence_frames = 0;
                    guard.active = false; // Prevent double-triggering until speech resumes
                }
            }
        }
    }
}
