//! The crop as an optimization with a hard constraint: the largest output rectangle that only ever
//! shows source pixels.
//!
//! Per frame, the bound is the largest rectangle of the output's aspect, centred on the reframe offset,
//! inside the stabilized outline of the source (inset by the interpolation's reach). Over time, the zoom
//! is the largest curve under those bounds whose log changes by at most `ln 2` per response time:
//! `ln f(t) = min_s (ln F(s) + r·|t - s|)`. Every frame of it is then checked with the renderer's own
//! map (`Stabilization::undistort_coord`); a frame that fails gets a bisected bound and the curve is
//! solved again. A rectangle that passes holds for every smaller zoom, as the rectangles are nested.

use crate::*;
use gyroflow_core::{stabilization_params::StabilizationParams, zooming};
use nalgebra::Vector2;
use rayon::prelude::*;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Weak};

/// Points per side of the source outline mapped into the output plane
const OUTLINE_POINTS: usize = 64;
/// Output-plane pixels between checked points on the frame's edge
const CHECK_SPACING: f64 = 16.0;
/// Interior points per axis, so a map that folds can't pass on its edge alone
const CHECK_GRID: usize = 8;
/// Float error and curvature between checked points, in source pixels
const TOLERANCE_PX: f64 = 0.05;
/// The renderer's smallest fov (`FrameTransform::get_fov`)
const MIN_FOV: f64 = 0.001;
const MAX_ROUNDS: usize = 6;

#[derive(Clone, Debug, PartialEq)]
pub struct CropRequest {
    /// Frames the crop has to hold for, inclusive
    pub frames: (usize, usize),
    /// Source pixels the interpolation reads past a sample, per axis
    pub margin: (f64, f64),
    pub response_s: f64,
    pub input_rotation: f32,
    /// One zoom for all of `frames`, the tightest bound's, instead of a curve that follows them
    pub static_crop: bool,
    /// Extra zoom in on top of the crop, at least 1, per frame of the clip; one value holds for every frame.
    /// Applied after the curve, so keyframed zoom is followed as it is
    pub zoom: Vec<f64>,
}

impl Default for CropRequest {
    fn default() -> Self {
        Self { frames: (0, 0), margin: (0.0, 0.0), response_s: 4.0, input_rotation: 0.0, static_crop: false, zoom: vec![1.0] }
    }
}

impl CropRequest {
    /// The extra zoom at a frame of the clip
    pub fn zoom_at(&self, frame: usize) -> f64 {
        let zoom = match self.zoom.len() {
            0 => 1.0,
            n => self.zoom[frame.min(n - 1)],
        };
        if zoom.is_finite() { zoom.max(1.0) } else { 1.0 }
    }
}

// Render only needs the solve's log line; the real-clip test reads the rest
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct CropReport {
    pub frames: (usize, usize),
    /// Per frame of `frames`, as fovs (rectangle width / source width): the bound and the applied zoom
    pub bounds: Vec<f64>,
    pub fovs: Vec<f64>,
    /// Per frame of `frames`: how far inside the usable source area the closest sample is, in source pixels
    pub slack: Vec<f64>,
    /// Frames whose outline bound failed the render check, and the largest overshoot, in source pixels
    pub corrected: usize,
    pub overshoot: f64,
    pub elapsed_ms: f64,
}

/// Source pixels the renderer's interpolation reads past a sample, per axis, for a source of `size`
/// drawn from `rect` (x, y, w, h) of the host's buffer
pub fn source_margin(
    interpolation: Interpolation,
    size: (usize, usize),
    rect: (usize, usize, usize, usize),
) -> (f64, f64) {
    // Taps on the far side of the sample (`sample_input_at`; the EWA filters reach 3 while not downscaling), plus the 1/32 px coefficient table
    let reach = match interpolation {
        Interpolation::Bilinear => 1.0,
        Interpolation::Bicubic => 2.0,
        _ => 4.0,
    } + 1.0 / 32.0;
    (
        reach * size.0 as f64 / rect.2.max(1) as f64 + TOLERANCE_PX,
        reach * size.1 as f64 / rect.3.max(1) as f64 + TOLERANCE_PX,
    )
}

