//! The simple plugin's controls and how they drive gyroflow-core.
//!
//! The panel has a few user-facing controls; everything else is fixed here so the
//! result doesn't depend on presets saved by the Gyroflow app.

use gyroflow_plugin_base::*;
use gyroflow_plugin_base::parking_lot::RwLock;

pub const DEFAULT_STABILIZATION: f64 = 85.0;
pub const DEFAULT_HORIZON_LEVEL: f64 = 0.9;

/// Never crop below this much source detail (long side, short side).
const MIN_DETAIL: (f64, f64) = (1920.0, 1080.0);
/// Floor for sources that are already at or below `MIN_DETAIL`, so some stabilization stays possible.
const MIN_ZOOM_LIMIT_PERCENT: f64 = 115.0;
const MAX_ZOOM_LIMIT_PERCENT: f64 = 400.0;
/// Dynamic zoom: zoom in only around shaky moments, easing over this many seconds.
const ADAPTIVE_ZOOM_WINDOW_S: f64 = 4.0;
const ADAPTIVE_ZOOM_METHOD_ENVELOPE: i32 = 1;
/// The core's default (5) often stops before the zoom is under the limit.
const MAX_ZOOM_ITERATIONS: usize = 15;
/// Index of the "Default" algorithm in gyroflow-core's smoothing list.
const SMOOTHING_METHOD_DEFAULT: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Core smoothness, 0.001..=1.0
    pub smoothness: f64,
    /// Horizon lock amount, 0..=100
    pub horizon_percent: f64,
    /// Zoom limit relative to the output crop, in percent
    pub max_zoom_percent: f64,
}

impl Settings {
    /// `stabilization`: 0..=100, `horizon_level`: 0..=1, `output_crop`: the stabilizer's output size in source pixels.
    pub fn new(stabilization: f64, horizon_level: f64, output_crop: (usize, usize)) -> Self {
        let stabilization = if stabilization.is_finite() { stabilization.clamp(0.0, 100.0) } else { DEFAULT_STABILIZATION };
        let horizon_level = if horizon_level.is_finite() { horizon_level.clamp(0.0, 1.0) } else { DEFAULT_HORIZON_LEVEL };
        Self {
            smoothness: (stabilization / 100.0).max(0.001),
            horizon_percent: horizon_level * 100.0,
            max_zoom_percent: max_zoom_percent(output_crop),
        }
    }
}

/// The zoom at which the visible part of the output crop still has `MIN_DETAIL` pixels.
pub fn max_zoom_percent(output_crop: (usize, usize)) -> f64 {
    let long = output_crop.0.max(output_crop.1) as f64;
    let short = output_crop.0.min(output_crop.1) as f64;
    if short <= 0.0 { return MIN_ZOOM_LIMIT_PERCENT; }
    let zoom = (long / MIN_DETAIL.0).min(short / MIN_DETAIL.1) * 100.0;
    zoom.clamp(MIN_ZOOM_LIMIT_PERCENT, MAX_ZOOM_LIMIT_PERCENT)
}

