// ═══════════════════════════════════════════════════════════════════
// audio_bars_ui.rs — Twinkle pattern driven by VAD score
//
// Random bars jump independently (twinkle), but the amplitude
// is controlled by the VAD score:
//   Silence (score < 0.20): gentle idle wobble
//   Voice   (score > 0.50): energetic full-range twinkle
//
// Drop-in module. No changes to main.rs, audio.rs, or any other file.
// ═══════════════════════════════════════════════════════════════════

use egui::{Color32, Pos2, Rect, Rounding, Stroke, Vec2};

// ── Configuration ──────────────────────────────────────────────
const BAR_COUNT: usize = 25;
const BAR_WIDTH: f32 = 3.2;
const BAR_GAP: f32 = 1.8;
const BAR_MIN_HEIGHT: f32 = 2.0;
const BAR_MAX_HEIGHT: f32 = 34.0;
const BAR_ROUNDING: f32 = 1.6;

const BASE_HEIGHTS: [f32; BAR_COUNT] = [
    4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 22.0,
    24.0, 26.0, 28.0,
    26.0, 24.0, 22.0, 20.0, 18.0, 16.0, 14.0, 12.0, 10.0, 8.0, 6.0, 4.0,
];

const GRADIENT: [(u8, u8, u8); 9] = [
    (255, 120, 50), (255, 80,  80), (240, 60,  120),
    (200, 55,  170), (160, 60,  210), (120, 80,  230),
    (80,  120, 235), (55,  160, 230), (40,  185, 215),
];

fn gradient_color(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let seg = t * (GRADIENT.len() - 1) as f32;
    let i = seg.floor() as usize;
    let f = seg - i as f32;
    if i >= GRADIENT.len() - 1 {
        let (r, g, b) = GRADIENT[GRADIENT.len() - 1];
        return Color32::from_rgb(r, g, b);
    }
    let (r0, g0, b0) = GRADIENT[i];
    let (r1, g1, b1) = GRADIENT[i + 1];
    Color32::from_rgb(
        (r0 as f32 + (r1 as f32 - r0 as f32) * f) as u8,
        (g0 as f32 + (g1 as f32 - g0 as f32) * f) as u8,
        (b0 as f32 + (b1 as f32 - b0 as f32) * f) as u8,
    )
}

// ═══════════════════════════════════════════════════════════════════
// VAD Score → Twinkle Energy
// ═══════════════════════════════════════════════════════════════════

const VAD_SILENCE_MAX: f32 = 0.05;
const VAD_VOICE_MIN: f32 = 0.25;
const VAD_PEAK: f32 = 0.50;

/// Map VAD score to twinkle energy (0.0 – 1.0).
///
/// - 0.00 – 0.20 → 0.0  (idle wobble only)
/// - 0.20 – 0.50 → 0.0 – 0.3  (transition)
/// - 0.50 – 0.80 → 0.3 – 1.0  (full twinkle)
pub fn vad_score_to_energy(score: f32) -> f32 {
    let score = score.clamp(0.0, 1.0);
    if score <= VAD_SILENCE_MAX {
        return 0.0;
    }
    if score <= VAD_VOICE_MIN {
        return ((score - VAD_SILENCE_MAX) / (VAD_VOICE_MIN - VAD_SILENCE_MAX)) * 0.3;
    }
    0.3 + ((score - VAD_VOICE_MIN) / (VAD_PEAK - VAD_VOICE_MIN)) * 0.7
}

// ═══════════════════════════════════════════════════════════════════
// State
// ═══════════════════════════════════════════════════════════════════

pub struct TwinkleVadState {
    pub bar_heights: [f32; BAR_COUNT],
    pub twinkle_targets: [f32; BAR_COUNT],
    pub smoothed_energy: f32,
    pub last_twinkle_time: f32,
}

impl Default for TwinkleVadState {
    fn default() -> Self {
        Self {
            bar_heights: [BAR_MIN_HEIGHT; BAR_COUNT],
            twinkle_targets: [0.5; BAR_COUNT],
            smoothed_energy: 0.0,
            last_twinkle_time: -1.0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Simple PRNG for twinkle (no std::rand dependency)
// ═══════════════════════════════════════════════════════════════════

fn simple_hash(seed: u64) -> f32 {
    let mut x = seed;
    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    ((x >> 11) as f32) / (1u64 << 53) as f32
}

// ═══════════════════════════════════════════════════════════════════
// Drawing
// ═══════════════════════════════════════════════════════════════════

/// Draw twinkle bars driven by VAD score.
///
/// `vad_score` — raw VAD score (0.0 – 1.0) from earshot::Detector.
/// `time` — seconds, monotonically increasing (e.g. from Instant::now).
/// `state` — persistent twinkle state, mutated each frame.
pub fn draw_twinkle_vad(
    ui: &egui::Ui,
    clip_rect: Rect,
    vad_score: f32,
    time: f32,
    state: &mut TwinkleVadState,
) {
    let painter = ui.painter().with_clip_rect(clip_rect);

    // VAD score → energy
    let target_energy = vad_score_to_energy(vad_score);
    state.smoothed_energy += (target_energy - state.smoothed_energy) * 0.20;
    let energy = state.smoothed_energy;

    // Refresh twinkle targets every ~120ms
    if time - state.last_twinkle_time > 0.12 {
        let seed = (time * 1000.0) as u64;
        for i in 0..BAR_COUNT {
            state.twinkle_targets[i] = simple_hash(seed + i as u64);
        }
        state.last_twinkle_time = time;
    }

    // Layout: centered
    let bars_total_width = BAR_COUNT as f32 * BAR_WIDTH + (BAR_COUNT - 1) as f32 * BAR_GAP;
    let center = clip_rect.center();
    let bars_left = center.x - bars_total_width / 2.0;

    // Idle wobble amplitude when silent
    const IDLE_WOBBLE: f32 = 0.08;

    // Draw bars
    for i in 0..BAR_COUNT {
        let base = BASE_HEIGHTS[i] * 0.15;
        let max_reach = BAR_MAX_HEIGHT - base;

        // Twinkle amplitude: idle wobble when silent, full range when voice
        let twinkle_amp = IDLE_WOBBLE + energy * (1.0 - IDLE_WOBBLE);

        // Each bar goes toward its random target, scaled by energy
        let target = base + state.twinkle_targets[i] * max_reach * twinkle_amp;

        // Per-bar smoothing: fast attack, slow decay
        let speed = if target > state.bar_heights[i] {
            0.30
        } else {
            0.12
        };
        state.bar_heights[i] += (target - state.bar_heights[i]) * speed;

        let h = state.bar_heights[i];
        let x = bars_left + i as f32 * (BAR_WIDTH + BAR_GAP);
        let color = gradient_color(i as f32 / (BAR_COUNT - 1) as f32);

        let rect = Rect::from_center_size(
            Pos2::new(x + BAR_WIDTH / 2.0, center.y),
            Vec2::new(BAR_WIDTH, h),
        );
        painter.rect(rect, Rounding::same(BAR_ROUNDING), color, Stroke::NONE);
    }
}