/// The frame `StabilizationManager::process_pixels` renders `timestamp_us` as
pub fn render_frame(params: &StabilizationParams, timestamp_us: i64) -> usize {
    let mut ts =
        timestamp_us + (params.frame_offset as f64 / params.fps * 1_000_000.0).round() as i64;
    if let Some(scale) = params.fps_scale {
        ts = (ts as f64 / scale).round() as i64;
    }
    gyroflow_core::frame_at_timestamp(ts as f64 / 1000.0, params.get_scaled_fps()).max(0) as usize
}

/// Frames of the timeline item being rendered, from the host's time, its source frame and the item's
/// range; the whole clip when the host doesn't tell
pub fn visible_frames(
    params: &StabilizationParams,
    time: f64,
    frame_range: Option<(f64, f64)>,
    src_frame: Option<i64>,
    host_fps: f64,
    is_fusion_page: bool,
) -> (usize, usize) {
    let last = params.frame_count.saturating_sub(1);
    let (Some(src), Some((min, max))) = (src_frame, frame_range) else {
        return (0, last);
    };
    if is_fusion_page
        || !time.is_finite()
        || !(max >= min)
        || !(params.fps > 0.0)
        || !(host_fps > 0.0)
    {
        return (0, last);
    }
    // A source frame is also the core's frame index: `fps_scale` divides the timestamp and multiplies the fps
    let per_host_frame = params.fps / host_fps;
    let clamp = |frame: f64| frame.clamp(0.0, last as f64) as usize;
    let start = clamp((src as f64 + (min - time) * per_host_frame).floor() - 2.0);
    let end = clamp((src as f64 + (max - time) * per_host_frame).ceil() + 2.0);
    (start, end.max(start))
}

/// Frames of a timeline item that plays source frames `start..end` (from Resolve's scripting API),
/// padded by a frame for Resolve's instances disagreeing by one
pub fn item_frames(params: &StabilizationParams, range: (f64, f64)) -> Option<(usize, usize)> {
    let last = params.frame_count.checked_sub(1)?;
    let (start, end) = range;
    if !start.is_finite() || !end.is_finite() || end <= start || start > last as f64 {
        return None;
    }
    // A source frame is the core's frame index, as in `visible_frames`
    let clamp = |frame: f64| frame.clamp(0.0, last as f64) as usize;
    Some((clamp(start.floor() - 1.0), clamp(end.ceil())))
}

/// The solved crop of one plugin instance
#[derive(Default)]
pub struct CropState {
    /// Held weakly, so no other manager can take its address while it's compared against
    solved: Option<(Weak<StabilizationManager>, CropRequest)>,
    estimate: Option<(usize, usize)>,
}

impl CropState {
    /// Forgets the solve, so the next `ensure` solves again
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Solves the crop again if the stabilization, the request or the manager changed, or `frame` isn't covered yet
    pub fn ensure(
        &mut self,
        stab: &Arc<StabilizationManager>,
        mut request: CropRequest,
        frame: usize,
    ) -> Option<CropReport> {
        if stab.smoothing_invalidated.load(SeqCst) {
            stab.recompute_smoothness();
            stab.smoothing_invalidated.store(false, SeqCst);
        }
        let solved = self
            .solved
            .as_ref()
            .filter(|(manager, _)| std::ptr::eq(manager.as_ptr(), Arc::as_ptr(stab)))
            .map(|(_, solved)| solved.clone());
        let estimate = request.frames;
        let mut frames = (estimate.0.min(frame), estimate.1.max(frame));
        if let Some(solved) = &solved {
            // A frame outside the item's estimated range widens the solved range until the item changes
            if self.estimate == Some(estimate) {
                frames = (frames.0.min(solved.frames.0), frames.1.max(solved.frames.1));
            }
        }
        request.frames = frames;
        if !stab.zooming_invalidated.load(SeqCst) && solved.as_ref() == Some(&request) {
            return None;
        }
        let report = solve(stab, &request)?;
        self.solved = Some((Arc::downgrade(stab), request));
        self.estimate = Some(estimate);
        Some(report)
    }
}

