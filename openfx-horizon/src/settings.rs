//! The Horizon Leveling controls and how they drive gyroflow-core.
//!
//! Only the roll is corrected: the core runs without smoothing, so the one rotation left is the
//! horizon lock, on a distortion-free lens (see `cameras::pinhole_profile_json`). The crop solver
//! (`gyroflow_plugin_base::crop`) fits the frame inside the rotated source.

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Horizon lock amount, 0..=100
    pub horizon_percent: f64,
    pub static_crop: bool,
    pub crop_smoothness_s: f64,
    /// Zoom in on top of the largest crop, 1..=4
    pub zoom: f64,
    /// Frame centre offset as a fraction of the source size, the core's `adaptive_zoom_center_offset`
    pub offset: (f64, f64),
}

impl Settings {
    pub fn new(leveling: f64, static_crop: bool, crop_smoothness_s: f64, zoom_percent: f64, offset_x: f64, offset_y: f64) -> Self {
        let clamp = |v: f64, min: f64, max: f64, default: f64| if v.is_finite() { v.clamp(min, max) } else { default };
        let offset = |v: f64| clamp(v, -MAX_OFFSET_PERCENT, MAX_OFFSET_PERCENT, 0.0) / 200.0;
        Self {
            horizon_percent: clamp(leveling, 0.0, 100.0, DEFAULT_LEVELING),
            static_crop,
            crop_smoothness_s: clamp(crop_smoothness_s, MIN_CROP_SMOOTHNESS_S, MAX_CROP_SMOOTHNESS_S, DEFAULT_CROP_SMOOTHNESS_S),
            zoom: clamp(zoom_percent, DEFAULT_ZOOM_PERCENT, MAX_ZOOM_PERCENT, DEFAULT_ZOOM_PERCENT) / 100.0,
            offset: (offset(offset_x), offset(offset_y)),
        }
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

/// Writes the settings into the plugin's keyframe cache (which the core reads through the keyframe
/// provider) and the core's parameters. Returns true when something changed.
pub fn apply(stab: &StabilizationManager, keyframable: &RwLock<KeyframableParams>, s: &Settings) -> bool {
    let mut changed = false;
    {
        let mut kp = keyframable.write();
        for (typ, value) in [
            (KeyframeType::LockHorizonAmount, s.horizon_percent),
            (KeyframeType::MaxZoom, 0.0),
            // The crop solver's fovs are what is rendered, unscaled
            (KeyframeType::Fov, 1.0),
        ] {
            let current = kp.cached_keyframes.value_at_video_timestamp(&typ, 0.0);
            if current.map_or(true, |c| (c - value).abs() > 1e-6) {
                kp.cached_keyframes.set(&typ, 0, value);
                changed = true;
            }
        }
    }
    if stab.smoothing.read().current().get_name() != SMOOTHING_METHOD_NONE_NAME {
        stab.set_smoothing_method(SMOOTHING_METHOD_NONE);
        changed = true;
    }
    if changed {
        log::info!("Applying {s:?}");
        // The setters above only invalidate for the app's background recompute
        stab.invalidate_blocking_smoothing();
    }
    let current = stab.params.read().adaptive_zoom_center_offset;
    if (current.0 - s.offset.0).abs() > 1e-9 || (current.1 - s.offset.1).abs() > 1e-9 {
        log::info!("Offset {current:.4?} -> {:.4?}", s.offset);
        stab.set_zooming_center_x(s.offset.0);
        stab.set_zooming_center_y(s.offset.1);
        stab.invalidate_blocking_zooming();
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_map_to_core_ranges() {
        let s = Settings::new(100.0, false, 4.0, 100.0, 0.0, 0.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom, s.offset), (100.0, 4.0, 1.0, (0.0, 0.0)));
        let s = Settings::new(50.0, true, 1.0, 150.0, 50.0, -20.0);
        assert_eq!((s.horizon_percent, s.static_crop, s.crop_smoothness_s, s.zoom), (50.0, true, 1.0, 1.5));
        assert!((s.offset.0 - 0.25).abs() < 1e-12 && (s.offset.1 + 0.1).abs() < 1e-12);
    }

    #[test]
    fn controls_are_clamped_so_zoom_never_widens_the_crop() {
        let s = Settings::new(250.0, false, 100.0, 20.0, 400.0, -400.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom), (100.0, 8.0, 1.0));
        assert_eq!(s.offset, (0.25, -0.25));
        let s = Settings::new(-5.0, false, 0.0, 1000.0, 0.0, 0.0);
        assert_eq!((s.horizon_percent, s.crop_smoothness_s, s.zoom), (0.0, 0.2, 4.0));
        let nan = Settings::new(f64::NAN, false, f64::NAN, f64::NAN, f64::NAN, f64::NAN);
        assert_eq!(nan, Settings::new(DEFAULT_LEVELING, false, DEFAULT_CROP_SMOOTHNESS_S, DEFAULT_ZOOM_PERCENT, 0.0, 0.0));
    }
}
