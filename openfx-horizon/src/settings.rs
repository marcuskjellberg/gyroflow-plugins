//! The Horizon Leveling controls and how they drive gyroflow-core.
//!
//! Only the roll is corrected: the core runs without smoothing, so the one rotation left is the
//! horizon lock, on a distortion-free lens (see `cameras::pinhole_profile_json`). The crop solver
//! (`gyroflow_plugin_base::crop`) fits the frame inside the rotated source.
//!
//! Leveling, zoom and offset can be keyframed: each is one value, or a value per frame of the clip.

use gyroflow_plugin_base::*;
use gyroflow_plugin_base::parking_lot::RwLock;

pub const DEFAULT_LEVELING: f64 = 100.0;
/// Seconds the crop takes to zoom by 2x
pub const DEFAULT_CROP_SMOOTHNESS_S: f64 = 4.0;
pub const MIN_CROP_SMOOTHNESS_S: f64 = 0.2;
pub const MAX_CROP_SMOOTHNESS_S: f64 = 8.0;
pub const DEFAULT_ZOOM_PERCENT: f64 = 100.0;
pub const MAX_ZOOM_PERCENT: f64 = 400.0;
/// Offset slider range, in percent of half the source size
pub const MAX_OFFSET_PERCENT: f64 = 50.0;
/// Index of "No smoothing" in gyroflow-core's smoothing list
const SMOOTHING_METHOD_NONE: usize = 0;
const SMOOTHING_METHOD_NONE_NAME: &str = "No smoothing";

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Horizon lock amount, 0..=100
    pub horizon_percent: Vec<f64>,
    pub static_crop: bool,
    pub crop_smoothness_s: f64,
    /// Zoom in on top of the largest crop, 1..=4
    pub zoom: Vec<f64>,
    /// Frame centre offset as a fraction of the source size, the core's zooming centre
    pub offset: (Vec<f64>, Vec<f64>),
}

impl Settings {
    /// Each of `leveling`, `zoom_percent` and the offsets holds one value, or one per frame of the clip
    pub fn new(leveling: Vec<f64>, static_crop: bool, crop_smoothness_s: f64, zoom_percent: Vec<f64>, offset_x: Vec<f64>, offset_y: Vec<f64>) -> Self {
        let clamp = |v: f64, min: f64, max: f64, default: f64| if v.is_finite() { v.clamp(min, max) } else { default };
        let offset = |values: Vec<f64>| values.into_iter().map(|v| clamp(v, -MAX_OFFSET_PERCENT, MAX_OFFSET_PERCENT, 0.0) / 200.0).collect();
        Self {
            horizon_percent: leveling.into_iter().map(|v| clamp(v, 0.0, 100.0, DEFAULT_LEVELING)).collect(),
            static_crop,
            crop_smoothness_s: clamp(crop_smoothness_s, MIN_CROP_SMOOTHNESS_S, MAX_CROP_SMOOTHNESS_S, DEFAULT_CROP_SMOOTHNESS_S),
            zoom: zoom_percent.into_iter().map(|v| clamp(v, DEFAULT_ZOOM_PERCENT, MAX_ZOOM_PERCENT, DEFAULT_ZOOM_PERCENT) / 100.0).collect(),
            offset: (offset(offset_x), offset(offset_y)),
        }
    }

    /// The same values for every frame
    pub fn constant(leveling: f64, static_crop: bool, crop_smoothness_s: f64, zoom_percent: f64, offset_x: f64, offset_y: f64) -> Self {
        Self::new(vec![leveling], static_crop, crop_smoothness_s, vec![zoom_percent], vec![offset_x], vec![offset_y])
    }
}

/// Loads the distortion-free lens for the clip's size, unless it already is the loaded one
pub fn apply_pinhole_lens(stab: &StabilizationManager) {
    let (width, height) = stab.params.read().size;
    let json = cameras::pinhole_profile_json(width, height);
    let identifier = format!("{}-{width}x{height}", cameras::PINHOLE_IDENTIFIER);
    if stab.lens.read().identifier == identifier { return; }
    match stab.load_lens_profile(&json) {
        Ok(_) => {
            log::info!("Applied pinhole lens {width}x{height}");
            stab.invalidate_blocking_smoothing();
        }
        Err(e) => log::error!("Failed to apply pinhole lens {width}x{height}: {e:?}"),
    }
}

/// The keyframes of a control: one at the start, or one at every frame
fn keyframes_for(values: &[f64], fps: f64) -> Vec<(i64, f64)> {
    match values {
        [] => Vec::new(),
        [value] => vec![(0, *value)],
        _ => values.iter().enumerate().map(|(frame, v)| ((frame as f64 * 1_000_000.0 / fps).round() as i64, *v)).collect(),
    }
}

/// Replaces the keyframes of `typ` with `wanted` unless they already are. Returns true when they changed
fn set_keyframes(manager: &mut KeyframeManager, typ: &KeyframeType, wanted: &[(i64, f64)]) -> bool {
    let current = manager.get_keyframes(typ);
    let same = current.is_some_and(|current| {
        current.len() == wanted.len() && current.iter().zip(wanted).all(|((ts, kf), (want_ts, want))| ts == want_ts && (kf.value - want).abs() <= 1e-9)
    });
    if same { return false; }
    manager.clear_type(typ);
    for (ts, value) in wanted {
        manager.set(typ, *ts, *value);
    }
    true
}

