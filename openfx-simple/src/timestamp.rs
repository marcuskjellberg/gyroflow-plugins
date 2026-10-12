/// Pure timestamp computation for the Render action, extracted so it can be unit tested
/// without an OFX host.
///
/// Two modes:
/// - **Host-provided source frame** (`src_frame`, Resolve 20+ `kOfxImageEffectPropSrcFrame`):
///   the source-media frame index is the time base. The clip's trim in Resolve is inherently
///   respected and the `speed_stretch` heuristic is skipped entirely (it derives a bogus
///   ratio when the clip is trimmed, e.g. ~90x for a 20s section of a 30min file).
/// - **Legacy** (no `src_frame`): bit-identical to the historical behavior, including the
///   `speed_stretch` heuristic and its ±3% whitelist.
pub struct TimeParams {
    /// Timeline time of the render, in frames (OFX `kOfxPropTime`)
    pub time: f64,
    /// Source clip frame range (`kOfxImageEffectPropFrameRange`), if available
    pub frame_range: Option<(f64, f64)>, // (min, max)
    /// Source clip frame rate reported by the host
    pub src_fps: f64,
    /// Frame rate of the gyroflow project / video file
    pub fps: f64,
    /// Duration of the gyroflow project / video file in milliseconds
    pub duration_ms: f64,
    /// Source-media frame index for this render (Resolve 20+ extension), if available
    pub src_frame: Option<i64>,
    pub is_fusion_page: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputedTime {
    /// Host time at which to fetch the source image (may differ from input time when
    /// a Gyroflow-internal speed ramp remaps the frame)
    pub fetch_time: f64,
    /// Gyro data timestamp for this frame
    pub timestamp_us: i64,
    pub speed_stretch: f64,
    /// Detected offset of the clip's first frame within the source media, in source frames.
    /// Only available when the host provides `src_frame`. Used to feed gyroflow-core's
    /// trim ranges so adaptive zoom is solved over the visible sub-clip only.
    pub trim_offset_frames: Option<f64>,
}

pub fn compute_timestamp(p: &TimeParams, ramp: impl Fn(i64) -> i64) -> ComputedTime {
    if !p.time.is_finite() || !p.src_fps.is_finite() || p.src_fps <= 0.0 || !p.fps.is_finite() || p.fps <= 0.0 {
        return ComputedTime {
            fetch_time: if p.time.is_finite() { p.time } else { 0.0 },
            timestamp_us: 0,
            speed_stretch: 1.0,
            trim_offset_frames: None,
        };
    }

    let mut speed_stretch = 1.0;
    let mut time_adj = 0.0;
    if let Some((min, max)) = p.frame_range {
        if p.is_fusion_page {
            time_adj = min;
        }
        // The stretch heuristic compares clip duration to media duration, which is only
        // meaningful when the clip isn't trimmed. With a host-provided source frame the
        // real time base is known, so skip it.
        if max > 0.0 && !p.is_fusion_page && p.src_frame.is_none() {
            let duration_at_src_fps = (max / p.src_fps) * 1000.0;
            speed_stretch = ((p.duration_ms.round() / duration_at_src_fps.round()) * 100.0).floor() / 100.0;
        }
    }

    // This should cover most cases by default, and for the rest users will use Fusion
    #[allow(clippy::float_cmp)]
    if speed_stretch == 1.01 || speed_stretch == 0.99 || speed_stretch == 1.02 || speed_stretch == 0.98 || speed_stretch == 1.03 || speed_stretch == 0.97 {
        speed_stretch = 1.0;
    }

    let fps_mismatch = (p.src_fps - p.fps).abs() > 0.01;

    let mut time = p.time - time_adj;
    let mut timestamp_us = ((time / p.src_fps * 1_000_000.0) * speed_stretch).round() as i64;
    if fps_mismatch {
        let frame = (time / p.src_fps) * p.fps * speed_stretch;
        timestamp_us = (frame.floor() * (1_000_000.0 / p.fps)).round() as i64;
    }

    let mut trim_offset_frames = None;
    if let Some(src_frame) = p.src_frame {
        timestamp_us = (src_frame as f64 * (1_000_000.0 / p.fps)).round() as i64;
        let range_min = p.frame_range.map(|(min, _)| min).unwrap_or(0.0);
        trim_offset_frames = Some(src_frame as f64 - (p.time - range_min));
    }

    let source_timestamp_us = ramp(timestamp_us);
    if source_timestamp_us != timestamp_us {
        if let Some(src_frame) = p.src_frame {
            // The host-provided source frame is the time base: shift it by the ramped delta
            // directly instead of round-tripping through timeline time and `speed_stretch`
            // (which would discard the source frame and reintroduce the trim error).
            let new_src_frame = (source_timestamp_us as f64 / 1_000_000.0 * p.fps).round();
            time += new_src_frame - src_frame as f64;
            timestamp_us = (new_src_frame * (1_000_000.0 / p.fps)).round() as i64;
        } else {
            time = (source_timestamp_us as f64 / speed_stretch / 1_000_000.0 * p.src_fps).round();
            timestamp_us = ((time / p.src_fps * 1_000_000.0) * speed_stretch).round() as i64;
            if fps_mismatch {
                let frame = (time / p.src_fps) * p.fps * speed_stretch;
                timestamp_us = (frame.floor() * (1_000_000.0 / p.fps)).round() as i64;
            }
        }
    }

    ComputedTime {
        fetch_time: time + time_adj,
        timestamp_us,
        speed_stretch,
        trim_offset_frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation of the historical (pre-src_frame) behavior,
    /// transcribed verbatim from the original Render action. Legacy-path results
    /// must be bit-identical to this.
    fn legacy_reference(p: &TimeParams, ramp: impl Fn(i64) -> i64) -> (f64, i64, f64) {
        let mut speed_stretch = 1.0;
        let mut time_adj = 0.0;
        if let Some((min, max)) = p.frame_range {
            if p.is_fusion_page { time_adj = min; }
            if max > 0.0 && !p.is_fusion_page {
                let duration_at_src_fps = (max / p.src_fps) * 1000.0;
                speed_stretch = ((p.duration_ms.round() / duration_at_src_fps.round()) * 100.0).floor() / 100.0;
            }
        }
        #[allow(clippy::float_cmp)]
        if speed_stretch == 1.01 || speed_stretch == 0.99 || speed_stretch == 1.02 || speed_stretch == 0.98 || speed_stretch == 1.03 || speed_stretch == 0.97 {
            speed_stretch = 1.0;
        }
        let mut time = p.time - time_adj;
        let mut timestamp_us = ((time / p.src_fps * 1_000_000.0) * speed_stretch).round() as i64;
        if (p.src_fps - p.fps).abs() > 0.01 {
            let frame = (time / p.src_fps) * p.fps * speed_stretch;
            timestamp_us = (frame.floor() * (1_000_000.0 / p.fps)).round() as i64;
        }
        let source_timestamp_us = ramp(timestamp_us);
        if source_timestamp_us != timestamp_us {
            time = (source_timestamp_us as f64 / speed_stretch / 1_000_000.0 * p.src_fps).round();
            timestamp_us = ((time / p.src_fps * 1_000_000.0) * speed_stretch).round() as i64;
            if (p.src_fps - p.fps).abs() > 0.01 {
                let frame = (time / p.src_fps) * p.fps * speed_stretch;
                timestamp_us = (frame.floor() * (1_000_000.0 / p.fps)).round() as i64;
            }
        }
        (time + time_adj, timestamp_us, speed_stretch)
    }

    fn no_ramp(t: i64) -> i64 { t }

    #[test]
    fn legacy_untrimmed_identity() {
        // Untrimmed 60s clip @ 29.97, matching fps — must match reference exactly
        for frame in [0.0, 1.0, 100.0, 1798.0] {
            let p = TimeParams {
                time: frame, frame_range: Some((0.0, 1799.0)),
                src_fps: 29.97, fps: 29.97, duration_ms: 60_060.0,
                src_frame: None, is_fusion_page: false,
            };
            let c = compute_timestamp(&p, no_ramp);
            let (rt, rts, rss) = legacy_reference(&p, no_ramp);
            assert_eq!(c.fetch_time, rt);
            assert_eq!(c.timestamp_us, rts);
            assert_eq!(c.speed_stretch, rss);
            assert_eq!(c.trim_offset_frames, None);
        }
    }

    #[test]
    fn legacy_whitelist_stretch_values() {
        // Durations that produce speed_stretch of exactly 0.97..1.03 must snap to 1.0
        // range.max=1000 @ 25fps => duration_at_src_fps = 40000ms
        for (dur, expected) in [
            (40_400.0, 1.0),  // 1.01 -> snapped
            (39_600.0, 1.0),  // 0.99 -> snapped
            (41_200.0, 1.0),  // 1.03 -> snapped
            (38_800.0, 1.0),  // 0.97 -> snapped
            (48_000.0, 1.2),  // 1.2  -> kept
            (20_000.0, 0.5),  // 0.5  -> kept
        ] {
            let p = TimeParams {
                time: 10.0, frame_range: Some((0.0, 1000.0)),
                src_fps: 25.0, fps: 25.0, duration_ms: dur,
                src_frame: None, is_fusion_page: false,
            };
            let c = compute_timestamp(&p, no_ramp);
            let (_, rts, rss) = legacy_reference(&p, no_ramp);
            assert_eq!(c.speed_stretch, expected, "duration_ms={dur}");
            assert_eq!(c.speed_stretch, rss);
            assert_eq!(c.timestamp_us, rts);
        }
    }

    #[test]
    fn legacy_fps_mismatch_matches_reference() {
        let p = TimeParams {
            time: 50.0, frame_range: Some((0.0, 500.0)),
            src_fps: 25.0, fps: 50.0, duration_ms: 20_000.0,
            src_frame: None, is_fusion_page: false,
        };
        let c = compute_timestamp(&p, no_ramp);
        let (rt, rts, rss) = legacy_reference(&p, no_ramp);
        assert_eq!(c.fetch_time, rt);
        assert_eq!(c.timestamp_us, rts);
        assert_eq!(c.speed_stretch, rss);
    }

    #[test]
    fn legacy_fusion_page_time_adj() {
        let p = TimeParams {
            time: 120.0, frame_range: Some((100.0, 400.0)),
            src_fps: 30.0, fps: 30.0, duration_ms: 10_000.0,
            src_frame: None, is_fusion_page: true,
        };
        let c = compute_timestamp(&p, no_ramp);
        let (rt, rts, rss) = legacy_reference(&p, no_ramp);
        assert_eq!(c.fetch_time, rt);
        assert_eq!(c.timestamp_us, rts);
        assert_eq!(c.speed_stretch, rss);
        // frame 120 with comp starting at 100 => 20 frames into clip
        assert_eq!(c.timestamp_us, ((20.0 / 30.0) * 1_000_000.0_f64).round() as i64);
    }

    #[test]
    fn legacy_ramped_matches_reference() {
        // A ramp that doubles the source time
        let ramp = |t: i64| t * 2;
        let p = TimeParams {
            time: 30.0, frame_range: Some((0.0, 300.0)),
            src_fps: 30.0, fps: 30.0, duration_ms: 10_000.0,
            src_frame: None, is_fusion_page: false,
        };
        let c = compute_timestamp(&p, ramp);
        let (rt, rts, rss) = legacy_reference(&p, ramp);
        assert_eq!(c.fetch_time, rt);
        assert_eq!(c.timestamp_us, rts);
        assert_eq!(c.speed_stretch, rss);
    }

    #[test]
    fn src_frame_trimmed_clip() {
        // 30-minute source (54000 frames @ 30fps), 20s section (600 frames) starting at
        // source frame 30000, on the Edit page (frame_range = clip length).
        // Old behavior would compute speed_stretch = 1800000/20000 = 90.
        let p = TimeParams {
            time: 10.0, frame_range: Some((0.0, 600.0)),
            src_fps: 30.0, fps: 30.0, duration_ms: 1_800_000.0,
            src_frame: Some(30_010), is_fusion_page: false,
        };
        let c = compute_timestamp(&p, no_ramp);
        assert_eq!(c.speed_stretch, 1.0, "stretch heuristic must be disabled with src_frame");
        assert_eq!(c.timestamp_us, ((30_010.0 / 30.0) * 1_000_000.0_f64).round() as i64);
        assert_eq!(c.trim_offset_frames, Some(30_000.0));
        assert_eq!(c.fetch_time, 10.0);
    }

    #[test]
    fn src_frame_untrimmed_clip() {
        let p = TimeParams {
            time: 42.0, frame_range: Some((0.0, 600.0)),
            src_fps: 30.0, fps: 30.0, duration_ms: 20_000.0,
            src_frame: Some(42), is_fusion_page: false,
        };
        let c = compute_timestamp(&p, no_ramp);
        assert_eq!(c.trim_offset_frames, Some(0.0));
        assert_eq!(c.timestamp_us, ((42.0 / 30.0) * 1_000_000.0_f64).round() as i64);
    }

    #[test]
    fn src_frame_with_ramp_keeps_source_base() {
        // Trimmed clip + gyroflow speed ramp: the ramped remap must shift the
        // source-frame base, not round-trip through timeline time.
        let offset_us = |frames: f64| ((frames / 30.0) * 1_000_000.0_f64).round() as i64;
        let ramp = |t: i64| t + offset_us(30.0); // ramp shifts forward by 30 source frames
        let p = TimeParams {
            time: 10.0, frame_range: Some((0.0, 600.0)),
            src_fps: 30.0, fps: 30.0, duration_ms: 1_800_000.0,
            src_frame: Some(30_010), is_fusion_page: false,
        };
        let c = compute_timestamp(&p, ramp);
        assert_eq!(c.timestamp_us, offset_us(30_040.0));
        // fetch time shifts by the same 30 frames in timeline space
        assert_eq!(c.fetch_time, 40.0);
        assert_eq!(c.speed_stretch, 1.0);
        assert_eq!(c.trim_offset_frames, Some(30_000.0));
    }

    #[test]
    fn no_frame_range_at_all() {
        let p = TimeParams {
            time: 5.0, frame_range: None,
            src_fps: 24.0, fps: 24.0, duration_ms: 8_000.0,
            src_frame: None, is_fusion_page: false,
        };
        let c = compute_timestamp(&p, no_ramp);
        assert_eq!(c.speed_stretch, 1.0);
        assert_eq!(c.timestamp_us, ((5.0 / 24.0) * 1_000_000.0_f64).round() as i64);
    }

    #[test]
    fn invalid_timing_metadata_has_a_bounded_fallback() {
        for (time, src_fps, fps) in [
            (5.0, 0.0, 24.0),
            (5.0, 24.0, f64::NAN),
            (f64::INFINITY, 24.0, 24.0),
        ] {
            let p = TimeParams {
                time,
                frame_range: Some((0.0, 100.0)),
                src_fps,
                fps,
                duration_ms: 4_000.0,
                src_frame: Some(10),
                is_fusion_page: false,
            };
            let c = compute_timestamp(&p, no_ramp);
            assert!(c.fetch_time.is_finite());
            assert_eq!(c.timestamp_us, 0);
            assert_eq!(c.speed_stretch, 1.0);
            assert_eq!(c.trim_offset_frames, None);
        }
    }
}
