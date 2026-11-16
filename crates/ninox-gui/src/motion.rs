//! The reply drawer's "is thinking" animation: frame maths, and the macOS
//! Reduce Motion setting that swaps it for a static ellipsis.

use std::time::Duration;

/// One animation step; the dot wave is [`WAVE_FRAMES`] of these.
pub const TICK: Duration = Duration::from_millis(120);
pub const WAVE_FRAMES: u64 = 8;

/// System Settings › Accessibility › Display › Reduce motion.
#[cfg(target_os = "macos")]
pub fn reduce_motion() -> bool {
    use objc2_app_kit::NSWorkspace;
    unsafe { NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() }
}

#[cfg(not(target_os = "macos"))]
pub fn reduce_motion() -> bool {
    false
}

/// Brightness (0..=1) of each of the three trailing dots at `tick`: a crest
/// that walks left to right, then a beat of rest before the next pass.
pub fn dot_levels(tick: u64) -> [f32; 3] {
    let crest = (tick % WAVE_FRAMES) as f32;
    std::array::from_fn(|k| (1.0 - (crest - 1.0 - k as f32).abs() / 1.5).max(0.0))
}

/// The header status dot's opacity at `tick`: a slow breathe between 45%
/// and 100%, one cycle per two dot waves.
pub fn dot_pulse(tick: u64) -> f32 {
    let period = 2 * WAVE_FRAMES;
    let phase = (tick % period) as f32 / period as f32;
    0.725 + 0.275 * (phase * std::f32::consts::TAU).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dot_wave_cycles_and_moves_every_frame() {
        for t in 0..WAVE_FRAMES {
            assert_eq!(dot_levels(t), dot_levels(t + WAVE_FRAMES));
        }
        let frames: Vec<[f32; 3]> = (0..WAVE_FRAMES).map(dot_levels).collect();
        let mut distinct = frames.clone();
        distinct.dedup();
        assert!(distinct.len() >= 5, "the wave animates rather than holding: {frames:?}");
        for k in 0..3 {
            assert!(frames.iter().any(|f| f[k] == 1.0), "dot {k} reaches full brightness once per wave");
        }
    }

    #[test]
    fn the_status_dot_breathes_between_45_and_100_percent() {
        let levels: Vec<f32> = (0..2 * WAVE_FRAMES).map(dot_pulse).collect();
        assert_eq!(levels[0], 1.0);
        let min = levels.iter().copied().fold(f32::MAX, f32::min);
        assert!((min - 0.45).abs() < 1e-5, "{min}");
        assert_eq!(dot_pulse(0), dot_pulse(2 * WAVE_FRAMES));
    }
}
