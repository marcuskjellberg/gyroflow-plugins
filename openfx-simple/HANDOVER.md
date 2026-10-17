# Gyroflow Simple (OpenFX) handover

## Scope

- Resolve Studio on macOS, initially GoPro MISSION 1 PRO Linear with a 4000x3000 (4:3) source at any frame rate and EIS setting. The bundled calibration in `common/res/lens_profiles/` was measured at 23.98 fps without EIS; EIS-on footage is accepted but its geometric accuracy has not been validated. Wide and 16:9 source modes have no bundled calibration and pass through.
- Bundle: `GyroflowSimple.ofx.bundle`; identifier: `xyz.gyroflow.simple.ofx`. The existing `openfx/` plugin keeps its own project workflow.
- Simple loads source-video metadata and embedded gyro directly through the pinned Gyroflow core. It does not run fuscript, launch the Gyroflow app, import `.gyroflow` projects or presets, or consult the installed lens-profile database. Its cache entries are separate from the legacy plugin's.
- Unsupported or unsynchronized clips show a pass-through reason. `IsIdentity` decides pass-through before rendering (sized by the last render's output, or the source clip's RoD before the first), because Simple declares Metal, OpenCL and CUDA rendering and GPU buffers can't be copied in Render. Render only copies the source when the host hands over CPU buffers. Check this behavior in Resolve before release.

## Panel

| Control | Default | Behavior |
|---|---|---|
| Stabilization | 85 | Core smoothing; the crop solver zooms as needed to keep every rendered pixel inside the source |
| Horizon level | 0.9 | Core horizon lock |
| Flip 180 degrees | off | Manual video rotation through a core keyframe; no automatic inversion detection |
| Zoom response (s) | 4 | Time the crop takes to zoom by 2x, 0.2 to 8 seconds; longer values are gentler but zoom in earlier around shakes |
| Reframe X / Y | 0 | Offset the frame center by up to 2% of source size per axis; the crop solver adds the extra crop required |
| View | Normal | Overview zooms out and draws the per-frame crop bound in rendered pixels (also exported) |

The old Crop priority remains defined but hidden for saved-project compatibility; it no longer limits zoom. Parameter changes trigger a new solve in Render, so no separate recalculate button is needed.

### Crop solver (`src/crop.rs`)

The crop is a constrained optimization, solved by the plugin instead of the core's adaptive zoom:

1. Per frame, the bound is the largest rectangle of the timeline's aspect, centred on the reframe offset, inside the stabilized outline of the 4000x3000 source. The outline is inset by the interpolation kernel's reach at the host buffer's scale: Lanczos4 reads 4 buffer pixels past a sample, about 11 source pixels in Resolve's 1440x1080 pillarbox. The rectangle-to-outline distance is computed exactly per outline segment.
2. Over time, the zoom is the largest curve under those bounds that changes by at most 2x per response time: `ln f(t) = min_s (ln F(s) + r|t - s|)`. A moving minimum and moving average over response/8 then ease its corners without exceeding it.
3. Every frame of the curve is checked with the renderer's own map (`Stabilization::undistort_coord`) at 16-pixel spacing along the frame edge and on an interior grid. A failing frame's bound is bisected and the curve solved again. The rectangles are nested, so a frame that passes also holds at any smaller zoom.

The solve covers the timeline item's frames, derived from Resolve's source frame and the effect's frame range, and widens if a frame outside them is rendered; without a source frame it covers the whole clip. It is keyed on the settings, the frame range and the margin, so a host buffer of another size gets its own solve. The previous 1% FOV inset and the core's 3 px margin were not a guarantee: with Lanczos4 on a 1080p timeline, kernel taps reached past the source area.

The source aspect and timeline aspect are distinct: a supported 4:3 source can be stabilized on a 16:9 Resolve timeline, though the host may supply only a cropped frame. Overview only shows the source area supplied by the host. On Resolve Edit/Color, buffers are timeline-sized and may already be cropped to the timeline aspect; the plugin cannot reveal missing 4:3 or offscreen source pixels. The outline is a rendered diagnostic, not a separate UI overlay.

## Verification

From `gyroflow-plugins/`:

```bash
CARGO_TARGET_DIR=target cargo test --release --manifest-path common/Cargo.toml
CARGO_TARGET_DIR=target cargo test --release --manifest-path openfx-simple/Cargo.toml
CARGO_TARGET_DIR=target cargo test --release --manifest-path openfx/Cargo.toml
CARGO_TARGET_DIR=target cargo fmt --all --check
just -f openfx-simple/Justfile dev
```

The ignored `real_clip_loads_without_project_or_external_lens` test needs `GF_TEST_VIDEO=/path/to/clip.MP4`; no test media is checked in. It runs the plugin's settings and crop solve the way the OFX instance does (inverted framebuffer, Lanczos4) at five settings, asserts that every frame passes the render check, and renders the six tightest frames plus an even spread into float buffers. In those buffers the 4:3 area is white inside opaque black bars, and the renderer's background is set to -1000 so any sample past the source area shows; 8-bit output would hide it, since the core truncates Lanczos sums of 254.99 to 254. Set `GF_TEST_FULL_RES=1` for Resolve's 1920x1080 buffers and `GF_TEST_ALL_FRAMES=1` to render every frame. In the October 2026 sample folder, all 62 calibrated recordings passed at 1920x1080 (310 solves). The outline bound never had to be lowered by the render check, the closest sample sat 0.04-0.05 px inside the kernel margin, peak zoom ran from 103% to 613%, and solves took up to 68 ms for 5242 frames. The three 16:9 recordings correctly passed through. GX010647 also passed with every frame rendered at all five settings. These checks use the CPU renderer with synthetic pixels; they do not cover Resolve's GPU paths or prove which source area Resolve supplies. Before shipping, test upright and inverted clips at different frame rates and EIS settings, and a Wide/unknown/no-gyro clip in Resolve. Verify calibration accuracy on EIS-on footage, CPU pass-through and `IsIdentity` on unsupported footage, and that Normal/Overview agree between preview and export at multiple source timestamps. Test both Edit/Color and Fusion where applicable. No release-ready claim is warranted until these host and real-media checks pass.

Do not run the `just` install target without the user's approval; it modifies `/Library/OFX/Plugins`. The dev bundle is created under `target/gyroflow-ofx-simple-dev/`.

## Logs

- Simple: `~/Library/Application Support/Gyroflow/gyroflow-openfx-simple.log` (truncated on Resolve startup).
- Legacy: `~/Library/Application Support/Gyroflow/gyroflow-openfx.log`.