/// Solves the crop and hands it to the renderer: the per-frame fovs, and the bounds as the minimal fovs
pub fn solve(stab: &StabilizationManager, request: &CropRequest) -> Option<CropReport> {
    let started = std::time::Instant::now();
    let mut cp = ComputeParams::from_manager(stab);
    if cp.frame_count == 0 || !(cp.scaled_fps > 0.0) || cp.width == 0 || cp.height == 0 {
        return None;
    }
    let first = request.frames.0.min(cp.frame_count - 1);
    let last = request.frames.1.clamp(first, cp.frame_count - 1);
    let frames: Vec<usize> = (first..=last).collect();
    // What's checked is the normal view; the overview is a diagnostic that shows past the edge on purpose
    cp.fov_overview = false;

    let mut bounds: Vec<f64> = outline_bounds(&cp, &frames, request.margin)
        .into_iter()
        .map(|b| b.max(MIN_FOV))
        .collect();
    let step = std::f64::consts::LN_2 / request.response_s.max(0.01) / cp.scaled_fps;
    let half = (request.response_s / 16.0 * cp.scaled_fps).round() as usize;
    // A smaller rectangle inside one that fits fits too, so the extra zoom only divides the curve under the bounds
    let zoom: Vec<f64> = frames.iter().map(|f| request.zoom_at(*f)).collect();
    let curve = |bounds: &[f64]| {
        let fit = if request.static_crop { static_curve(bounds) } else { zoom_curve(bounds, step, half) };
        fit.into_iter().zip(&zoom).map(|(f, z)| (f / z).max(MIN_FOV)).collect::<Vec<f64>>()
    };
    let mut fovs = curve(&bounds);

    let samples = output_samples((cp.output_width as f64, cp.output_height as f64));
    let frame_size = rotated_size((cp.width as f64, cp.height as f64), request.input_rotation);
    let mut flags = KernelParamsFlags::empty();
    flags.set(
        KernelParamsFlags::HAS_DIGITAL_LENS,
        cp.digital_lens.is_some(),
    );
    flags.set(
        KernelParamsFlags::HORIZONTAL_RS,
        cp.frame_readout_direction.is_horizontal(),
    );
    flags.set(
        KernelParamsFlags::FRAMEBUFFER_INVERTED,
        cp.framebuffer_inverted,
    );
    let flags = flags.bits();

    let mut slack = vec![f64::NEG_INFINITY; frames.len()];
    let mut pending: Vec<usize> = (0..frames.len()).collect();
    let (mut rounds, mut corrected, mut overshoot) = (0, 0, 0.0f64);
    while !pending.is_empty() {
        if rounds == MAX_ROUNDS {
            log::error!(
                "Crop check still fails at {} frame(s) after {rounds} rounds",
                pending.len()
            );
            break;
        }
        rounds += 1;
        cp.fovs = extend(&fovs, first, cp.frame_count);
        let checked: Vec<(usize, f64, Option<f64>)> = pending
            .par_iter()
            .map(|&k| {
                let check = FrameCheck::new(&cp, frames[k], request.input_rotation, flags, fovs[k]);
                let s = check.slack(&cp, fovs[k], &samples, request.margin, frame_size);
                let fix = (s < 0.0)
                    .then(|| check.largest_fov(&cp, fovs[k], &samples, request.margin, frame_size));
                (k, s, fix)
            })
            .collect();
        let mut lowered = false;
        for (k, s, fix) in checked {
            slack[k] = s;
            if let Some(fov) = fix {
                log::debug!(
                    "Crop: frame {} fails the render check by {:.2} px, bound {:.5} -> {fov:.5}",
                    frames[k],
                    -s,
                    bounds[k]
                );
                bounds[k] = bounds[k].min(fov * zoom[k]).max(MIN_FOV);
                corrected += 1;
                overshoot = overshoot.max(-s);
                lowered = true;
            }
        }
        if !lowered {
            break;
        }
        // The curve only goes down, and a rectangle that passed holds when smaller: only what moved is checked again
        let next = curve(&bounds);
        pending = (0..frames.len()).filter(|&k| next[k] != fovs[k]).collect();
        fovs = next;
    }

    // The bounds as the extra zoom leaves them, so every fov stays under its bound
    let bounds: Vec<f64> = bounds.iter().zip(&zoom).map(|(b, z)| (b / z).max(MIN_FOV)).collect();
    let lens_fov_adjustment = stab.lens.read().optimal_fov.unwrap_or(1.0);
    {
        let mut params = stab.params.write();
        params.set_fovs(extend(&fovs, first, cp.frame_count), lens_fov_adjustment);
        params.minimal_fovs = extend(&bounds, first, cp.frame_count);
    }
    stab.recompute_undistortion();
    stab.zooming_invalidated.store(false, SeqCst);
    stab.undistortion_invalidated.store(false, SeqCst);

    let closest = slack.iter().copied().fold(f64::INFINITY, f64::min);
    let (widest, tightest) = fovs.iter().fold((0.0f64, f64::INFINITY), |(hi, lo), f| {
        (hi.max(*f), lo.min(*f))
    });
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    log::info!(
        "Crop solved for frames {first}..={last}: zoom {:.1}%..{:.1}%, closest sample {closest:.2} px inside a {:.1}x{:.1} px edge margin, {corrected} bound(s) lowered by the render check (up to {overshoot:.2} px), {rounds} round(s), {elapsed_ms:.0} ms",
        100.0 / widest,
        100.0 / tightest,
        request.margin.0,
        request.margin.1
    );

    Some(CropReport {
        frames: (first, last),
        bounds,
        fovs,
        slack,
        corrected,
        overshoot,
        elapsed_ms,
    })
}

