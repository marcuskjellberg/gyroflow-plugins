use std::sync::Arc;
use ofx::*;
use super::settings;
use gyroflow_plugin_base::*;
use gyroflow_plugin_base::parking_lot::{ Mutex, RwLock };
use gyroflow_plugin_base::lru::LruCache;

plugin_module!(
    "xyz.gyroflow.horizon",
    ApiVersion(1),
    PluginVersion(1, 0),
    GyroflowPlugin::default
);

#[derive(Default)]
struct GyroflowPlugin {
    gyroflow_plugin: GyroflowPluginBase,
}

pub fn frame_from_timetype(time: TimeType) -> Result<f64> {
    match time {
        TimeType::Frame(x) => Ok(x),
        TimeType::FrameOrMicrosecond((Some(x), _)) => Ok(x),
        _ => Err(Error::UnknownError),
    }
}

fn image_layout(bounds: RectI, row_bytes: Int, depth: BitDepth) -> Option<(usize, usize, usize)> {
    let width = usize::try_from(bounds.x2.checked_sub(bounds.x1)?).ok()?;
    let height = usize::try_from(bounds.y2.checked_sub(bounds.y1)?).ok()?;
    if width == 0 || height == 0 || row_bytes < 0 {
        return None;
    }

    let bytes_per_pixel = match depth {
        BitDepth::Byte => 4,
        BitDepth::Short | BitDepth::Half => 8,
        BitDepth::Float => 16,
        BitDepth::None => return None,
    };
    let minimum_stride = width.checked_mul(bytes_per_pixel)?;
    let stride = if row_bytes == 0 {
        minimum_stride
    } else {
        usize::try_from(row_bytes).ok()?
    };
    if stride < minimum_stride || stride.checked_mul(height).is_none() {
        return None;
    }

    Some((width, height, stride))
}

fn copy_source_frame(source: &[u8], output: &mut [u8], source_size: (usize, usize, usize), output_size: (usize, usize, usize), depth: BitDepth) -> bool {
    if source_size.0 != output_size.0 || source_size.1 != output_size.1 { return false; }
    let bytes_per_pixel = match depth {
        BitDepth::Byte => 4,
        BitDepth::Short | BitDepth::Half => 8,
        BitDepth::Float => 16,
        BitDepth::None => return false,
    };
    let Some(row_length) = source_size.0.checked_mul(bytes_per_pixel) else { return false };
    let Some(source_length) = source_size.2.checked_mul(source_size.1) else { return false };
    let Some(output_length) = output_size.2.checked_mul(output_size.1) else { return false };
    if source_size.2 < row_length || output_size.2 < row_length || source.len() < source_length || output.len() < output_length { return false; }
    for row in 0..source_size.1 {
        let src = row * source_size.2;
        let dst = row * output_size.2;
        output[dst..dst + row_length].copy_from_slice(&source[src..src + row_length]);
    }
    true
}

fn unsupported_reason(stab: &StabilizationManager) -> Option<&'static str> {
    let gyro = stab.gyro.read();
    if !gyro.has_motion() { return Some("No embedded gyro data"); }
    if !gyro.file_metadata.read().has_accurate_timestamps && gyro.get_offsets().is_empty() {
        return Some("Embedded gyro is not synchronized");
    }
    None
}

/// Parameters shown in the panel. The shared ones keep their IDs but are hidden.
const VISIBLE: &[&str] = &[
    "ProjectGroup", "Status",
    "AdjustGroup", "Leveling", "StaticCrop", "CropSmoothness", "CropZoom", "OffsetX", "OffsetY",
    "ViewMode",
];

/// Controls handled by this plugin (see `settings.rs`)
fn horizon_param_definitions() -> Vec<ParameterType> {
    vec![
        ParameterType::Slider   { id: "Leveling",       label: "Leveling amount",     hint: "How much of the camera's roll to take out: 100 = horizon locked, 0 = left as filmed", min: 0.0, max: 100.0, default: settings::DEFAULT_LEVELING },
        ParameterType::Checkbox { id: "StaticCrop",     label: "Static crop",         hint: "One crop for the whole clip, sized for its most tilted frame. Off: the crop follows the tilt", default: false },
        ParameterType::Slider   { id: "CropSmoothness", label: "Crop smoothness (s)", hint: "Seconds the dynamic crop takes to zoom by 2x: longer is gentler but crops in earlier before a tilt", min: settings::MIN_CROP_SMOOTHNESS_S, max: settings::MAX_CROP_SMOOTHNESS_S, default: settings::DEFAULT_CROP_SMOOTHNESS_S },
        ParameterType::Slider   { id: "CropZoom",       label: "Zoom (%)",            hint: "Zoom in further than the largest crop that fits", min: settings::DEFAULT_ZOOM_PERCENT, max: settings::MAX_ZOOM_PERCENT, default: settings::DEFAULT_ZOOM_PERCENT },
        ParameterType::Slider   { id: "OffsetX",        label: "Offset X",            hint: "Move the frame left or right, in percent of half the source width. The crop zooms in as needed", min: -settings::MAX_OFFSET_PERCENT, max: settings::MAX_OFFSET_PERCENT, default: 0.0 },
        ParameterType::Slider   { id: "OffsetY",        label: "Offset Y",            hint: "Move the frame up or down, in percent of half the source height. The crop zooms in as needed", min: -settings::MAX_OFFSET_PERCENT, max: settings::MAX_OFFSET_PERCENT, default: 0.0 },
        ParameterType::Select   { id: "ViewMode",       label: "View",                hint: "Overview shows the source and the crop's boundary", options: vec!["Normal", "Overview"], default: "Normal" },
    ]
}

fn is_hidden_in_panel(id: &str) -> bool {
    !VISIBLE.contains(&id)
}

