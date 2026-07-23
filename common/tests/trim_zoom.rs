// Verifies that setting trim ranges + invalidate_blocking_smoothing + recompute
// actually relaxes the adaptive zoom solve in gyroflow-core, end-to-end with a real
// video file. Run manually with:
//   GF_TEST_VIDEO=/path/to/clip.MP4 cargo test --release --test trim_zoom -- --ignored --nocapture
use gyroflow_plugin_base::gyroflow_core::{ StabilizationManager, filesystem };

#[test]
#[ignore]
fn trim_relaxes_zoom() {
    let path = std::env::var("GF_TEST_VIDEO").expect("set GF_TEST_VIDEO to a video file with gyro data");
    let stab = StabilizationManager::default();
    let url = filesystem::path_to_url(&path);
    let mut file = filesystem::open_file(&url, false, false).unwrap();
    let filesize = file.size;
    stab.load_video_file(file.get_file(), filesize, &url, None, true).unwrap();
    {
        let mut p = stab.params.write();
        let size = p.size;
        p.output_size = size;
    }
    stab.init_size();
    stab.recompute_blocking();
    let full: Vec<f64> = stab.params.read().fovs.clone();
    assert!(!full.is_empty(), "no fovs computed for the full clip");

    // Trim off the last 10% (e.g. a camera rotation at the end that forces heavy zoom)
    stab.set_trim_ranges(vec![(0.0, 0.9)]);
    stab.invalidate_blocking_smoothing();
    stab.recompute_blocking();
    let trimmed: Vec<f64> = stab.params.read().fovs.clone();
    assert!(!trimmed.is_empty(), "no fovs computed for the trimmed clip");

    let n = full.len().min(trimmed.len());
    let kept = n * 9 / 10;
    let min_full = full[..kept].iter().cloned().fold(f64::MAX, f64::min);
    let min_trim = trimmed[..kept].iter().cloned().fold(f64::MAX, f64::min);
    let avg_full = full[..kept].iter().sum::<f64>() / kept as f64;
    let avg_trim = trimmed[..kept].iter().sum::<f64>() / kept as f64;
    println!("fov count: full={} trimmed={}", full.len(), trimmed.len());
    println!("within kept range — min fov: full={min_full:.4} trimmed={min_trim:.4} (higher = less zoom)");
    println!("within kept range — avg fov: full={avg_full:.4} trimmed={avg_trim:.4}");
    assert!(min_trim >= min_full - 1e-9, "trimmed solve must never zoom in more than the full solve");
    assert!(min_trim > min_full + 1e-6 || avg_trim > avg_full + 1e-6,
        "zoom did not relax after trimming — trim_ranges may not affect the zoom solve");
}