/// Per frame, the largest fov whose rectangle the stabilized source outline still contains (see `zooming::FovIterative`)
fn outline_bounds(cp: &ComputeParams, frames: &[usize], margin: (f64, f64)) -> Vec<f64> {
    let mut fwd = cp.clone();
    fwd.fov_scale = 1.0;
    fwd.fovs.clear();
    fwd.minimal_fovs.clear();
    fwd.output_width = fwd.width;
    fwd.output_height = fwd.height;
    let (w, h) = (cp.width as f64, cp.height as f64);
    let half = (
        w / 2.0,
        w * cp.output_height as f64 / cp.output_width.max(1) as f64 / 2.0,
    );
    // A hair inside the checked margin, so float differences between the two maps rarely need a bisection
    let outline = source_outline((w, h), (margin.0 + TOLERANCE_PX, margin.1 + TOLERANCE_PX));
    let first_ts = frames
        .first()
        .map_or(0.0, |f| *f as f64 * 1000.0 / cp.scaled_fps);
    // Where the picture stops being worth showing (very wide lenses); the outline alone has no say past it
    let limit =
        zooming::max_zoom_out_fov(&fwd, cp.lens_correction_amount, cp.output_width, first_ts);
    frames
        .par_iter()
        .map(|&frame| {
            let ts = frame as f64 * 1000.0 / cp.scaled_fps;
            let keyframes = &fwd.keyframes;
            let cx = keyframes
                .value_at_video_timestamp(&KeyframeType::ZoomingCenterX, ts)
                .unwrap_or(fwd.adaptive_zoom_center_offset.0);
            let cy = keyframes
                .value_at_video_timestamp(&KeyframeType::ZoomingCenterY, ts)
                .unwrap_or(fwd.adaptive_zoom_center_offset.1);
            let amount = keyframes
                .value_at_video_timestamp(&KeyframeType::LensCorrectionStrength, ts)
                .unwrap_or(fwd.lens_correction_amount);
            let centre = (w * (0.5 + cx), h * (0.5 + cy));
            let mapped = undistort_points_with_rolling_shutter(
                &outline,
                ts,
                Some(frame),
                &fwd,
                amount,
                false,
                true,
            );
            let relative: Vec<(f64, f64)> = mapped
                .into_iter()
                .filter(|p| is_valid_point(*p))
                .map(|p| (p.0 as f64 - centre.0, p.1 as f64 - centre.1))
                .collect();
            let scale = inscribed_scale(&relative, half);
            if limit > 0.0 { scale.min(limit) } else { scale }
        })
        .collect()
}

/// The source rectangle inset by `margin`, as an ordered outline
fn source_outline(size: (f64, f64), margin: (f64, f64)) -> Vec<(f32, f32)> {
    let (x0, y0, x1, y1) = (margin.0, margin.1, size.0 - margin.0, size.1 - margin.1);
    let at = |from: f64, to: f64, i: usize| from + (to - from) * i as f64 / OUTLINE_POINTS as f64;
    let mut outline = Vec::with_capacity(4 * OUTLINE_POINTS);
    for i in 0..OUTLINE_POINTS {
        outline.push((at(x0, x1, i), y0));
    }
    for i in 0..OUTLINE_POINTS {
        outline.push((x1, at(y0, y1, i)));
    }
    for i in 0..OUTLINE_POINTS {
        outline.push((at(x1, x0, i), y1));
    }
    for i in 0..OUTLINE_POINTS {
        outline.push((x0, at(y1, y0, i)));
    }
    outline
        .into_iter()
        .map(|(x, y)| (x as f32, y as f32))
        .collect()
}