define_params!(ParamHandler {
    strings: [
        Status              => status:           ParamHandle<String>,
        InstanceId          => instance_id:      ParamHandle<String>,
        ProjectData         => project_data:     ParamHandle<String>,
        EmbeddedLensProfile => embedded_lens:    ParamHandle<String>,
        EmbeddedPreset      => embedded_preset:  ParamHandle<String>,
        ProjectPath         => project_path:     ParamHandle<String>,
        OpenGyroflow        => open_in_gyroflow: ParamHandle<String>,
        ReloadProject       => reload_project:   ParamHandle<String>,
        OutputSizeSwap      => output_swap:      ParamHandle<String>,
        OutputSizeToTimeline=> output_size_fit:  ParamHandle<String>,
        LoadedProject       => loaded_project:   ParamHandle<String>,
        LoadedPreset        => loaded_preset:    ParamHandle<String>,
        LoadedLens          => loaded_lens:      ParamHandle<String>,
    ],
    bools: [
        DisableStretch        => disable_stretch:         ParamHandle<bool>,
        ToggleOverview        => toggle_overview:         ParamHandle<bool>,
        DontDrawOutside       => dont_draw_outside:       ParamHandle<bool>,
        IncludeProjectData    => include_project_data:    ParamHandle<bool>,
        UseGyroflowsKeyframes => use_gyroflows_keyframes: ParamHandle<bool>,
    ],
    f64s: [
        InputRotation         => input_rotation:           ParamHandle<Double>,
        Fov                   => fov:                      ParamHandle<Double>,
        Smoothness            => smoothness:               ParamHandle<Double>,
        ZoomLimit             => zoom_limit:               ParamHandle<Double>,
        LensCorrectionStrength=> lens_correction_strength: ParamHandle<Double>,
        HorizonLockAmount     => horizon_lock_amount:      ParamHandle<Double>,
        HorizonLockRoll       => horizon_lock_roll:        ParamHandle<Double>,
        // PositionX             => positionx:                ParamHandle<Double>,
        // PositionY             => positiony:                ParamHandle<Double>,
        AdditionalYaw         => additional_yaw:           ParamHandle<Double>,
        AdditionalPitch       => additional_pitch:         ParamHandle<Double>,
        Rotation              => rotation:                 ParamHandle<Double>,
        VideoSpeed            => video_speed:              ParamHandle<Double>,
        OutputWidth           => output_width:             ParamHandle<Double>,
        OutputHeight          => output_height:            ParamHandle<Double>,
        //FusionStartFrame      => fusion_start_frame:       ParamHandle<Double>,
    ],
    i32s: [
        Interpolation         => interpolation:            ParamHandle<Int>,
        IntegrationMethod     => integration_method:       ParamHandle<Int>,
    ],

    get_string:  _s p    { Ok(p.get_value()?) },
    set_string:  _s p, v { Ok(p.set_value(v.into())?) },
    get_bool:    _s p    { Ok(p.get_value() ?) },
    set_bool:    _s p, v { Ok(p.set_value(v)?) },
    get_f64:     _s p    { Ok(p.get_value() ?) },
    set_f64:     _s p, v { Ok(p.set_value(v)?) },
    get_i32:     _s p    { Ok(p.get_value() ?) },
    set_i32:     _s p, v { Ok(p.set_value(v)?) },
    set_label:   _s p, l { Ok(p.set_label(l)?) },
    set_hint:    _s p, h { Ok(p.set_hint(h) ?) },
    set_enabled: _s p, e { Ok(p.set_enabled(e)?) },
    get_bool_at_time: _s p, t    { Ok(p.get_value_at_time(frame_from_timetype(t)?)?) },
    get_f64_at_time:  _s p, t    { Ok(p.get_value_at_time(frame_from_timetype(t)?)?) },
    set_f64_at_time:  _s p, t, v { Ok(p.set_value_at_time(frame_from_timetype(t)?, v)?) },
    is_keyframed: _s p { p.get_num_keys().unwrap_or_default() > 0 },
    get_keyframes: _s p {
        let num_keys = p.get_num_keys().unwrap_or_default();
        let mut ret = Vec::with_capacity(num_keys as usize);
        for i in 0..num_keys {
            if let Ok(time) = p.get_key_time(i) {
                if let Ok(val) = p.get_value_at_time(time) {
                    ret.push((TimeType::Frame(time), val));
                }
            }
        }
        ret
    },
    clear_keyframes: _s p { Ok(p.delete_all_keys()?) },
});

struct InstanceData {
    source_clip: ClipInstance,
    output_clip: ClipInstance,

    params: ParamHandler,
    plugin: GyroflowPluginBaseInstance,
    supports_output_size: bool,
    is_fusion_page: bool,
    file_path: Option<String>,
    unsupported_reason: Option<String>,

    leveling: ParamHandle<Double>,
    static_crop: ParamHandle<bool>,
    crop_smoothness: ParamHandle<Double>,
    zoom: ParamHandle<Double>,
    offset_x: ParamHandle<Double>,
    offset_y: ParamHandle<Double>,
    view_mode: ParamHandle<Int>,
    crop: gyroflow_plugin_base::crop::CropState,
}