/// Writes the settings into the plugin's keyframe cache (which the core reads through the keyframe
/// provider) and the core's parameters. Returns true when something changed.
pub fn apply(stab: &StabilizationManager, keyframable: &RwLock<KeyframableParams>, s: &Settings) -> bool {
    let fps = stab.params.read().get_scaled_fps();
    if !(fps > 0.0) { return false; }
    let (smoothing_changed, offset_changed) = {
        let mut kp = keyframable.write();
        let kf = &mut kp.cached_keyframes;
        let mut smoothing = set_keyframes(kf, &KeyframeType::LockHorizonAmount, &keyframes_for(&s.horizon_percent, fps));
        smoothing |= set_keyframes(kf, &KeyframeType::MaxZoom, &[(0, 0.0)]);
        // The crop solver's fovs are what is rendered, unscaled
        smoothing |= set_keyframes(kf, &KeyframeType::Fov, &[(0, 1.0)]);
        let mut offset = set_keyframes(kf, &KeyframeType::ZoomingCenterX, &keyframes_for(&s.offset.0, fps));
        offset |= set_keyframes(kf, &KeyframeType::ZoomingCenterY, &keyframes_for(&s.offset.1, fps));
        (smoothing, offset)
    };
    let mut smoothing_changed = smoothing_changed;
    if stab.smoothing.read().current().get_name() != SMOOTHING_METHOD_NONE_NAME {
        stab.set_smoothing_method(SMOOTHING_METHOD_NONE);
        smoothing_changed = true;
    }
    // The offset is in the keyframes
    if stab.params.read().adaptive_zoom_center_offset != (0.0, 0.0) {
        stab.set_zooming_center_x(0.0);
        stab.set_zooming_center_y(0.0);
    }
    if smoothing_changed {
        log::info!("Applying leveling {}", describe(&s.horizon_percent));
        // The setters above only invalidate for the app's background recompute
        stab.invalidate_blocking_smoothing();
    }
    if offset_changed {
        log::info!("Offset x {}, y {}", describe(&s.offset.0), describe(&s.offset.1));
        stab.invalidate_blocking_zooming();
    }
    smoothing_changed || offset_changed
}

/// A control's value, or its range when keyframed, for the log
pub fn describe(values: &[f64]) -> String {
    match values {
        [value] => format!("{value:.4}"),
        _ => {
            let (lo, hi) = values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
            format!("keyframed {lo:.4}..{hi:.4} over {} frames", values.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_map_to_core_ranges() {
        let s = Settings::constant(100.0, false, 4.0, 100.0, 0.0, 0.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom, s.offset), (vec![100.0], 4.0, vec![1.0], (vec![0.0], vec![0.0])));
        let s = Settings::constant(50.0, true, 1.0, 150.0, 50.0, -20.0);
        assert_eq!((s.horizon_percent, s.static_crop, s.crop_smoothness_s, s.zoom), (vec![50.0], true, 1.0, vec![1.5]));
        assert!((s.offset.0[0] - 0.25).abs() < 1e-12 && (s.offset.1[0] + 0.1).abs() < 1e-12);
    }

    #[test]
    fn controls_are_clamped_so_zoom_never_widens_the_crop() {
        let s = Settings::constant(250.0, false, 100.0, 20.0, 400.0, -400.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom), (vec![100.0], 8.0, vec![1.0]));
        assert_eq!(s.offset, (vec![0.25], vec![-0.25]));
        let s = Settings::constant(-5.0, false, 0.0, 1000.0, 0.0, 0.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom), (vec![0.0], 0.2, vec![4.0]));
        let nan = Settings::constant(f64::NAN, false, f64::NAN, f64::NAN, f64::NAN, f64::NAN);
        assert_eq!(nan, Settings::constant(DEFAULT_LEVELING, false, DEFAULT_CROP_SMOOTHNESS_S, DEFAULT_ZOOM_PERCENT, 0.0, 0.0));
    }

    #[test]
    fn keyframed_controls_map_every_frame() {
        let s = Settings::new(vec![0.0, 50.0, 120.0], false, 4.0, vec![100.0, 200.0, 50.0], vec![0.0, 10.0, -60.0], vec![0.0]);
        assert_eq!(s.horizon_percent, vec![0.0, 50.0, 100.0]);
        assert_eq!(s.zoom, vec![1.0, 2.0, 1.0]);
        assert_eq!(s.offset, (vec![0.0, 0.05, -0.25], vec![0.0]));
    }

    #[test]
    fn keyframes_are_one_or_one_per_frame() {
        assert_eq!(keyframes_for(&[42.0], 25.0), vec![(0, 42.0)]);
        assert_eq!(keyframes_for(&[1.0, 2.0, 3.0], 25.0), vec![(0, 1.0), (40_000, 2.0), (80_000, 3.0)]);
    }

    #[test]
    fn keyframes_are_only_replaced_when_they_differ() {
        let mut manager = KeyframeManager::new();
        let typ = KeyframeType::ZoomingCenterX;
        let curve = keyframes_for(&[0.0, 0.1, 0.2], 25.0);
        assert!(set_keyframes(&mut manager, &typ, &curve));
        assert!(!set_keyframes(&mut manager, &typ, &curve));
        assert_eq!(manager.value_at_video_timestamp(&typ, 40.0), Some(0.1));
        // Back to one value: the per-frame keyframes go
        assert!(set_keyframes(&mut manager, &typ, &[(0, 0.3)]));
        assert_eq!(manager.get_keyframes(&typ).map(|k| k.len()), Some(1));
        assert_eq!(manager.value_at_video_timestamp(&typ, 80.0), Some(0.3));
    }
}