/// Largest scale of the centred rectangle with half size `half` inside a closed outline given relative
/// to the centre; 0 when the centre is outside it
fn inscribed_scale(outline: &[(f64, f64)], half: (f64, f64)) -> f64 {
    if outline.len() < 3 {
        return 0.0;
    }
    let mut inside = false;
    let mut scale = f64::INFINITY;
    for (i, &p) in outline.iter().enumerate() {
        let q = outline[(i + 1) % outline.len()];
        if (p.1 > 0.0) != (q.1 > 0.0) && p.0 - p.1 * (q.0 - p.0) / (q.1 - p.1) > 0.0 {
            inside = !inside;
        }
        scale = scale.min(segment_gauge(p, q, half));
    }
    if inside { scale } else { 0.0 }
}

/// Smallest `max(|x|/a, |y|/b)` on the segment p-q, ie. the scale at which the centred rectangle first touches it
fn segment_gauge(p: (f64, f64), q: (f64, f64), half: (f64, f64)) -> f64 {
    let (px, py) = (p.0 / half.0, p.1 / half.1);
    let (dx, dy) = ((q.0 - p.0) / half.0, (q.1 - p.1) / half.1);
    let gauge = |t: f64| (px + t * dx).abs().max((py + t * dy).abs());
    let mut best = gauge(0.0).min(gauge(1.0));
    // The gauge is convex and piecewise linear along the segment, so its minimum is at an end or a kink
    for (num, den) in [
        (-px, dx),
        (-py, dy),
        (py - px, dx - dy),
        (-(px + py), dx + dy),
    ] {
        if den.abs() > 1e-12 {
            let t = num / den;
            if t > 0.0 && t < 1.0 {
                best = best.min(gauge(t));
            }
        }
    }
    best
}

/// The largest zoom curve under `bounds` whose log changes by at most `step` per frame, with its corners eased
fn zoom_curve(bounds: &[f64], step: f64, half: usize) -> Vec<f64> {
    ease(&rate_limited(bounds, step), half)
        .into_iter()
        .map(f64::exp)
        .collect()
}

/// The tightest bound held for every frame
fn static_curve(bounds: &[f64]) -> Vec<f64> {
    let tightest = bounds.iter().copied().fold(f64::INFINITY, f64::min);
    vec![tightest; bounds.len()]
}

/// `min_j (ln bounds[j] + step·|i - j|)`, in two passes
fn rate_limited(bounds: &[f64], step: f64) -> Vec<f64> {
    let mut v: Vec<f64> = bounds.iter().map(|b| b.ln()).collect();
    for i in 1..v.len() {
        v[i] = v[i].min(v[i - 1] + step);
    }
    for i in (1..v.len()).rev() {
        v[i - 1] = v[i - 1].min(v[i] + step);
    }
    v
}

/// Moving minimum, then a moving average over the same window: every averaged value is a minimum over a
/// window containing the frame itself, so the result never rises above the input
fn ease(values: &[f64], half: usize) -> Vec<f64> {
    let n = values.len();
    if half == 0 || n < 2 {
        return values.to_vec();
    }
    let eroded: Vec<f64> = (0..n)
        .map(|i| {
            values[i.saturating_sub(half)..=(i + half).min(n - 1)]
                .iter()
                .copied()
                .fold(f64::INFINITY, f64::min)
        })
        .collect();
    let width = 2 * half + 1;
    (0..n)
        .map(|i| {
            (0..width)
                .map(|k| eroded[(i + k).saturating_sub(half).min(n - 1)])
                .sum::<f64>()
                / width as f64
        })
        .collect()
}

/// Per-frame values over `first..` extended to all `count` frames with their nearest end
fn extend(values: &[f64], first: usize, count: usize) -> Vec<f64> {
    (0..count)
        .map(|i| values[i.saturating_sub(first).min(values.len() - 1)])
        .collect()
}