/// Writes the settings into the plugin's keyframe cache (which the core reads through the keyframe
/// provider) and the core's zoom parameters. Returns true when something changed and the
/// stabilization is being recomputed.
pub fn apply(stab: &StabilizationManager, keyframable: &RwLock<KeyframableParams>, s: &Settings) -> bool {
    let mut changed = false;
    {
        let mut kp = keyframable.write();
        for (typ, value) in [
            (KeyframeType::SmoothingParamSmoothness, s.smoothness),
            (KeyframeType::LockHorizonAmount, s.horizon_percent),
            (KeyframeType::MaxZoom, s.max_zoom_percent),
        ] {
            let current = kp.cached_keyframes.value_at_video_timestamp(&typ, 0.0);
            if current.map_or(true, |c| (c - value).abs() > 1e-6) {
                kp.cached_keyframes.set(&typ, 0, value);
                changed = true;
            }
        }
    }
    let (max_zoom, iterations, window, method) = {
        let p = stab.params.read();
        (p.max_zoom, p.max_zoom_iterations, p.adaptive_zoom_window, p.adaptive_zoom_method)
    };
    if max_zoom.map_or(true, |z| (z - s.max_zoom_percent).abs() > 1e-6) || iterations != MAX_ZOOM_ITERATIONS {
        stab.set_max_zoom(s.max_zoom_percent, MAX_ZOOM_ITERATIONS);
        changed = true;
    }
    if (window - ADAPTIVE_ZOOM_WINDOW_S).abs() > 1e-6 {
        stab.set_adaptive_zoom(ADAPTIVE_ZOOM_WINDOW_S);
        changed = true;
    }
    if method != ADAPTIVE_ZOOM_METHOD_ENVELOPE {
        stab.set_zooming_method(ADAPTIVE_ZOOM_METHOD_ENVELOPE);
        changed = true;
    }
    // Only the default algorithm reads the smoothness keyframe; a saved app preset may pick another
    if stab.smoothing.read().current().get_name() != "Default" {
        stab.set_smoothing_method(SMOOTHING_METHOD_DEFAULT);
        changed = true;
    }
    if changed {
        log::info!("Applying {s:?}");
        // The setters above only invalidate for the app's background recompute
        stab.invalidate_blocking_smoothing();
    }
    changed
}

/// Largest zoom center offset per axis (fraction of the source size) that keeps the output
/// window inside the source at the given zoom (1.0 = no zoom).
pub fn position_range(source: (usize, usize), output: (usize, usize), zoom: f64) -> (f64, f64) {
    let zoom = if zoom.is_finite() { zoom.max(1.0) } else { 1.0 };
    let axis = |out: usize, src: usize| if src == 0 { 0.0 } else { ((1.0 - out as f64 / (src as f64 * zoom)) / 2.0).max(0.0) };
    (axis(output.0, source.0), axis(output.1, source.1))
}

