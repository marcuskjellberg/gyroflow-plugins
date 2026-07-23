use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use ofx::*;
use super::fuscript::*;
use gyroflow_plugin_base::*;
use gyroflow_plugin_base::parking_lot::{ Mutex, RwLock };
use gyroflow_plugin_base::lru::LruCache;

plugin_module!(
    "xyz.gyroflow",
    ApiVersion(1),
    PluginVersion(1, 2),
    GyroflowPlugin::default
);

#[derive(Default)]
struct GyroflowPlugin {
    gyroflow_plugin: GyroflowPluginBase,
}

pub fn frame_from_timetype(time: TimeType) -> f64 {
    match time {
        TimeType::Frame(x) => x,
        TimeType::FrameOrMicrosecond((Some(x), _)) => x,
        _ => panic!("Shouldn't happen"),
    }
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
    get_bool_at_time: _s p, t    { Ok(p.get_value_at_time(frame_from_timetype(t))?) },
    get_f64_at_time:  _s p, t    { Ok(p.get_value_at_time(frame_from_timetype(t))?) },
    set_f64_at_time:  _s p, t, v { Ok(p.set_value_at_time(frame_from_timetype(t), v)?) },
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
    /// The source video path of this instance (kept for matching scripting-query results)
    video_path: Option<String>,
    /// Trim of the timeline item queried from Resolve's scripting API:
    /// (left offset into the source media, duration), in frames
    resolve_trim: Option<(f64, f64)>,
    /// The pending scripting query was automatic (not user-initiated):
    /// only consume the trim info from it, don't touch the project path
    trim_query_auto: bool,
    /// When the last automatic trim query was issued (rate-limits re-queries)
    last_trim_query: Option<std::time::Instant>,

    current_file_info_pending: Arc<AtomicBool>,
    current_file_info: Arc<Mutex<Option<CurrentFileInfo>>>,
}