fn rotated_size(size: (f64, f64), rotation_deg: f32) -> (f64, f64) {
    if rotation_deg == 0.0 {
        return size;
    }
    let (sin, cos) = (rotation_deg as f64).to_radians().sin_cos();
    (
        (size.0 * cos - size.1 * sin).abs().round(),
        (size.0 * sin + size.1 * cos).abs().round(),
    )
}

/// Output-plane points that are checked: the frame's edge, densely, and a grid inside it
fn output_samples(size: (f64, f64)) -> Vec<(f32, f32)> {
    let nx = (size.0 / CHECK_SPACING).ceil().max(1.0) as usize;
    let ny = (size.1 / CHECK_SPACING).ceil().max(1.0) as usize;
    let mut samples = Vec::with_capacity(2 * (nx + ny) + CHECK_GRID * CHECK_GRID);
    for i in 0..=nx {
        let x = size.0 * i as f64 / nx as f64;
        samples.push((x, 0.0));
        samples.push((x, size.1));
    }
    for j in 1..ny {
        let y = size.1 * j as f64 / ny as f64;
        samples.push((0.0, y));
        samples.push((size.0, y));
    }
    for i in 0..CHECK_GRID {
        for j in 0..CHECK_GRID {
            samples.push((
                size.0 * (i as f64 + 0.5) / CHECK_GRID as f64,
                size.1 * (j as f64 + 0.5) / CHECK_GRID as f64,
            ));
        }
    }
    samples
        .into_iter()
        .map(|(x, y)| (x as f32, y as f32))
        .collect()
}

/// One frame of the renderer's pixel map, built the way `Stabilization::get_frame_transform_at` builds it
struct FrameCheck {
    transform: FrameTransform,
    mesh: Vec<f64>,
    /// The fov the transform was built with
    fov: f64,
}

impl FrameCheck {
    fn new(cp: &ComputeParams, frame: usize, input_rotation: f32, flags: i32, fov: f64) -> Self {
        let mut transform =
            FrameTransform::at_timestamp(cp, frame as f64 * 1000.0 / cp.scaled_fps, frame);
        let size = rotated_size((cp.width as f64, cp.height as f64), input_rotation);
        let kp = &mut transform.kernel_params;
        kp.width = cp.width as i32;
        kp.height = cp.height as i32;
        kp.output_width = cp.output_width as i32;
        kp.output_height = cp.output_height as i32;
        kp.flags = flags;
        kp.input_rotation = input_rotation;
        // Identity rects and no edge fill: the sample position itself, in source pixels
        kp.background_mode = 0;
        kp.source_rect = [0, 0, size.0 as i32, size.1 as i32];
        kp.output_rect = [0, 0, kp.output_width, kp.output_height];
        let mesh = transform.mesh_data.iter().map(|v| *v as f64).collect();
        Self {
            transform,
            mesh,
            fov,
        }
    }