impl InstanceData {
    fn stab_manager(&mut self, manager_cache: &Mutex<LruCache<String, Arc<StabilizationManager>>>, output_rect: RectI) -> std::result::Result<Arc<StabilizationManager>, String> {
        /*let source_rect = self.source_clip.get_region_of_definition(0.0)?;
        let mut source_rect = RectI {
            x1: source_rect.x1 as i32,
            x2: source_rect.x2 as i32,
            y1: source_rect.y1 as i32,
            y2: source_rect.y2 as i32
        };
        if source_rect.x1 != output_rect.x1 || source_rect.x2 != output_rect.x2 || source_rect.y1 != output_rect.y1 || source_rect.y2 != output_rect.y2 {
            source_rect = self.source_clip.get_image(0.0)?.get_bounds()?;
        }
        let in_size = ((source_rect.x2 - source_rect.x1) as usize, (source_rect.y2 - source_rect.y1) as usize);*/
        let width = usize::try_from(output_rect.x2.checked_sub(output_rect.x1).ok_or("Invalid output width")?).map_err(|_| "Invalid output width")?;
        let height = usize::try_from(output_rect.y2.checked_sub(output_rect.y1).ok_or("Invalid output height")?).map_err(|_| "Invalid output height")?;
        if width == 0 || height == 0 {
            return Err("Empty output frame".into());
        }
        let out_size = (width, height);
        let cache_context = if self.supports_output_size {
            String::new()
        } else {
            let divisor = greatest_common_divisor(width, height);
            format!("aspect:{}:{}", width / divisor, height / divisor)
        };

        self.plugin.stab_manager_standalone(&mut self.params, manager_cache, out_size, &cache_context).map_err(|e| {
            log::error!("plugin.stab_manager error: {e:?}");
            e.to_string()
        })
    }
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

impl Execute for GyroflowPlugin {
    #[allow(clippy::float_cmp)]
    fn execute(&mut self, _plugin_context: &PluginContext, action: &mut Action) -> Result<Int> {
        use Action::*;

        match *action {
            Render(ref mut effect, ref in_args) => {
                let _time = std::time::Instant::now();

                let time = in_args.get_time()?;
                let instance_data: &mut InstanceData = effect.get_instance_data()?;

                if let Some(path) = instance_data.file_path.take() {
                    let project_path = instance_data.params.get_string(Params::ProjectPath).unwrap_or_default();
                    if project_path != path {
                        let _ = instance_data.params.set_string(Params::ProjectPath, &path);
                    }
                }

                let output_image = if in_args.get_opengl_enabled().unwrap_or_default() {
                    instance_data.output_clip.load_texture_mut(time, None)?
                } else {
                    instance_data.output_clip.get_image_mut(time)?
                };
                let output_image = output_image.borrow_mut();

                let output_rect: RectI = output_image.get_region_of_definition()?;

                let src_frame = in_args.get_src_frame().ok().map(|f| f as i64);
                let stab_result = instance_data.stab_manager(&self.gyroflow_plugin.manager_cache, output_rect);
                let reason = stab_result.as_ref().err().cloned().or_else(|| stab_result.as_ref().ok().and_then(|stab| unsupported_reason(stab).map(str::to_owned)));
                if let Some(reason) = reason {
                    instance_data.unsupported_reason = Some(reason.clone());
                    instance_data.plugin.set_status(&mut instance_data.params, &format!("Pass-through: {reason}"), &reason, false);
                    let source_image = if in_args.get_opengl_enabled().unwrap_or_default() {
                        instance_data.source_clip.load_texture(time, None)?
                    } else {
                        instance_data.source_clip.get_image(time)?
                    };
                    let source_size = image_layout(source_image.get_bounds()?, source_image.get_row_bytes()?, source_image.get_pixel_depth()?);
                    let output_size = image_layout(output_image.get_bounds()?, output_image.get_row_bytes()?, output_image.get_pixel_depth()?);
                    if let (Some(source_size), Some(output_size)) = (source_size, output_size) {
                        if source_image.get_pixel_depth()? == output_image.get_pixel_depth()? && !in_args.get_metal_enabled().unwrap_or_default() && !in_args.get_opengl_enabled().unwrap_or_default() && !in_args.get_opencl_enabled().unwrap_or_default() {
                            let source_ptr = source_image.get_data()? as *const u8;
                            let output_ptr = output_image.get_data()? as *mut u8;
                            if !source_ptr.is_null() && !output_ptr.is_null() {
                                if std::ptr::eq(source_ptr, output_ptr.cast_const()) && source_size == output_size { return OK; }
                                let source_len = source_size.2.checked_mul(source_size.1).ok_or(Error::UnknownError)?;
                                let output_len = output_size.2.checked_mul(output_size.1).ok_or(Error::UnknownError)?;
                                let source_end = (source_ptr as usize).checked_add(source_len).ok_or(Error::UnknownError)?;
                                let output_end = (output_ptr as usize).checked_add(output_len).ok_or(Error::UnknownError)?;
                                let overlaps = (source_ptr as usize) < output_end && (output_ptr as usize) < source_end;
                                let source_copy = if overlaps { Some(unsafe { std::slice::from_raw_parts(source_ptr, source_len).to_vec() }) } else { None };
                                let source = source_copy.as_deref().unwrap_or_else(|| unsafe { std::slice::from_raw_parts(source_ptr, source_len) });
                                let output = unsafe { std::slice::from_raw_parts_mut(output_ptr, output_len) };
                                let copied = copy_source_frame(source, output, source_size, output_size, source_image.get_pixel_depth()?);
                                if copied { return OK; }
                            }
                        }
                    }
                    log::error!("Unable to pass through unsupported frame: {reason}");
                    return FAILED;
                }
                instance_data.unsupported_reason = None;
                let stab = stab_result.map_err(|_| Error::UnknownError)?;
                settings::apply_pinhole_lens(&stab);
                let overview = instance_data.view_mode.get_value().unwrap_or(0) == 1;
                if stab.params.read().fov_overview != overview {
                    stab.set_fov_overview(overview);
                    stab.invalidate_blocking_undistortion();
                }
                let settings = settings::Settings::new(
                    instance_data.leveling.get_value().unwrap_or(settings::DEFAULT_LEVELING),
                    instance_data.static_crop.get_value().unwrap_or(false),
                    instance_data.crop_smoothness.get_value().unwrap_or(settings::DEFAULT_CROP_SMOOTHNESS_S),
                    instance_data.zoom.get_value().unwrap_or(settings::DEFAULT_ZOOM_PERCENT),
                    instance_data.offset_x.get_value().unwrap_or(0.0),
                    instance_data.offset_y.get_value().unwrap_or(0.0),
                );
                settings::apply(&stab, &instance_data.plugin.keyframable_params, &settings);

                if !instance_data.supports_output_size {
                    let _ = instance_data.params.output_width.set_enabled(false);
                    let _ = instance_data.params.output_height.set_enabled(false);
                    let _ = instance_data.params.output_swap.set_enabled(false);
                    let _ = instance_data.params.output_size_fit.set_enabled(false);
                }
                /*if !instance_data.is_fusion_page {
                    let _ = instance_data.params.fusion_start_frame.set_enabled(false);
                }*/

                let params = stab.params.read();
                let fps = params.fps;
                let src_fps = instance_data.source_clip.get_frame_rate().unwrap_or(fps);
                if !time.is_finite() || !fps.is_finite() || fps <= 0.0 || !src_fps.is_finite() || src_fps <= 0.0 || params.size.0 == 0 || params.size.1 == 0 {
                    log::error!("Invalid render timing or source size: time={time}, fps={fps}, src_fps={src_fps}, size={:?}", params.size);
                    return FAILED;
                }
                let org_ratio = params.size.0 as f64 / params.size.1 as f64;
                let stab_output_size = params.output_size;
                let (has_accurate_timestamps, has_offsets) = {
                    let gyro = stab.gyro.read();
                    let md = gyro.file_metadata.read();
                    (md.has_accurate_timestamps, !gyro.get_offsets().is_empty())
                };

                // With a source frame from Resolve the timestamps are exact, so a timeline running at
                // a different frame rate than the clip is fine there (see DEVELOPMENT.md)
                if (src_fps - fps).abs() > 0.01 && src_frame.is_none() {
                    instance_data.plugin.set_status(&mut instance_data.params, "Timeline fps mismatch!", "Timeline frame rate doesn't match the clip frame rate! Use the plugin in Fusion instead", false);
                } else if !has_accurate_timestamps && !has_offsets {
                    instance_data.plugin.set_status(&mut instance_data.params, "Not synced. Open in Gyroflow", "Gyro data is not synced with the video, open the video in Gyroflow and add sync points (eg. by doing autosync)", false);
                } else {
                    // Constant text: Resolve sends an InstanceChanged for every status change
                    instance_data.plugin.set_status(&mut instance_data.params, "OK", "Leveling the horizon", true);
                }

                let frame_range = instance_data.source_clip.get_frame_range().ok().map(|r| (r.min, r.max));
                let duration_ms = params.duration_ms;

                let computed = gyroflow_plugin_base::timestamp::compute_timestamp(&gyroflow_plugin_base::timestamp::TimeParams {
                    time,
                    frame_range,
                    src_fps,
                    fps,
                    duration_ms,
                    src_frame,
                    is_fusion_page: instance_data.is_fusion_page,
                }, |ts| params.get_source_timestamp_at_ramped_timestamp(ts));
                let visible_frames = gyroflow_plugin_base::crop::visible_frames(&params, time, frame_range, src_frame, src_fps, instance_data.is_fusion_page);
                let crop_frame = gyroflow_plugin_base::crop::render_frame(&params, computed.timestamp_us);
                let source_size = params.size;
                drop(params);

                let time = computed.fetch_time;
                let timestamp_us = computed.timestamp_us;

                // log::info!("fps: {fps:?}, src_fps: {src_fps:?}, speed_stretch: {:.6}, time: {time:?}, timestamp_us: {timestamp_us:?}, frame_range: {frame_range:?}, src_frame: {src_frame:?}, trim_offset: {:?}", computed.speed_stretch, computed.trim_offset_frames);

                let source_image = if in_args.get_opengl_enabled().unwrap_or_default() {
                    instance_data.source_clip.load_texture(time, None)?
                } else {
                    instance_data.source_clip.get_image(time)?
                };

                let source_bounds = source_image.get_bounds()?;
                let output_bounds = output_image.get_bounds()?;
                let src_row_bytes = source_image.get_row_bytes()?;
                let out_row_bytes = output_image.get_row_bytes()?;
                let src_depth = source_image.get_pixel_depth()?;
                let out_depth = output_image.get_pixel_depth()?;
                if src_depth != out_depth {
                    log::error!("Source and output pixel depths differ: {src_depth:?} != {out_depth:?}");
                    return FAILED;
                }
                let Some(src_size) = image_layout(source_bounds, src_row_bytes, src_depth) else {
                    log::error!("Invalid source image layout: bounds={source_bounds:?}, row_bytes={src_row_bytes}, depth={src_depth:?}");
                    return FAILED;
                };
                let Some(out_size) = image_layout(output_bounds, out_row_bytes, out_depth) else {
                    log::error!("Invalid output image layout: bounds={output_bounds:?}, row_bytes={out_row_bytes}, depth={out_depth:?}");
                    return FAILED;
                };
                let src_stride = src_size.2;
                let out_stride = out_size.2;

                let src_rect = GyroflowPluginBase::get_center_rect_strict(src_size.0, src_size.1, org_ratio);

                let dont_draw_outside = instance_data.params.get_bool_at_time(Params::DontDrawOutside, TimeType::Frame(time)).map_err(|e| {
                    log::error!("Failed to read DontDrawOutside: {e}");
                    Error::UnknownError
                })?;
                let mut out_rect = if dont_draw_outside {
                    let output_ratio = out_size.0 as f64 / out_size.1 as f64;
                    let mut rect = GyroflowPluginBase::get_center_rect_strict(src_rect.2, src_rect.3, output_ratio);
                    rect.0 += src_rect.0;
                    rect.1 += src_rect.1;
                    Some(rect)
                } else if !instance_data.supports_output_size {
                    // The host forces the output to the timeline size (Resolve Edit/Color pages).
                    // If the stabilizer output aspect ratio doesn't match the output buffer,
                    // draw centered at the stabilizer aspect instead of stretching to fill
                    let stab_ratio = stab_output_size.0 as f64 / stab_output_size.1.max(1) as f64;
                    let rect = GyroflowPluginBase::get_center_rect_strict(out_size.0, out_size.1, stab_ratio);
                    if rect == (0, 0, out_size.0, out_size.1) {
                        None
                    } else {
                        Some(rect)
                    }
                } else {
                    None
                };
                if _plugin_context.get_host().get_name().as_deref().ok() == Some("com.vegascreativesoftware.vegas") {
                    out_rect = None;
                }

                let input_rotation = instance_data.params.get_f64_at_time(Params::InputRotation, TimeType::Frame(time)).ok().map(|x| x as f32);

                // The crop is solved for the source area this buffer really holds, at its scale
                let crop_request = gyroflow_plugin_base::crop::CropRequest {
                    frames: visible_frames,
                    margin: gyroflow_plugin_base::crop::source_margin(stab.stabilization.read().interpolation, source_size, src_rect),
                    response_s: settings.crop_smoothness_s,
                    input_rotation: input_rotation.unwrap_or(0.0),
                    static_crop: settings.static_crop,
                    zoom: settings.zoom,
                };
                instance_data.crop.ensure(&stab, crop_request, crop_frame);

                // log::debug!("src_size: {src_size:?} | src_rect: {src_rect:?}");
                // log::debug!("out_size: {out_size:?} | out_rect: {out_rect:?}");

                let buffers =
                    if in_args.get_opencl_enabled().unwrap_or_default() {
                        use std::ffi::c_void;
                        let queue = in_args.get_opencl_command_queue()? as *mut c_void;
                        Some((
                            BufferSource::OpenCL { texture: source_image.get_data()? as *mut c_void, queue },
                            BufferSource::OpenCL { texture: output_image.get_data()? as *mut c_void, queue },
                            false
                        ))
                    } else if in_args.get_metal_enabled().unwrap_or_default() {
                        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
                        { None }
                        #[cfg(any(target_os = "macos", target_os = "ios"))]
                        {
                            log::info!("metal: src_size: {src_size:?} | {src_stride}, out_size: {out_size:?} | {out_stride}");
                            let command_queue = in_args.get_metal_command_queue()? as *mut std::ffi::c_void;

                            Some((
                                BufferSource::MetalBuffer { buffer: source_image.get_data()? as *mut std::ffi::c_void, command_queue },
                                BufferSource::MetalBuffer { buffer: output_image.get_data()? as *mut std::ffi::c_void, command_queue },
                                instance_data.is_fusion_page
                            ))
                        }
                    } else if in_args.get_cuda_enabled().unwrap_or_default() {
                        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
                        { None }
                        #[cfg(any(target_os = "windows", target_os = "linux"))]
                        {
                            Some((
                                BufferSource::CUDABuffer { buffer: source_image.get_data()? as *mut std::ffi::c_void },
                                BufferSource::CUDABuffer { buffer: output_image.get_data()? as *mut std::ffi::c_void },
                                true
                            ))
                        }
                    } else if in_args.get_opengl_enabled().unwrap_or_default() {
                        log::info!("OpenGL: src_size: {src_size:?} | {src_stride}, out_size: {out_size:?} | {out_stride}");
                        let texture = source_image.get_opengl_texture_index()? as u32;
                        let out_texture = output_image.get_opengl_texture_index()? as u32;

                        log::info!("OpenGL in: {texture}, out: {out_texture} src_size: {src_size:?}, out_size: {out_size:?}, in_rect: {src_rect:?}, out_rect: {out_rect:?}");
                        Some((
                            BufferSource::OpenGL { texture: texture, context: std::ptr::null_mut() },
                            BufferSource::OpenGL { texture: out_texture, context: std::ptr::null_mut() },
                            true
                        ))
                    } else {
                        log::info!("CPU: src_size: {src_size:?} | {src_stride}, out_size: {out_size:?} | {out_stride}");
                        if src_row_bytes <= 0 || out_row_bytes <= 0 {
                            log::error!("CPU images require positive row bytes: source={src_row_bytes}, output={out_row_bytes}");
                            return FAILED;
                        }
                        use std::slice::from_raw_parts_mut;
                        let src_ptr = source_image.get_data()? as *mut u8;
                        let dst_ptr = output_image.get_data()? as *mut u8;
                        if src_ptr.is_null() || dst_ptr.is_null() {
                            log::error!("CPU image has a null data pointer");
                            return FAILED;
                        }
                        let src_len = src_stride.checked_mul(src_size.1).ok_or(Error::UnknownError)?;
                        let dst_len = out_stride.checked_mul(out_size.1).ok_or(Error::UnknownError)?;
                        let src_buf = unsafe { from_raw_parts_mut(src_ptr, src_len) };
                        let dst_buf = unsafe { from_raw_parts_mut(dst_ptr, dst_len) };
                        Some((
                            BufferSource::Cpu { buffer: src_buf },
                            BufferSource::Cpu { buffer: dst_buf },
                            false
                        ))
                    };

                if effect.abort()? { return FAILED; }

                if let Some(buffers) = buffers {
                    let mut buffers = Buffers {
                        input:  BufferDescription { size: src_size, rect: Some(src_rect), data: buffers.0, rotation: input_rotation, texture_copy: buffers.2 },
                        output: BufferDescription { size: out_size, rect: out_rect,       data: buffers.1, rotation: None,           texture_copy: buffers.2 }
                    };

                    let processed = match out_depth {
                        BitDepth::None  => { return FAILED; },
                        BitDepth::Byte  => stab.process_pixels::<RGBA8>  (timestamp_us, None, &mut buffers),
                        BitDepth::Short => stab.process_pixels::<RGBA16> (timestamp_us, None, &mut buffers),
                        BitDepth::Half  => stab.process_pixels::<RGBAf16>(timestamp_us, None, &mut buffers),
                        BitDepth::Float => stab.process_pixels::<RGBAf>  (timestamp_us, None, &mut buffers)
                    };
                    match processed {
                        Ok(_) => {
                            // log::info!("Rendered | {}x{} in {:.2}ms: {:?}", src_size.0, src_size.1, _time.elapsed().as_micros() as f64 / 1000.0, _);
                            OK
                        },
                        Err(e) => {
                            log::warn!("Failed to render: {e:?}");
                            FAILED
                        }
                    }
                } else {
                    FAILED
                }
            }

            CreateInstance(ref mut effect) => {
                let param_set = effect.parameter_set()?;
                // let mut effect_props: EffectInstance = effect.properties()?;

                let source_clip = effect.get_simple_input_clip()?;
                let output_clip = effect.get_output_clip()?;

                let mut instance_data = InstanceData {
                    leveling:        param_set.parameter("Leveling")?,
                    static_crop:     param_set.parameter("StaticCrop")?,
                    crop_smoothness: param_set.parameter("CropSmoothness")?,
                    zoom:            param_set.parameter("CropZoom")?,
                    offset_x:        param_set.parameter("OffsetX")?,
                    offset_y:        param_set.parameter("OffsetY")?,
                    view_mode:       param_set.parameter("ViewMode")?,
                    crop: Default::default(),
                    source_clip,
                    output_clip,
                    supports_output_size: true,
                    is_fusion_page: false,
                    file_path: None,
                    unsupported_reason: None,
                    params: ParamHandler {
                        instance_id:              param_set.parameter("InstanceId")?,
                        project_data:             param_set.parameter("ProjectData")?,
                        embedded_lens:            param_set.parameter("EmbeddedLensProfile")?,
                        embedded_preset:          param_set.parameter("EmbeddedPreset")?,
                        project_path:             param_set.parameter("ProjectPath")?,
                        disable_stretch:          param_set.parameter("DisableStretch")?,
                        status:                   param_set.parameter("Status")?,
                        open_in_gyroflow:         param_set.parameter("OpenGyroflow")?,
                        reload_project:           param_set.parameter("ReloadProject")?,
                        toggle_overview:          param_set.parameter("ToggleOverview")?,
                        dont_draw_outside:        param_set.parameter("DontDrawOutside")?,
                        include_project_data:     param_set.parameter("IncludeProjectData")?,
                        input_rotation:           param_set.parameter("InputRotation")?,
                        use_gyroflows_keyframes:  param_set.parameter("UseGyroflowsKeyframes")?,
                        fov:                      param_set.parameter("Fov")?,
                        smoothness:               param_set.parameter("Smoothness")?,
                        zoom_limit:               param_set.parameter("ZoomLimit")?,
                        lens_correction_strength: param_set.parameter("LensCorrectionStrength")?,
                        horizon_lock_amount:      param_set.parameter("HorizonLockAmount")?,
                        horizon_lock_roll:        param_set.parameter("HorizonLockRoll")?,
                        video_speed:              param_set.parameter("VideoSpeed")?,
                        //positionx:                param_set.parameter("PositionX")?,
                        //positiony:                param_set.parameter("PositionY")?,
                        additional_pitch:         param_set.parameter("AdditionalPitch")?,
                        additional_yaw:           param_set.parameter("AdditionalYaw")?,
                        rotation:                 param_set.parameter("Rotation")?,
                        output_width:             param_set.parameter("OutputWidth")?,
                        output_height:            param_set.parameter("OutputHeight")?,
                        output_swap:              param_set.parameter("OutputSizeSwap")?,
                        output_size_fit:          param_set.parameter("OutputSizeToTimeline")?,
                        interpolation:            param_set.parameter("Interpolation")?,
                        integration_method:       param_set.parameter("IntegrationMethod")?,

                        loaded_project:           param_set.parameter("LoadedProject")?,
                        loaded_lens:              param_set.parameter("LoadedLens")?,
                        loaded_preset:            param_set.parameter("LoadedPreset")?,

                        //fusion_start_frame:       param_set.parameter("FusionStartFrame")?,

                        fields: Default::default(),
                    },
                    plugin: GyroflowPluginBaseInstance {
                        managers:                    LruCache::new(std::num::NonZeroUsize::new(20).unwrap()),
                        original_output_size:        (0, 0),
                        original_video_size:         (0, 0),
                        timeline_size:               (0, 0),
                        num_frames:                  0,
                        fps:                         0.0,
                        reload_values_from_project:  false,
                        ever_changed:                false,
                        opencl_disabled:             false,
                        cache_keyframes_every_frame: true,
                        framebuffer_inverted:        true,
                        anamorphic_adjust_size:      true,
                        always_set_input_rotation:   false,
                        fill_output_to_host_aspect:  false,
                        has_motion:                  false,
                        keyframable_params: Arc::new(RwLock::new(KeyframableParams {
                            use_gyroflows_keyframes: param_set.parameter::<Bool>("UseGyroflowsKeyframes")?.get_value()?,
                            cached_keyframes:        KeyframeManager::default()
                        })),
                    },
                };
                let mut instance_id = instance_data.params.get_string(Params::InstanceId).unwrap_or_default();
                instance_data.plugin.initialize_instance_id(&mut instance_id);
                let _ = instance_data.params.set_string(Params::InstanceId, &instance_id);

                let props: EffectInstance = effect.properties()?;
                if matches!(props.get_resolve_page().as_deref(), Ok("Edit") | Ok("Color")) {
                    instance_data.supports_output_size = false;
                    // These pages force timeline-sized buffers: fill the frame with a
                    // timeline-aspect crop of the source (no bars, no stretching), letting
                    // adaptive zoom use the cropped-away headroom
                    instance_data.plugin.fill_output_to_host_aspect = true;
                }
                if matches!(props.get_resolve_page().as_deref(), Ok("Fusion")) {
                    instance_data.is_fusion_page = true;
                }
                if let Ok(path) = props.get_src_file_path() {
                    if !path.is_empty() {
                        instance_data.file_path = Some(path.clone());
                    }
                }

                effect.set_instance_data(instance_data)?;

                OK
            }
            InstanceChanged(ref mut effect, ref mut in_args) => {
                // Sent synchronously from CreateInstance, before the instance data exists
                if in_args.get_name()? == "InstanceId" {
                    return OK;
                }
                let instance_data: &mut InstanceData = effect.get_instance_data()?;
                if in_args.get_name()? == "Source" || in_args.get_name()? == "Output" || in_args.get_name()? == "ResolveUseAlphaForTrackCompositing" {
                    log::info!("InstanceChanged {:?} {:?}", in_args.get_name()?, in_args.get_change_reason()?);
                    return OK;
                }

                if in_args.get_change_reason()? == Change::UserEdited {
                    log::info!("InstanceChanged {:?}", in_args.get_name()?);
                }
                // Everything else is applied in Render, where the stabilizer is available
                if in_args.get_name()? == "StaticCrop" {
                    let static_crop = instance_data.static_crop.get_value()?;
                    let _ = instance_data.crop_smoothness.set_enabled(!static_crop);
                }
                OK
            }

            IsIdentity(ref mut effect, ref _in_args, ref mut out_args) => {
                let instance_data: &InstanceData = effect.get_instance_data()?;
                if instance_data.unsupported_reason.is_some() {
                    out_args.set_name(&image_effect_simple_source_clip_name())?;
                    OK
                } else {
                    REPLY_DEFAULT
                }
            }

            GetRegionOfDefinition(ref mut effect, ref in_args, ref mut out_args) => {
                let time = in_args.get_time()?;
                let instance_data = effect.get_instance_data::<InstanceData>()?;
                let rod = instance_data.source_clip.get_region_of_definition(time)?;
                if !instance_data.supports_output_size {
                    // Resolve Edit/Color pages: buffers are always timeline-sized and Resolve
                    // doesn't consult this action in practice (verified with logging) — the
                    // output fills the frame with a timeline-aspect crop of the source, so
                    // just pass the host-reported RoD through
                    log::debug!("GetRegionOfDefinition: time: {time}, source rod: {:?}", (rod.x1, rod.y1, rod.x2, rod.y2));
                    out_args.set_effect_region_of_definition(rod)?;
                    return OK;
                }
                let mut out_rod = rod;
                let dont_draw_outside = instance_data.params.get_bool_at_time(Params::DontDrawOutside, TimeType::Frame(time)).map_err(|e| {
                    log::error!("Failed to read DontDrawOutside: {e}");
                    Error::UnknownError
                })?;
                if instance_data.plugin.original_output_size != (0, 0) && !dont_draw_outside {
                    out_rod.x2 = instance_data.plugin.original_output_size.0 as f64;
                    out_rod.y2 = instance_data.plugin.original_output_size.1 as f64;
                }
                if let Ok(ow) = instance_data.params.get_f64(Params::OutputWidth)  { out_rod.x2 = ow; }
                if let Ok(oh) = instance_data.params.get_f64(Params::OutputHeight) { out_rod.y2 = oh; }
                out_args.set_effect_region_of_definition(out_rod)?;

                OK
            }

            DestroyInstance(ref mut effect) => {
                effect.get_instance_data::<InstanceData>()?.plugin.clear_stab(&self.gyroflow_plugin.manager_cache);
                OK
            },
            PurgeCaches(ref mut effect) => {
                effect.get_instance_data::<InstanceData>()?.plugin.clear_stab(&self.gyroflow_plugin.manager_cache);
                OK
            },

            DescribeInContext(ref mut effect, ref _in_args) => {
                let mut output_clip = effect.new_output_clip()?;
                output_clip.set_supported_components(&[ImageComponent::RGBA])?;

                let mut input_clip = effect.new_simple_input_clip()?;
                input_clip.set_supported_components(&[ImageComponent::RGBA])?;

                let mut param_set = effect.parameter_set()?;

                fn define_param(param_set: &mut ParamSetHandle, x: ParameterType, group: Option<&'static str>) -> Result<Int> {
                    match x {
                        ParameterType::HiddenString { id } => {
                            let mut param = param_set.param_define_string(id)?;
                            let _ = param.set_script_name(id);
                            param.set_secret(true)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Button { id, label, hint } => {
                            if id == "CreateCamera" { return OK; }
                            let mut param = param_set.param_define_button(id)?;
                            let _ = param.set_script_name(id);
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::TextBox { id, label, hint } => {
                            let mut param = param_set.param_define_string(id)?;
                            let _ = param.set_script_name(id);
                            param.set_string_type(ParamStringType::SingleLine)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Text { id, label, hint } => {
                            let mut param = param_set.param_define_string(id)?;
                            param.set_string_type(ParamStringType::SingleLine)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            //param.set_enabled(false)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Slider { id, label, hint, min, max, default } => {
                            let mut param = param_set.param_define_double(id)?;
                            param.set_default(default)?;
                            param.set_display_min(min)?;
                            param.set_display_max(max)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            let _ = param.set_script_name(id);
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Checkbox { id, label, hint, default } => {
                            if id == "StabilizationSpeedRamp" { return OK; }
                            let mut param = param_set.param_define_boolean(id)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            param.set_default(default)?;
                            let _ = param.set_script_name(id);
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Select { id, label, hint, options, default } => {
                            let mut param = param_set.param_define_choice(id)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            param.set_default(options.iter().position(|x| *x == default).unwrap_or(0) as i32)?;
                            param.set_choices(&options)?;
                            let _ = param.set_script_name(id);
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }
                        }
                        ParameterType::Group { id, label, parameters, opened } => {
                            let mut param = param_set.param_define_group(id)?;
                            param.set_label(label)?;
                            param.set_group_open(opened)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                            if is_hidden_in_panel(id) { param.set_secret(true)?; }

                            for x in parameters {
                                define_param(param_set, x, Some(id))?;
                            }
                        }
                    }
                    OK
                }

                for param in GyroflowPluginBase::get_param_definitions() {
                    define_param(&mut param_set, param, None)?;
                }
                for param in horizon_param_definitions() {
                    define_param(&mut param_set, param, Some("AdjustGroup"))?;
                }

                param_set
                    .param_define_page("Main")?
                    .set_children(&[
                        "ProjectGroup",
                        "AdjustGroup",
                        "ViewMode"
                    ])?;

                OK
            }

            OpenGLContextAttached(ref mut _effect) => { self.gyroflow_plugin.initialize_gpu_context();   OK },
            OpenGLContextDetached(ref mut _effect) => { self.gyroflow_plugin.deinitialize_gpu_context(); OK },

            Describe(ref mut effect) => {
                let supports_opencl = _plugin_context.get_host().get_opencl_render_supported().unwrap_or_default() == "true";
                let supports_opengl = _plugin_context.get_host().get_opengl_render_supported().unwrap_or_default() == "true";
                let supports_cuda   = _plugin_context.get_host().get_cuda_render_supported().unwrap_or_default() == "true";
                let supports_metal  = _plugin_context.get_host().get_metal_render_supported().unwrap_or_default() == "true";

                log::info!("Host name: {:?}", _plugin_context.get_host().get_name());
                log::info!("Host version: {:?}", _plugin_context.get_host().get_version_label());
                log::info!("Host supports OpenGL: {:?}", supports_opengl);
                log::info!("Host supports OpenCL: {:?}", supports_opencl);
                log::info!("Host supports CUDA: {:?}", supports_cuda);
                log::info!("Host supports Metal: {:?}", supports_metal);
                let mut effect_properties: EffectDescriptor = effect.properties()?;
                effect_properties.set_grouping("Warp")?;

                effect_properties.set_label("Horizon Leveling")?;
                effect_properties.set_short_label("Horizon Leveling")?;
                effect_properties.set_long_label("Horizon Leveling")?;

                effect_properties.set_supported_pixel_depths(&[BitDepth::Byte, BitDepth::Short, BitDepth::Float])?;
                effect_properties.set_supported_contexts(&[ImageEffectContext::Filter])?;
                effect_properties.set_supports_tiles(false)?;

                effect_properties.set_single_instance(false)?;
                effect_properties.set_host_frame_threading(false)?;
                // The pinned ofx-rs wrapper uses an unsynchronized global registry and
                // exposes instance data as a raw mutable reference. Concurrent OFX calls
                // are therefore unsafe even if the stabilization core is thread-safe.
                effect_properties.set_render_thread_safety(ImageEffectRender::Unsafe)?;
                effect_properties.set_supports_multi_resolution(true)?;
                effect_properties.set_temporal_clip_access(true)?;

                OK
            }

            Load => {
				self.gyroflow_plugin.initialize_log("openfx-horizon");
                OK
            },

            _ => REPLY_DEFAULT,
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// The roll the correction takes out at a gyro timestamp, the way the core's horizon lock reads it, in degrees
    fn correction_roll_deg(stab: &StabilizationManager, timestamp_ms: f64) -> f64 {
        let m = stab.gyro.read().smoothed_quat_at_timestamp(timestamp_ms).to_rotation_matrix();
        (-m[(0, 1)]).atan2(m[(0, 0)]).to_degrees()
    }

    #[test]
    #[ignore]
    fn real_clip_levels_without_showing_past_the_source() {
        let path = std::env::var("GF_TEST_VIDEO").expect("set GF_TEST_VIDEO to a GoPro clip with embedded telemetry");
        let stab = Arc::new(StabilizationManager::default());
        load_standalone_video(&stab, &path).unwrap();
        if let Some(reason) = unsupported_reason(&stab) {
            panic!("{path}: pass-through: {reason}");
        }
        settings::apply_pinhole_lens(&stab);
        assert!(stab.lens.read().identifier.starts_with(cameras::PINHOLE_IDENTIFIER));

        let (test_width, test_height) = if std::env::var_os("GF_TEST_FULL_RES").is_some() { (1920, 1080) } else { (320, 180) };
        // As the OFX instances run it
        stab.params.write().framebuffer_inverted = true;
        stab.stabilization.write().interpolation = Interpolation::Lanczos4;
        stab.set_output_size(test_width, test_height);
        stab.init_size();
        let keyframable = Arc::new(RwLock::new(KeyframableParams { use_gyroflows_keyframes: false, cached_keyframes: KeyframeManager::default() }));
        let provider = keyframable.clone();
        stab.keyframes.write().set_custom_provider(move |_, typ, ts| provider.read().cached_keyframes.value_at_video_timestamp(typ, ts));
        let (fps, frame_count, source_size) = {
            let p = stab.params.read();
            (p.get_scaled_fps(), p.frame_count, p.size)
        };
        assert!(fps > 0.0 && frame_count > 1);

        // Resolve's buffer: the source pillarboxed in opaque black, in float. The renderer's background past the
        // source area is far out of range, so any sample of it at a visible weight shows; 8-bit output would truncate
        stab.set_background_color(nalgebra::Vector4::new(-1000.0, -1000.0, -1000.0, -1000.0));
        let source_rect = GyroflowPluginBase::get_center_rect_strict(test_width, test_height, source_size.0 as f64 / source_size.1 as f64);
        let mut input = vec![0u8; test_width * test_height * 16];
        for (index, pixel) in input.chunks_exact_mut(16).enumerate() {
            let (x, y) = (index % test_width, index / test_width);
            let inside = x >= source_rect.0 && x < source_rect.0 + source_rect.2 && y >= source_rect.1 && y < source_rect.1 + source_rect.3;
            let value = if inside { [1.0f32; 4] } else { [0.0, 0.0, 0.0, 1.0] };
            for (bytes, channel) in pixel.chunks_exact_mut(4).zip(value) { bytes.copy_from_slice(&channel.to_ne_bytes()); }
        }
        let channels = |pixel: &[u8]| pixel.chunks_exact(4).map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<_>>();
        let mut output = vec![0u8; input.len()];
        let every_frame = std::env::var_os("GF_TEST_ALL_FRAMES").is_some();
        let mut crop = gyroflow_plugin_base::crop::CropState::default();
        let cases = [
            // leveling, static, smoothness, zoom, x, y
            (100.0, false, 4.0, 100.0, 0.0, 0.0),
            (50.0, false, 4.0, 100.0, 0.0, 0.0),
            (0.0, false, 4.0, 100.0, 0.0, 0.0),
            (100.0, true, 4.0, 100.0, 0.0, 0.0),
            (100.0, false, 1.0, 100.0, 0.0, 0.0),
            (100.0, false, 8.0, 150.0, 0.0, 0.0),
            (100.0, false, 4.0, 100.0, 30.0, -30.0),
            (100.0, false, 4.0, 100.0, -30.0, 30.0),
        ];
        for (leveling, static_crop, smoothness, zoom, x, y) in cases {
            let settings = settings::Settings::new(leveling, static_crop, smoothness, zoom, x, y);
            settings::apply(&stab, &keyframable, &settings);
            let request = gyroflow_plugin_base::crop::CropRequest {
                frames: (0, frame_count - 1),
                margin: gyroflow_plugin_base::crop::source_margin(Interpolation::Lanczos4, source_size, source_rect),
                response_s: settings.crop_smoothness_s,
                input_rotation: 0.0,
                static_crop: settings.static_crop,
                zoom: settings.zoom,
            };
            let report = crop.ensure(&stab, request, 0).expect("changed settings solve the crop again");
            let label = format!("leveling {leveling}%, static {static_crop}, smoothness {smoothness}s, zoom {zoom}%, offset ({x}, {y})");
            if let Some((k, slack)) = report.slack.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1)) {
                assert!(*slack >= 0.0, "frame {} renders {:.2} px past the source edge, {label}", report.frames.0 + k, -slack);
            }
            assert!(report.fovs.iter().zip(&report.bounds).all(|(fov, bound)| fov.is_finite() && *fov > 0.0 && *fov <= bound + 1e-9));
            if static_crop {
                assert!(report.fovs.windows(2).all(|w| w[0] == w[1]), "static crop changes, {label}");
            }

            // The applied roll at an even spread of frames
            let rolls: Vec<f64> = (0..=200).map(|s| correction_roll_deg(&stab, s as f64 * stab.params.read().duration_ms / 200.0)).collect();
            let max_roll = rolls.iter().fold(0.0f64, |m, r| m.max(r.abs()));
            if leveling == 0.0 {
                assert!(max_roll < 1e-6, "rotates by up to {max_roll:.4} deg at 0%");
            }

            // The frames closest to the edge, and an even spread
            let mut closest: Vec<usize> = (0..report.slack.len()).collect();
            closest.sort_by(|a, b| report.slack[*a].total_cmp(&report.slack[*b]));
            let mut frames: Vec<usize> = if every_frame {
                (0..frame_count).collect()
            } else {
                closest.into_iter().take(6).map(|k| report.frames.0 + k).chain((0..=12).map(|s| s * (frame_count - 1) / 12)).collect()
            };
            frames.sort_unstable();
            frames.dedup();
            for frame in frames {
                let timestamp_us = (frame as f64 * 1_000_000.0 / fps).round() as i64;
                assert_eq!(gyroflow_plugin_base::crop::render_frame(&stab.params.read(), timestamp_us), frame);
                output.fill(0);
                let mut buffers = Buffers {
                    input: BufferDescription { size: (test_width, test_height, test_width * 16), rect: Some(source_rect), data: BufferSource::Cpu { buffer: &mut input }, rotation: None, texture_copy: false },
                    output: BufferDescription { size: (test_width, test_height, test_width * 16), rect: None, data: BufferSource::Cpu { buffer: &mut output }, rotation: None, texture_copy: false },
                };
                stab.process_pixels::<RGBAf>(timestamp_us, None, &mut buffers).unwrap();
                let uncovered = output.chunks_exact(16).enumerate()
                    .filter(|(_, pixel)| channels(pixel).iter().any(|c| (c - 1.0).abs() > 1e-4))
                    .take(4).map(|(index, pixel)| (index % test_width, index / test_width, channels(pixel))).collect::<Vec<_>>();
                assert!(uncovered.is_empty(), "uncovered {uncovered:?} at frame {frame}, {label}");
            }
            let tightest = report.fovs.iter().copied().fold(f64::INFINITY, f64::min);
            let widest = report.fovs.iter().copied().fold(0.0f64, f64::max);
            let closest = report.slack.iter().copied().fold(f64::INFINITY, f64::min);
            println!("{path}: {label}: roll taken out up to {max_roll:.2} deg, zoom {:.1}%..{:.1}%, closest sample {closest:.2} px inside the margin, {} of {} bounds lowered by the render check, solved in {:.0} ms",
                100.0 / widest, 100.0 / tightest, report.corrected, report.fovs.len(), report.elapsed_ms);
        }

        // At 100%, the image's roll against gravity (GoPro GRAV, rotated into the image by IORI) should be what's taken out
        settings::apply(&stab, &keyframable, &settings::Settings::new(100.0, false, 4.0, 100.0, 0.0, 0.0));
        stab.recompute_blocking();
        let gyro = stab.gyro.read();
        let gravity = gyro.file_metadata.read().gravity_vectors.clone();
        match gravity.filter(|g| !g.is_empty()) {
            None => println!("{path}: no gravity vectors to compare the roll against"),
            Some(gravity) => {
                drop(gyro);
                let pairs: Vec<(f64, f64)> = (1..200).filter_map(|s| {
                    let ts_ms = s as f64 * stab.params.read().duration_ms / 200.0;
                    let g = gyroflow_core::smoothing::horizon::HorizonLock::interpolate_gravity_vector(&gravity, (ts_ms * 1000.0) as i64)?;
                    Some((g.x.atan2(g.y).to_degrees(), correction_roll_deg(&stab, ts_ms)))
                }).collect();
                let rms = |sign: f64| (pairs.iter().map(|(g, c)| (g - sign * c).powi(2)).sum::<f64>() / pairs.len() as f64).sqrt();
                let spread = pairs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (g, _)| (lo.min(*g), hi.max(*g)));
                println!("{path}: gravity roll {:.1}..{:.1} deg; RMS difference to the correction {:.2} deg (same sign) / {:.2} deg (opposite sign)", spread.0, spread.1, rms(1.0), rms(-1.0));
            }
        }
    }

    #[test]
    fn pass_through_preserves_pixels_across_different_strides() {
        let source = [1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 99, 99];
        let mut output = [0; 24];
        assert!(copy_source_frame(&source, &mut output, (2, 2, 10), (2, 2, 12), BitDepth::Byte));
        assert_eq!(&output[..8], &source[..8]);
        assert_eq!(&output[12..20], &source[10..18]);
        assert!(!copy_source_frame(&source, &mut output, (2, 2, 10), (3, 2, 12), BitDepth::Byte));
    }

    #[test]
    fn layout_uses_actual_bounds_and_stride() {
        let bounds = RectI { x1: 100, y1: 50, x2: 2020, y2: 1130 };
        assert_eq!(image_layout(bounds, 8192, BitDepth::Byte), Some((1920, 1080, 8192)));
    }

    #[test]
    fn layout_derives_texture_stride_from_pixel_depth() {
        let bounds = RectI { x1: 0, y1: 0, x2: 100, y2: 50 };
        assert_eq!(image_layout(bounds, 0, BitDepth::Half), Some((100, 50, 800)));
        assert_eq!(image_layout(bounds, 0, BitDepth::Float), Some((100, 50, 1600)));
    }

    #[test]
    fn layout_rejects_invalid_geometry_and_strides() {
        let valid = RectI { x1: 0, y1: 0, x2: 100, y2: 50 };
        let inverted = RectI { x1: 100, y1: 0, x2: 0, y2: 50 };
        assert_eq!(image_layout(valid, -400, BitDepth::Byte), None);
        assert_eq!(image_layout(valid, 399, BitDepth::Byte), None);
        assert_eq!(image_layout(inverted, 400, BitDepth::Byte), None);
        assert_eq!(image_layout(valid, 400, BitDepth::None), None);
    }

    #[test]
    fn aspect_context_is_stable_across_proxy_resolutions() {
        let normalize = |width, height| {
            let divisor = greatest_common_divisor(width, height);
            (width / divisor, height / divisor)
        };
        assert_eq!(normalize(3840, 2160), normalize(1920, 1080));
        assert_eq!(normalize(1920, 1080), (16, 9));
        assert_ne!(normalize(1920, 1080), normalize(1440, 1080));
    }
}