impl InstanceData {
    fn stab_manager(&mut self, manager_cache: &Mutex<LruCache<String, Arc<StabilizationManager>>>, output_rect: RectI, loading_pending_video_file: bool) -> Result<Arc<StabilizationManager>> {
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
        let out_size = ((output_rect.x2 - output_rect.x1) as usize, (output_rect.y2 - output_rect.y1) as usize);

        self.plugin.stab_manager(&mut self.params, manager_cache, out_size, loading_pending_video_file).map_err(|e| {
            log::error!("plugin.stab_manager error: {e:?}");
            Error::UnknownError
        })
    }
    pub fn check_pending_file_info(&mut self) -> Result<bool> { // -> is_video_file
        if self.current_file_info_pending.load(SeqCst) {
            self.current_file_info_pending.store(false, SeqCst);
            let lock = self.current_file_info.lock();
            if let Some(ref current_file) = *lock {
                // Consume the timeline item's trim, but only if the queried item plays
                // this instance's video (the scripting API returns whatever is under the
                // playhead, which can be a different clip)
                if let Some(trim) = current_file.trim_frames {
                    let matches = self.video_path.as_deref().map(|p| p == current_file.file_path).unwrap_or(true);
                    if matches {
                        if self.resolve_trim != Some(trim) {
                            log::info!("Timeline item trim from Resolve: left offset {} frames, duration {} frames", trim.0, trim.1);
                            self.resolve_trim = Some(trim);
                        }
                    } else {
                        log::info!("Ignoring trim of a different timeline item: {} != {:?}", current_file.file_path, self.video_path);
                    }
                }
                if self.trim_query_auto {
                    self.trim_query_auto = false;
                    return Ok(false);
                }
                if let Some(proj) = &current_file.project_path {
                    self.params.set_string(Params::ProjectPath, &proj).unwrap(); // TODO: unwrap
                } else {
                    // Try to use the video directly
                    self.params.set_string(Params::ProjectPath, &current_file.file_path).unwrap(); // TODO: unwrap
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
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
                    let new_project_path = gyroflow_plugin_base::GyroflowPluginBase::get_project_path(&path).unwrap_or(path);
                    if project_path.is_empty() || project_path != new_project_path {
                        let _ = instance_data.params.set_string(Params::ProjectPath, &new_project_path);
                    }
                }

                let loading_pending_video_file = instance_data.check_pending_file_info()?;

                let output_image = if in_args.get_opengl_enabled().unwrap_or_default() {
                    instance_data.output_clip.load_texture_mut(time, None)?
                } else {
                    instance_data.output_clip.get_image_mut(time)?
                };
                let output_image = output_image.borrow_mut();

                let output_rect: RectI = output_image.get_region_of_definition()?;

                let stab = instance_data.stab_manager(&self.gyroflow_plugin.manager_cache, output_rect, loading_pending_video_file)?;

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
                let org_ratio = params.size.0 as f64 / params.size.1 as f64;
                let stab_output_size = params.output_size;
                let (has_accurate_timestamps, has_offsets) = {
                    let gyro = stab.gyro.read();
                    let md = gyro.file_metadata.read();
                    (md.has_accurate_timestamps, !gyro.get_offsets().is_empty())
                };

                if (src_fps - fps).abs() > 0.01 {
                    instance_data.plugin.set_status(&mut instance_data.params, "Timeline fps mismatch!", "Timeline frame rate doesn't match the clip frame rate! Use the plugin in Fusion instead", false);
                } else if !has_accurate_timestamps && !has_offsets {
                    instance_data.plugin.set_status(&mut instance_data.params, "Not synced. Open in Gyroflow", "Gyro data is not synced with the video, open the video in Gyroflow and add sync points (eg. by doing autosync)", false);
                } else {
                    instance_data.plugin.set_status(&mut instance_data.params, "OK", "OK", true);
                }

                let frame_range = instance_data.source_clip.get_frame_range().ok().map(|r| (r.min, r.max));
                let src_frame = in_args.get_src_frame().ok().map(|f| f as i64);
                let duration_ms = params.duration_ms;

                let computed = crate::timestamp::compute_timestamp(&crate::timestamp::TimeParams {
                    time,
                    frame_range,
                    src_fps,
                    fps,
                    duration_ms,
                    src_frame,
                    is_fusion_page: instance_data.is_fusion_page,
                }, |ts| params.get_source_timestamp_at_ramped_timestamp(ts));
                drop(params);

                let time = computed.fetch_time;
                let timestamp_us = computed.timestamp_us;

                // log::info!("fps: {fps:?}, src_fps: {src_fps:?}, speed_stretch: {:.6}, time: {time:?}, timestamp_us: {timestamp_us:?}, frame_range: {frame_range:?}, src_frame: {src_frame:?}, trim_offset: {:?}", computed.speed_stretch, computed.trim_offset_frames);

                // Feed the timeline item's trim (queried from Resolve's scripting API) into
                // gyroflow-core, so that adaptive zoom is solved over the visible sub-clip
                // instead of the whole source recording — just like trimming in the Gyroflow app.
                if duration_ms > 0.0 && fps > 0.0 {
                    // Sanity check: the source frames actually rendered must lie inside the
                    // queried trim, otherwise the query hit a different timeline item
                    if let (Some((left, len)), Some(sf)) = (instance_data.resolve_trim, src_frame) {
                        let sf = sf as f64;
                        if sf + 3.0 < left || sf > left + len + 3.0 {
                            log::info!("Rendered source frame {sf} is outside the queried trim ({left} + {len} frames) — discarding trim info");
                            instance_data.resolve_trim = None;
                            // The trim likely changed (e.g. the clip was extended) — re-query it
                            if CurrentFileInfo::is_available() && instance_data.last_trim_query.map(|t| t.elapsed().as_secs_f64() > 3.0).unwrap_or(true) {
                                instance_data.trim_query_auto = true;
                                instance_data.last_trim_query = Some(std::time::Instant::now());
                                CurrentFileInfo::query(instance_data.current_file_info.clone(), instance_data.current_file_info_pending.clone(), true);
                            }
                        }
                    }
                    if let Some((left_frames, len_frames)) = instance_data.resolve_trim {
                        let start_ms = (left_frames / fps * 1000.0).max(0.0);
                        let end_ms = (((left_frames + len_frames) / fps) * 1000.0).min(duration_ms);
                        if end_ms > start_ms {
                            // Tolerate a few frames of rounding/jitter before re-solving the zoom
                            let tolerance_ms = 3.0 * 1000.0 / fps;
                            let new_range = if start_ms <= tolerance_ms && end_ms >= duration_ms - tolerance_ms {
                                (0.0, 1.0) // Full clip -> no trim
                            } else {
                                (start_ms / duration_ms, end_ms / duration_ms)
                            };
                            let current = stab.trim_ranges().first().cloned().unwrap_or((0.0, 1.0));
                            if ((current.0 - new_range.0) * duration_ms).abs() > tolerance_ms || ((current.1 - new_range.1) * duration_ms).abs() > tolerance_ms {
                                log::info!("Setting trim range: {:?} ms of {duration_ms:?} ms", (new_range.0 * duration_ms, new_range.1 * duration_ms));
                                stab.set_trim_ranges(vec![new_range]);
                                stab.invalidate_blocking_smoothing();
                            }
                        }
                    }
                }
                let source_image = if in_args.get_opengl_enabled().unwrap_or_default() {
                    instance_data.source_clip.load_texture(time, None)?
                } else {
                    instance_data.source_clip.get_image(time)?
                };

                let source_rect: RectI = source_image.get_region_of_definition()?;

                let src_stride = source_image.get_row_bytes()? as usize;
                let out_stride = output_image.get_row_bytes()? as usize;
                let mut src_size = ((source_rect.x2 - source_rect.x1) as usize, (source_rect.y2 - source_rect.y1) as usize, src_stride);
                let mut out_size = ((output_rect.x2 - output_rect.x1) as usize, (output_rect.y2 - output_rect.y1) as usize, out_stride);

                if src_size.2 <= 0 { src_size.2 = src_size.0 * 4 * 4 }; // assuming 32-bit float
                if out_size.2 <= 0 { out_size.2 = out_size.0 * 4 * 4 }; // assuming 32-bit float

                let src_rect = GyroflowPluginBase::get_center_rect(src_size.0, src_size.1, org_ratio);

                let mut out_rect = if instance_data.params.get_bool_at_time(Params::DontDrawOutside, TimeType::Frame(time)).unwrap() { // TODO: unwrap
                    let output_ratio = out_size.0 as f64 / out_size.1 as f64;
                    let mut rect = GyroflowPluginBase::get_center_rect(src_rect.2, src_rect.3, output_ratio);
                    rect.0 += src_rect.0;
                    rect.1 += src_rect.1;
                    Some(rect)
                } else if !instance_data.supports_output_size {
                    // The host forces the output to the timeline size (Resolve Edit/Color pages).
                    // If the stabilizer output aspect ratio doesn't match the output buffer,
                    // draw centered at the stabilizer aspect instead of stretching to fill
                    let stab_ratio = stab_output_size.0 as f64 / stab_output_size.1.max(1) as f64;
                    let rect = GyroflowPluginBase::get_center_rect(out_size.0, out_size.1, stab_ratio);
                    if rect == (0, 0, out_size.0, out_size.1) {
                        None
                    } else {
                        Some(rect)
                    }
                } else {
                    None
                };
                let out_scale = output_image.get_render_scale()?;
                if (out_scale.x != 1.0 || out_scale.y != 1.0) && !in_args.get_opengl_enabled().unwrap_or_default() {
                    // log::debug!("out_scale: {:?}", out_scale);
                    let h = (out_size.1 as f64 * out_scale.y as f64).round() as usize;
                    if out_size.1 > h {
                        // Only part of the output buffer is valid at reduced render scale:
                        // scale any existing rect into that region instead of discarding it
                        let base = out_rect.unwrap_or((0, 0, out_size.0, out_size.1));
                        out_rect = Some((
                            (base.0 as f64 * out_scale.x as f64).round() as usize,
                            out_size.1 - h + (base.1 as f64 * out_scale.y as f64).round() as usize, // because the coordinates are inverted
                            (base.2 as f64 * out_scale.x as f64).round() as usize,
                            (base.3 as f64 * out_scale.y as f64).round() as usize
                        ));
                    }
                }

                if _plugin_context.get_host().get_name().as_deref().ok() == Some("com.vegascreativesoftware.vegas") {
                    out_rect = None;
                }

                let input_rotation = instance_data.params.get_f64_at_time(Params::InputRotation, TimeType::Frame(time)).ok().map(|x| x as f32);

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
                            instance_data.plugin.disable_opencl();
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
                            instance_data.plugin.disable_opencl();
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
                        let mut src_size = src_size;
                        let mut out_size = out_size;
                        src_size.2 = src_size.0 * 4 * (source_image.get_pixel_depth()?.bits() / 8);
                        out_size.2 = out_size.0 * 4 * (output_image.get_pixel_depth()?.bits() / 8);

                        log::info!("OpenGL in: {texture}, out: {out_texture} src_size: {src_size:?}, out_size: {out_size:?}, in_rect: {src_rect:?}, out_rect: {out_rect:?}");
                        Some((
                            BufferSource::OpenGL { texture: texture, context: std::ptr::null_mut() },
                            BufferSource::OpenGL { texture: out_texture, context: std::ptr::null_mut() },
                            true
                        ))
                    } else {
                        log::info!("CPU: src_size: {src_size:?} | {src_stride}, out_size: {out_size:?} | {out_stride}");
                        use std::slice::from_raw_parts_mut;
                        let src_buf = unsafe { match source_image.get_pixel_depth()? {
                            BitDepth::None  => { return FAILED; }
                            BitDepth::Byte  => { let b = source_image.get_descriptor::<RGBAColourB>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Short => { let b = source_image.get_descriptor::<RGBAColourS>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Half  => { let b = source_image.get_descriptor::<RGBAColourS>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Float => { let b = source_image.get_descriptor::<RGBAColourF>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) }
                        } };
                        let dst_buf = unsafe { match output_image.get_pixel_depth()? {
                            BitDepth::None  => { return FAILED; }
                            BitDepth::Byte  => { let b = output_image.get_descriptor::<RGBAColourB>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Short => { let b = output_image.get_descriptor::<RGBAColourS>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Half  => { let b = output_image.get_descriptor::<RGBAColourS>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) },
                            BitDepth::Float => { let b = output_image.get_descriptor::<RGBAColourF>()?; let mut b = b.data(); from_raw_parts_mut(b.ptr_mut(0), b.bytes()) }
                        } };
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

                    let processed = match output_image.get_pixel_depth()? {
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
                    source_clip,
                    output_clip,
                    supports_output_size: true,
                    is_fusion_page: false,
                    file_path: None,
                    video_path: None,
                    resolve_trim: None,
                    trim_query_auto: false,
                    last_trim_query: None,
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
                    current_file_info:         Arc::new(Mutex::new(None)),
                    current_file_info_pending: Arc::new(AtomicBool::new(false)),
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
                        instance_data.video_path = Some(path.clone());
                        if !instance_data.supports_output_size && CurrentFileInfo::is_available() {
                            // Query the timeline item's trim from Resolve's scripting API
                            // (async; the result is consumed on the next render). Resolve
                            // recreates the effect instance when the clip is re-trimmed, so
                            // this picks up trim changes as they happen.
                            instance_data.trim_query_auto = true;
                            instance_data.last_trim_query = Some(std::time::Instant::now());
                            CurrentFileInfo::query(instance_data.current_file_info.clone(), instance_data.current_file_info_pending.clone(), true);
                        }
                    }
                }

                effect.set_instance_data(instance_data)?;

                OK
            }
            InstanceChanged(ref mut effect, ref mut in_args) => {
                let instance_data: &mut InstanceData = effect.get_instance_data()?;
                if in_args.get_name()? == "LoadCurrent" {
                    CurrentFileInfo::query(instance_data.current_file_info.clone(), instance_data.current_file_info_pending.clone(), false);
                }
                if in_args.get_name()? == "Source" || in_args.get_name()? == "Output" || in_args.get_name()? == "ResolveUseAlphaForTrackCompositing" {
                    log::info!("InstanceChanged {:?} {:?}", in_args.get_name()?, in_args.get_change_reason()?);
                    return OK;
                }

                if let Ok(param) = std::str::FromStr::from_str(in_args.get_name()?.as_str()) {
                    if param == Params::OutputSizeToTimeline {
                        let rect = instance_data.source_clip.get_region_of_definition(0.0)?;
                        instance_data.plugin.timeline_size = ((rect.x2 - rect.x1) as usize, (rect.y2 - rect.y1) as usize);
                    }

                    instance_data.plugin.param_changed(&mut instance_data.params, &self.gyroflow_plugin.manager_cache, param, in_args.get_change_reason()? == Change::UserEdited).map_err(|e| {
                        log::error!("param_changed error: {e:?}");
                        Error::InvalidAction
                    })?;
                } else {
                    log::error!("Unknown param name: {:?}", in_args.get_name()?);
                }

                OK
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
                if instance_data.plugin.original_output_size != (0, 0) && !instance_data.params.get_bool_at_time(Params::DontDrawOutside, TimeType::Frame(time)).unwrap() { // TODO: unwrap
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
                        }
                        ParameterType::Button { id, label, hint } => {
                            if id == "CreateCamera" { return OK; }
                            if id == "LoadCurrent" && !CurrentFileInfo::is_available() {
                                return OK;
                            }
                            let mut param = param_set.param_define_button(id)?;
                            let _ = param.set_script_name(id);
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                        }
                        ParameterType::TextBox { id, label, hint } => {
                            let mut param = param_set.param_define_string(id)?;
                            let _ = param.set_script_name(id);
                            param.set_string_type(ParamStringType::SingleLine)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            if let Some(group) = group { param.set_parent(group)?; }
                        }
                        ParameterType::Text { id, label, hint } => {
                            let mut param = param_set.param_define_string(id)?;
                            param.set_string_type(ParamStringType::SingleLine)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            //param.set_enabled(false)?;
                            if let Some(group) = group { param.set_parent(group)?; }
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
                        }
                        ParameterType::Checkbox { id, label, hint, default } => {
                            if id == "StabilizationSpeedRamp" { return OK; }
                            let mut param = param_set.param_define_boolean(id)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            param.set_default(default)?;
                            let _ = param.set_script_name(id);
                            if let Some(group) = group { param.set_parent(group)?; }
                        }
                        ParameterType::Select { id, label, hint, options, default } => {
                            let mut param = param_set.param_define_choice(id)?;
                            param.set_label(label)?;
                            param.set_hint(hint)?;
                            param.set_default(options.iter().position(|x| *x == default).unwrap_or(0) as i32)?;
                            param.set_choices(&options)?;
                            let _ = param.set_script_name(id);
                            if let Some(group) = group { param.set_parent(group)?; }
                        }
                        ParameterType::Group { id, label, parameters, opened } => {
                            let mut param = param_set.param_define_group(id)?;
                            param.set_label(label)?;
                            param.set_group_open(opened)?;
                            if let Some(group) = group { param.set_parent(group)?; }

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

                param_set
                    .param_define_page("Main")?
                    .set_children(&[
                        "ProjectGroup",
                        "AdjustGroup",
                        "KeyframesGroup",
                        "ToggleOverview", "DontDrawOutside", "IncludeProjectData"
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
                if !supports_opencl && !supports_opengl {
                    unsafe { std::env::set_var("NO_OPENCL", "1") };
                }
                if _plugin_context.get_host().get_name().as_deref().ok() == Some("com.vegascreativesoftware.vegas") {
                    unsafe { std::env::set_var("NO_OPENCL", "1") };
                }

                let mut effect_properties: EffectDescriptor = effect.properties()?;
                effect_properties.set_grouping("Warp")?;

                effect_properties.set_label("Gyroflow")?;
                effect_properties.set_short_label("Gyroflow")?;
                effect_properties.set_long_label("Gyroflow")?;

                effect_properties.set_supported_pixel_depths(&[BitDepth::Byte, BitDepth::Short, BitDepth::Float])?;
                effect_properties.set_supported_contexts(&[ImageEffectContext::Filter])?;
                effect_properties.set_supports_tiles(false)?;

                effect_properties.set_single_instance(false)?;
                effect_properties.set_host_frame_threading(false)?;
                effect_properties.set_render_thread_safety(ImageEffectRender::FullySafe)?;
                effect_properties.set_supports_multi_resolution(true)?;
                effect_properties.set_temporal_clip_access(true)?;

                if supports_opengl && !supports_opencl && !supports_cuda && !supports_metal {
                    // We'll initialize the devices in OpenGLContextAttached
                    let _ = effect_properties.set_opengl_render_supported("true");
                    return OK;
                }

                let opencl_devices = gyroflow_plugin_base::opencl::OclWrapper::list_devices();
                let wgpu_devices = std::thread::spawn(|| gyroflow_plugin_base::wgpu::WgpuWrapper::list_devices()).join().unwrap();
                if !opencl_devices.is_empty() {
                    let _ = effect_properties.set_opencl_render_supported("true");
                    let _ = effect_properties.set_opengl_render_supported("true");
                }

                let _has_metal  = wgpu_devices.iter().any(|x| x.contains("(Metal)"));
                let _has_vulkan = wgpu_devices.iter().any(|x| x.contains("(Vulkan)"));
                let _has_dx12   = wgpu_devices.iter().any(|x| x.contains("(Dx12)"));

                #[cfg(target_os = "macos")]
                if !wgpu_devices.iter().any(|x| x.to_ascii_lowercase().contains("apple m")) {
                    unsafe {
                        std::env::set_var("NO_METAL", "1");
                        std::env::set_var("NO_WGPU", "1");
                    }
                }

                #[cfg(any(target_os = "macos", target_os = "ios"))]
                if _has_metal && std::env::var("NO_METAL").unwrap_or_default().is_empty() { let _ = effect_properties.set_metal_render_supported("true"); }
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                if _has_vulkan || _has_dx12 { let _ = effect_properties.set_cuda_render_supported("true"); }

                OK
            }

            Load => {
				self.gyroflow_plugin.initialize_log("openfx");
                OK
            },

            _ => REPLY_DEFAULT,
        }
    }
}