    /// Distance of the closest sample to the edge of the usable source area at `fov`, negative when one is past it
    fn slack(
        &self,
        cp: &ComputeParams,
        fov: f64,
        samples: &[(f32, f32)],
        margin: (f64, f64),
        frame_size: (f64, f64),
    ) -> f64 {
        // The fov scales the output plane, and the reframe offset is a translation divided by it (`FrameTransform::at_timestamp`)
        let scale = (fov / self.fov) as f32;
        let mut kp = self.transform.kernel_params;
        kp.fov *= scale;
        kp.translation2d = [kp.translation2d[0] / scale, kp.translation2d[1] / scale];
        let stretch = if kp.input_horizontal_stretch > 0.01 {
            1.0 / kp.input_horizontal_stretch
        } else {
            1.0
        };
        let out_c = Vector2::new(kp.output_width as f32 / 2.0, kp.output_height as f32 / 2.0);
        let out_f = Vector2::new(kp.f[0] * stretch / kp.fov, kp.f[1] * stretch / kp.fov);
        samples
            .iter()
            .map(|&(x, y)| {
                match Stabilization::undistort_coord(
                    Vector2::new(x, y),
                    &kp,
                    &self.transform.matrices,
                    &cp.distortion_model,
                    cp.digital_lens.as_ref(),
                    &self.mesh,
                    &out_c,
                    &out_f,
                ) {
                    Some(uv) if uv.x.is_finite() && uv.y.is_finite() => {
                        let (u, v) = (uv.x as f64, uv.y as f64);
                        (u - margin.0)
                            .min(frame_size.0 - margin.0 - u)
                            .min(v - margin.1)
                            .min(frame_size.1 - margin.1 - v)
                    }
                    _ => f64::NEG_INFINITY,
                }
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// The largest fov up to `fov` whose samples all pass, by bisection
    fn largest_fov(
        &self,
        cp: &ComputeParams,
        fov: f64,
        samples: &[(f32, f32)],
        margin: (f64, f64),
        frame_size: (f64, f64),
    ) -> f64 {
        let passes = |f: f64| self.slack(cp, f, samples, margin, frame_size) >= 0.0;
        let mut lo = fov * 0.5;
        while lo > MIN_FOV && !passes(lo) {
            lo *= 0.5;
        }
        if lo <= MIN_FOV {
            return MIN_FOV;
        }
        let mut hi = fov;
        for _ in 0..20 {
            let mid = 0.5 * (lo + hi);
            if passes(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic values in 0..1
    fn noise(n: usize, mut seed: u64) -> Vec<f64> {
        (0..n)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 11) as f64 / (1u64 << 53) as f64
            })
            .collect()
    }

    #[test]
    fn segment_gauge_finds_the_closest_point_between_far_ends() {
        let half = (2.0, 1.0);
        let (p, q) = ((4.0, 0.2), (0.2, 2.2));
        let exact = segment_gauge(p, q, half);
        let sampled = (0..=100_000)
            .map(|i| {
                let t = i as f64 / 100_000.0;
                ((p.0 + t * (q.0 - p.0)).abs() / half.0).max((p.1 + t * (q.1 - p.1)).abs() / half.1)
            })
            .fold(f64::INFINITY, f64::min);
        assert!(exact < 2.0 && exact < 2.2, "lower than both ends: {exact}");
        assert!(
            (exact - sampled).abs() < 1e-4,
            "{exact} vs sampled {sampled}"
        );
        assert!((exact - (0.2 + 2.0 * 1.8 / 3.9)).abs() < 1e-12);
    }

    #[test]
    fn inscribed_scale_fits_the_outline_and_needs_the_centre_inside() {
        let outline = [(-3.0, -2.0), (3.0, -2.0), (3.0, 2.0), (-3.0, 2.0)];
        assert!((inscribed_scale(&outline, (2.0, 1.0)) - 1.5).abs() < 1e-12);
        assert!((inscribed_scale(&outline, (1.0, 1.0)) - 2.0).abs() < 1e-12);
        let shifted: Vec<(f64, f64)> = outline.iter().map(|(x, y)| (x + 4.0, *y)).collect();
        assert_eq!(inscribed_scale(&shifted, (2.0, 1.0)), 0.0);
    }

    #[test]
    fn rate_limited_curve_is_the_largest_under_its_bounds() {
        let bounds: Vec<f64> = noise(400, 7).into_iter().map(|v| 0.3 + 0.7 * v).collect();
        let step = 0.01;
        let curve = rate_limited(&bounds, step);
        for i in 0..bounds.len() {
            let best = (0..bounds.len())
                .map(|j| bounds[j].ln() + step * (i as f64 - j as f64).abs())
                .fold(f64::INFINITY, f64::min);
            assert!((curve[i] - best).abs() < 1e-12);
            assert!(curve[i] <= bounds[i].ln());
            if i > 0 {
                assert!((curve[i] - curve[i - 1]).abs() <= step + 1e-12);
            }
        }
    }

    #[test]
    fn easing_never_rises_above_its_input_and_keeps_the_rate() {
        let step = 0.02;
        let curve = rate_limited(
            &noise(300, 11)
                .into_iter()
                .map(|v| 0.2 + v)
                .collect::<Vec<_>>(),
            step,
        );
        for half in [0, 1, 4, 20, 400] {
            let eased = ease(&curve, half);
            assert!(
                eased.iter().zip(&curve).all(|(e, c)| *e <= *c + 1e-12),
                "half {half}"
            );
            assert!(
                eased
                    .windows(2)
                    .all(|w| (w[1] - w[0]).abs() <= step + 1e-12),
                "half {half}"
            );
        }
    }

    #[test]
    fn static_curve_holds_the_tightest_bound() {
        let bounds: Vec<f64> = noise(200, 3).into_iter().map(|v| 0.3 + 0.7 * v).collect();
        let tightest = bounds.iter().copied().fold(f64::INFINITY, f64::min);
        let curve = static_curve(&bounds);
        assert!(curve.iter().all(|f| *f == tightest));
        assert!(curve.iter().zip(&bounds).all(|(f, b)| f <= b));
    }

    #[test]
    fn visible_frames_follow_the_items_source_range() {
        let mut params = StabilizationParams::default();
        params.fps = 30.0;
        params.frame_count = 1000;
        // 100 host frames from source frame 400, rendered at the item's 10th frame
        assert_eq!(
            visible_frames(&params, 10.0, Some((0.0, 100.0)), Some(410), 30.0, false),
            (398, 502)
        );
        // 60 fps on a 30 fps timeline: two source frames per host frame
        params.fps = 60.0;
        assert_eq!(
            visible_frames(&params, 10.0, Some((0.0, 100.0)), Some(420), 30.0, false),
            (398, 602)
        );
        // The whole clip without a source frame, and on the Fusion page
        assert_eq!(
            visible_frames(&params, 10.0, Some((0.0, 100.0)), None, 30.0, false),
            (0, 999)
        );
        assert_eq!(
            visible_frames(&params, 10.0, Some((0.0, 100.0)), Some(420), 30.0, true),
            (0, 999)
        );
    }

    #[test]
    fn zoom_holds_one_value_or_follows_the_frames() {
        let mut request = CropRequest::default();
        assert_eq!((request.zoom_at(0), request.zoom_at(500)), (1.0, 1.0));
        request.zoom = vec![1.5];
        assert_eq!((request.zoom_at(0), request.zoom_at(500)), (1.5, 1.5));
        request.zoom = vec![1.0, 2.0, 0.5, f64::NAN];
        assert_eq!((request.zoom_at(1), request.zoom_at(2), request.zoom_at(3), request.zoom_at(9)), (2.0, 1.0, 1.0, 1.0));
        request.zoom.clear();
        assert_eq!(request.zoom_at(3), 1.0);
    }

    #[test]
    fn item_frames_pad_and_clamp_the_items_source_range() {
        let mut params = StabilizationParams::default();
        params.fps = 30.0;
        params.frame_count = 881;
        assert_eq!(item_frames(&params, (120.0, 600.0)), Some((119, 600)));
        // Ends past the clip, and a cut at its start
        assert_eq!(item_frames(&params, (0.0, 2000.0)), Some((0, 880)));
        assert_eq!(item_frames(&params, (900.0, 1000.0)), None);
        assert_eq!(item_frames(&params, (50.0, 50.0)), None);
        assert_eq!(item_frames(&params, (f64::NAN, 50.0)), None);
        params.frame_count = 0;
        assert_eq!(item_frames(&params, (0.0, 10.0)), None);
    }

    #[test]
    fn margin_covers_the_kernel_at_the_host_scale() {
        // Resolve's 1080p frame holds the 4:3 source pillarboxed in 1440x1080
        let (x, y) = source_margin(Interpolation::Lanczos4, (4000, 3000), (240, 0, 1440, 1080));
        assert!((x - (4.0 + 1.0 / 32.0) * 4000.0 / 1440.0 - TOLERANCE_PX).abs() < 1e-12);
        assert!((y - (4.0 + 1.0 / 32.0) * 3000.0 / 1080.0 - TOLERANCE_PX).abs() < 1e-12);
        let (bx, _) = source_margin(Interpolation::Bilinear, (4000, 3000), (0, 0, 4000, 3000));
        assert!((bx - 1.0 - 1.0 / 32.0 - TOLERANCE_PX).abs() < 1e-12);
    }

    #[test]
    fn output_samples_cover_the_frame_edge() {
        let samples = output_samples((4000.0, 2250.0));
        for corner in [(0.0, 0.0), (4000.0, 0.0), (0.0, 2250.0), (4000.0, 2250.0)] {
            assert!(samples.contains(&corner));
        }
        let mut top: Vec<f32> = samples.iter().filter(|p| p.1 == 0.0).map(|p| p.0).collect();
        top.sort_by(f32::total_cmp);
        assert!(
            top.windows(2)
                .all(|w| w[1] - w[0] <= CHECK_SPACING as f32 + 1e-3)
        );
    }
}
