//! The simple plugin's controls and how they drive gyroflow-core.
//!
//! The panel has a few user-facing controls; everything else is fixed here so the
//! result doesn't depend on presets saved by the Gyroflow app.

use gyroflow_plugin_base::*;
use gyroflow_plugin_base::parking_lot::RwLock;

pub const DEFAULT_STABILIZATION: f64 = 85.0;
pub const DEFAULT_HORIZON_LEVEL: f64 = 0.9;

/// Seconds the crop takes to zoom by 2x (see `crop.rs`)
pub const DEFAULT_ZOOM_RESPONSE_S: f64 = 4.0;
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
    pub zoom_response_seconds: f64,
}

impl Settings {
    pub fn new(stabilization: f64, horizon_level: f64, zoom_response_seconds: f64) -> Self {
        let stabilization = if stabilization.is_finite() { stabilization.clamp(0.0, 100.0) } else { DEFAULT_STABILIZATION };
        let horizon_level = if horizon_level.is_finite() { horizon_level.clamp(0.0, 1.0) } else { DEFAULT_HORIZON_LEVEL };
        let zoom_response_seconds = if zoom_response_seconds.is_finite() { zoom_response_seconds.clamp(0.2, 8.0) } else { DEFAULT_ZOOM_RESPONSE_S };
        Self {
            smoothness: (stabilization / 100.0).max(0.001),
            horizon_percent: horizon_level * 100.0,
            zoom_response_seconds,
        }
    }
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
    let (max_zoom, iterations, window, method) = {
        let p = stab.params.read();
        (p.max_zoom, p.max_zoom_iterations, p.adaptive_zoom_window, p.adaptive_zoom_method)
    };
    // `set_max_zoom(0.0)` turns the limit off, which reads back as None
    if max_zoom.is_some() || iterations != MAX_ZOOM_ITERATIONS {
        stab.set_max_zoom(0.0, MAX_ZOOM_ITERATIONS);
        changed = true;
    }
    if (window - s.zoom_response_seconds).abs() > 1e-6 {
        stab.set_adaptive_zoom(s.zoom_response_seconds);
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

/// The crop solver fits the rectangle around the displaced center, cropping more where it has to.
pub fn apply_position(stab: &StabilizationManager, x_percent: f64, y_percent: f64) -> bool {
    let current = stab.params.read().adaptive_zoom_center_offset;
    let clamp = |v: f64| if v.is_finite() { v.clamp(-100.0, 100.0) } else { 0.0 };
    let target = (clamp(x_percent) / 5000.0, clamp(y_percent) / 5000.0);
    if (current.0 - target.0).abs() <= 1e-6 && (current.1 - target.1).abs() <= 1e-6 {
        return false;
    }
    log::info!("Reframe {x_percent:.1}%/{y_percent:.1}%: offset {current:.4?} -> {target:.4?}");
    stab.set_zooming_center_x(target.0);
    stab.set_zooming_center_y(target.1);
    stab.invalidate_blocking_zooming();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_map_to_core_ranges() {
        let s = Settings::new(85.0, 0.9, 4.0);
        assert!((s.smoothness - 0.85).abs() < 1e-9);
        assert!((s.horizon_percent - 90.0).abs() < 1e-9);
        let clamped = Settings::new(250.0, 3.0, 100.0);
        assert_eq!((clamped.smoothness, clamped.horizon_percent), (1.0, 100.0));
        assert_eq!(clamped.zoom_response_seconds, 8.0);
        let off = Settings::new(-5.0, -1.0, 0.0);
        assert_eq!((off.smoothness, off.horizon_percent), (0.001, 0.0));
        assert_eq!(off.zoom_response_seconds, 0.2);
        let nan = Settings::new(f64::NAN, f64::NAN, f64::NAN);
        assert_eq!((nan.smoothness, nan.horizon_percent), (0.85, 90.0));
        assert_eq!(nan.zoom_response_seconds, DEFAULT_ZOOM_RESPONSE_S);
    }

    #[test]
    fn unchanged_settings_are_not_applied_again() {
        let stab = StabilizationManager::default();
        let keyframable = RwLock::new(KeyframableParams { use_gyroflows_keyframes: false, cached_keyframes: KeyframeManager::default() });
        let s = Settings::new(85.0, 0.9, 4.0);
        assert!(apply(&stab, &keyframable, &s));
        // Applying again recomputes the stabilization and solves the crop at every rendered frame
        assert!(!apply(&stab, &keyframable, &s));
        assert!(apply(&stab, &keyframable, &Settings::new(50.0, 0.9, 4.0)));
    }

}