/// Applies Reframe X/Y (in percent) as the core's zoom center offset, scaled so ±100% reaches
/// the source edge at the zoom limit. Returns true when the offset changed.
pub fn apply_position(stab: &StabilizationManager, x_percent: f64, y_percent: f64, max_zoom_percent: f64) -> bool {
    let (current, range) = {
        let p = stab.params.read();
        (p.adaptive_zoom_center_offset, position_range(p.size, p.output_size, max_zoom_percent / 100.0))
    };
    let clamp = |v: f64| if v.is_finite() { v.clamp(-100.0, 100.0) } else { 0.0 };
    let target = (clamp(x_percent) / 100.0 * range.0, clamp(y_percent) / 100.0 * range.1);
    if (current.0 - target.0).abs() <= 1e-6 && (current.1 - target.1).abs() <= 1e-6 {
        return false;
    }
    log::info!("Reframe {x_percent:.1}%/{y_percent:.1}% (range {range:.3?}): offset {current:.4?} -> {target:.4?}");
    stab.set_zooming_center_x(target.0);
    stab.set_zooming_center_y(target.1);
    stab.invalidate_blocking_zooming();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_limit_keeps_hd_detail() {
        // 4000x3000 source in a 16:9 timeline -> 4000x2250 crop
        assert!((max_zoom_percent((4000, 2250)) - 208.333).abs() < 0.01);
        assert!((max_zoom_percent((3840, 2160)) - 200.0).abs() < 1e-9);
        // Vertical timelines use the same detail floor, rotated
        assert!((max_zoom_percent((2250, 4000)) - 208.333).abs() < 0.01);
    }

    #[test]
    fn zoom_limit_is_bounded() {
        assert_eq!(max_zoom_percent((1920, 1080)), MIN_ZOOM_LIMIT_PERCENT);
        assert_eq!(max_zoom_percent((0, 0)), MIN_ZOOM_LIMIT_PERCENT);
        assert_eq!(max_zoom_percent((16000, 9000)), MAX_ZOOM_LIMIT_PERCENT);
    }

    #[test]
    fn controls_map_to_core_ranges() {
        let s = Settings::new(85.0, 0.9, (4000, 2250));
        assert!((s.smoothness - 0.85).abs() < 1e-9);
        assert!((s.horizon_percent - 90.0).abs() < 1e-9);
        let clamped = Settings::new(250.0, 3.0, (4000, 2250));
        assert_eq!((clamped.smoothness, clamped.horizon_percent), (1.0, 100.0));
        let off = Settings::new(-5.0, -1.0, (4000, 2250));
        assert_eq!((off.smoothness, off.horizon_percent), (0.001, 0.0));
        let nan = Settings::new(f64::NAN, f64::NAN, (4000, 2250));
        assert_eq!((nan.smoothness, nan.horizon_percent), (0.85, 90.0));
    }

    #[test]
    fn range_uses_vertical_headroom_without_zoom() {
        let (x, y) = position_range((4000, 3000), (4000, 2250), 1.0);
        assert_eq!(x, 0.0);
        assert!((y - 0.125).abs() < 1e-9);
    }

    #[test]
    fn range_grows_with_zoom_limit() {
        let (x, y) = position_range((4000, 3000), (4000, 2250), 1.3);
        assert!((x - (1.0 - 1.0 / 1.3) / 2.0).abs() < 1e-9);
        assert!((y - (1.0 - 0.75 / 1.3) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn range_is_never_negative() {
        assert_eq!(position_range((1920, 1080), (3840, 2160), 0.5), (0.0, 0.0));
        assert_eq!(position_range((0, 0), (1920, 1080), 1.3), (0.0, 0.0));
    }

    /// End-to-end with a real clip: the zoom stays within the 1080p limit at every setting.
    /// `GF_TEST_VIDEO=/path/to/clip.MP4 cargo test --release real_clip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_clip_zoom_stays_within_limit() {
        use gyroflow_plugin_base::gyroflow_core::filesystem;
        use std::sync::Arc;

        let path = std::env::var("GF_TEST_VIDEO").expect("set GF_TEST_VIDEO to a video file with gyro data");
        let stab = StabilizationManager::default();
        let url = filesystem::path_to_url(&path);
        let mut file = filesystem::open_file(&url, false, false).unwrap();
        let filesize = file.size;
        stab.load_video_file(file.get_file(), filesize, &url, None, true).unwrap();
        let size = stab.params.read().size;
        // 16:9 crop of the source, as the plugin requests on a 16:9 timeline
        stab.set_output_size(size.0, size.0 * 9 / 16);
        stab.init_size();
        let output = stab.params.read().output_size;

        let keyframable = Arc::new(RwLock::new(KeyframableParams { use_gyroflows_keyframes: false, cached_keyframes: KeyframeManager::default() }));
        let provider = keyframable.clone();
        stab.keyframes.write().set_custom_provider(move |_, typ, ts| provider.read().cached_keyframes.value_at_video_timestamp(typ, ts));

        let max_zoom = |stab: &StabilizationManager| {
            let p = stab.params.read();
            100.0 / p.fovs.iter().copied().reduce(f64::min).unwrap_or(1.0)
        };
        println!("source {size:?}, output crop {output:?}, limit {:.1}%", max_zoom_percent(output));
        for (stabilization, horizon) in [(0.0, 0.0), (50.0, 0.0), (85.0, 0.9), (100.0, 1.0)] {
            let s = Settings::new(stabilization, horizon, output);
            assert!(apply(&stab, &keyframable, &s));
            stab.recompute_blocking();
            let zoom = max_zoom(&stab);
            println!("stabilization {stabilization:>5.1}, horizon {horizon:.1}: max zoom {zoom:.1}%");
            assert!(zoom <= s.max_zoom_percent * 1.02, "zoom {zoom:.1}% exceeds the limit {:.1}%", s.max_zoom_percent);
        }

        // A cut of the second half: the zoom is solved over the visible part only
        stab.set_trim_ranges(vec![(0.5, 1.0)]);
        stab.invalidate_blocking_smoothing();
        stab.recompute_blocking();
        let p = stab.params.read();
        let half = p.fovs.len() / 2;
        let zoom = 100.0 / p.fovs[half..].iter().copied().reduce(f64::min).unwrap_or(1.0);
        println!("second half only: max zoom {zoom:.1}%");
    }
}
