//! Predefined cameras for the simplified plugin.
//!
//! Each camera is matched by the model name stored in the clip's metadata (GoPro `DVNM`),
//! and each lens mode points at the lens profile to apply. Lens modes without a profile
//! yet are listed so they can be selected, but nothing is applied for them.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensMode {
    pub name: &'static str,
    /// Lens profile JSON embedded in the plugin, if one exists yet.
    pub profile_json: Option<&'static str>,
    /// `identifier` field of the embedded profile, used to recognize it once it is loaded.
    pub identifier: &'static str,
    /// Full-frame rolling shutter readout time in milliseconds. Provisional until measured.
    pub readout_time_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub name: &'static str,
    /// Model name as it appears in the clip metadata. Compared case-insensitively.
    pub metadata_model: &'static str,
    pub lens_modes: &'static [LensMode],
}

pub const CAMERAS: &[Camera] = &[
    Camera {
        name: "GoPro MISSION 1 PRO",
        metadata_model: "MISSION 1 PRO",
        lens_modes: &[
            LensMode {
                name: "Linear",
                profile_json: Some(include_str!("../res/lens_profiles/GoPro Mission 1 Pro Linear 4000x3000.json")),
                identifier: "gopro-mission1pro-linear-4000x3000@23976-no-eis",
                readout_time_ms: 4.0,
            },
            LensMode {
                name: "Wide",
                profile_json: None,
                identifier: "",
                readout_time_ms: 4.0,
            },
        ],
    },
    // Placeholder: no calibrated profile yet, and the metadata model name is unverified.
    Camera {
        name: "DJI Osmo Nano",
        metadata_model: "OSMO NANO",
        lens_modes: &[LensMode {
            name: "Default",
            profile_json: None,
            identifier: "",
            readout_time_ms: 0.0,
        }],
    },
];

/// Finds the camera whose metadata model name matches `model`, ignoring case and surrounding spaces.
pub fn find_camera(model: &str) -> Option<&'static Camera> {
    let model = model.trim();
    CAMERAS.iter().find(|c| c.metadata_model.eq_ignore_ascii_case(model))
}

/// Returns the predefined lens mode for a camera model and lens model reported by the core,
/// if that mode has an embedded profile.
pub fn predefined_lens_for(camera_model: &str, lens_model: &str) -> Option<&'static LensMode> {
    let camera = find_camera(camera_model)?;
    find_lens_mode(camera, lens_model).filter(|mode| mode.profile_json.is_some())
}

/// Finds a lens mode of `camera` by name, ignoring case.
pub fn find_lens_mode(camera: &Camera, name: &str) -> Option<&'static LensMode> {
    camera.lens_modes.iter().find(|m| m.name.eq_ignore_ascii_case(name))
}

/// Identifier prefix of [`pinhole_profile_json`]'s profiles
pub const PINHOLE_IDENTIFIER: &str = "gyroflow-plugin-pinhole";

/// A lens without distortion, centred on a `width`x`height` frame and read out at once.
///
/// A roll about the optical axis is a rotation of the image about its centre for any radially
/// symmetric lens, so with this profile the core's horizon lock levels footage of any lens mode
/// without knowing its distortion.
pub fn pinhole_profile_json(width: usize, height: usize) -> String {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    serde_json::json!({
        "name": format!("Pinhole {width}x{height}"),
        "camera_brand": "", "camera_model": "", "lens_model": "Pinhole",
        "calib_dimension": { "w": width, "h": height },
        "orig_dimension": { "w": width, "h": height },
        "output_dimension": { "w": width, "h": height },
        "frame_readout_time": 0.0,
        "input_horizontal_stretch": 1.0,
        "input_vertical_stretch": 1.0,
        "num_images": 0,
        "official": false,
        "asymmetrical": false,
        "fisheye_params": {
            "RMS_error": 0.0,
            "camera_matrix": [[w, 0.0, w / 2.0], [0.0, w, h / 2.0], [0.0, 0.0, 1.0]],
            "distortion_coeffs": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            "radial_distortion_limit": null
        },
        "distortion_model": "opencv_standard",
        "identifier": format!("{PINHOLE_IDENTIFIER}-{width}x{height}"),
        "compatible_settings": []
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinhole_profile_loads_centred_and_without_distortion() {
        let mut lens = gyroflow_core::lens_profile::LensProfile::default();
        let value: serde_json::Value = serde_json::from_str(&pinhole_profile_json(4000, 3000)).unwrap();
        lens.load_from_json_value(&value).unwrap();
        assert_eq!(lens.identifier, "gyroflow-plugin-pinhole-4000x3000");
        assert_eq!(lens.distortion_model.as_deref(), Some("opencv_standard"));
        assert!(lens.fisheye_params.distortion_coeffs.iter().all(|k| *k == 0.0));
        let m = lens.get_camera_matrix((4000, 3000));
        assert_eq!((m[(0, 2)], m[(1, 2)]), (2000.0, 1500.0));
    }

    #[test]
    fn finds_mission_1_pro_from_metadata_model() {
        let camera = find_camera("MISSION 1 PRO").expect("MISSION 1 PRO should be predefined");
        assert_eq!(camera.name, "GoPro MISSION 1 PRO");
    }

    #[test]
    fn model_match_ignores_case_and_surrounding_spaces() {
        assert!(find_camera("  mission 1 pro ").is_some());
    }

    #[test]
    fn unknown_model_is_not_predefined() {
        assert!(find_camera("HERO12 Black").is_none());
    }

    #[test]
    fn linear_mode_has_profile_and_wide_mode_does_not() {
        let camera = find_camera("MISSION 1 PRO").unwrap();
        let linear = find_lens_mode(camera, "linear").expect("Linear mode should exist");
        assert!(linear.profile_json.is_some());
        let wide = find_lens_mode(camera, "Wide").expect("Wide mode should exist");
        assert!(wide.profile_json.is_none());
    }

    #[test]
    fn linear_profile_is_valid_json_for_mission_1_pro() {
        let camera = find_camera("MISSION 1 PRO").unwrap();
        let linear = find_lens_mode(camera, "Linear").unwrap();
        let profile: serde_json::Value = serde_json::from_str(linear.profile_json.unwrap()).unwrap();
        assert_eq!(profile["camera_model"], "MISSION 1 PRO");
        assert_eq!(profile["lens_model"], "Linear");
    }

    #[test]
    fn predefined_lens_applies_only_to_mission_linear() {
        assert!(predefined_lens_for("MISSION 1 PRO", "Linear").is_some());
        assert!(predefined_lens_for("MISSION 1 PRO", "Wide").is_none());
        assert!(predefined_lens_for("HERO12 Black", "Linear").is_none());
    }

    #[test]
    fn embedded_profile_identifier_matches_table() {
        let mode = predefined_lens_for("MISSION 1 PRO", "Linear").unwrap();
        let profile: serde_json::Value = serde_json::from_str(mode.profile_json.unwrap()).unwrap();
        assert_eq!(profile["identifier"], mode.identifier);
    }

    #[test]
    fn unknown_lens_mode_is_none() {
        let camera = find_camera("MISSION 1 PRO").unwrap();
        assert!(find_lens_mode(camera, "Superview").is_none());
    }
}
