# Development notes — Resolve trim & aspect features (fork)

This fork (`marcuskjellberg/gyroflow-plugins`, branch `feature/resolve-trim-and-aspect`)
makes the OpenFX plugin behave natively in DaVinci Resolve on the Edit/Color pages:

1. **Source trim is respected automatically** — a clip trimmed on the timeline is
   stabilized with correct gyro timestamps AND the adaptive zoom is solved over the
   visible sub-clip only (like trimming in the Gyroflow app). No manually-created
   `.gyroflow` project needed.
2. **Aspect ratio: fill, not stretch** — when source and timeline aspect ratios differ
   (e.g. 4:3 GoPro in a 16:9 timeline), the output is a timeline-aspect **crop** of the
   source (no stretching, no letterbox bars), and adaptive zoom uses the cropped-away
   area as stabilization headroom.

All findings below were established empirically against **DaVinci Resolve Studio 21.0.3
(macOS, Apple Silicon)** with logging builds; they are the ground truth this design rests on.

---

## How Resolve presents clips to OpenFX filters (Edit/Color pages)

These findings cost several debugging rounds — trust them before re-deriving:

- **Render time is in SOURCE-frame space.** `kOfxPropTime` at render equals the
  source-media frame index; Resolve's BMD extension `kOfxImageEffectPropSrcFrame`
  (`in_args.get_src_frame()`, Resolve 20+) returns the same value on these pages.
  Timestamps derived from time are therefore automatically trim-correct.
- **The clip's `kOfxImageEffectPropFrameRange` always covers the FULL source media**,
  regardless of trim. Resolve simply only *requests* renders for visible frames. There is
  **no per-render signal for the trim boundaries** — don't try to detect trim from render
  args; it's mathematically impossible (offset formulas always yield 0).
- **`GetRegionOfDefinition` is never called** for Edit-page filters (verified: zero calls
  with logging across sessions). Output size negotiation is impossible; **buffers are
  always timeline-sized** (scaled by proxy/playback resolution: e.g. 3840×2160 full,
  1920×1080 half). Declaring a different RoD achieves nothing.
- **Resolve recreates effect instances on timeline edits** (re-trim, re-add). This is the
  event hook used for re-querying trim.
- Resolve keeps **multiple internal instances per clip** alive (playback, filmstrip, …)
  whose reported ranges can differ by ±1–2 frames — any state that reacts to per-render
  values needs a small tolerance (we use 3 frames) or it thrashes.
- `kOfxImageEffectPropSrcFilePath` (BMD extension) provides the source file path in
  `CreateInstance` — used for auto-loading and for matching scripting-query results.
- `kOfxImageEffectPropResolvePage` reports "Edit"/"Color"/"Fusion" — the Edit/Color
  behaviors are gated on it (`supports_output_size = false`, `fill_output_to_host_aspect`).

## Trim: how it works now

Since render args can't reveal trim, it's queried from **Resolve's scripting API** via the
bundled `fuscript` binary (Studio only — same mechanism as the existing "Load for current
file" button). See [openfx/src/fuscript.rs](openfx/src/fuscript.rs):

- Lua one-liner grabs `TimelineItem:GetLeftOffset()` (frames into the source media) and
  `GetDuration()` (item length in frames), alongside the existing clip properties.
  Verified to reproduce the app's `trim_ranges_ms` byte-for-byte on a test clip.
- **Auto-query** fires silently (no dialog, no FlipX re-render hack) in `CreateInstance`
  on Edit/Color pages. Because Resolve recreates instances on timeline edits, re-trims
  are picked up automatically.
- **`GetCurrentVideoItem()` returns nil for a few seconds right after a timeline edit**
  (Lua error: `attempt to index global 'i' (a nil value)`). The query retries up to
  8 × 700 ms. Note: `std::process::Command::args` APPENDS — build a fresh `Command`
  per retry or arguments duplicate.
- Results are only consumed if the queried item's file path matches this instance's
  video ([gyroflow.rs](openfx/src/gyroflow.rs) `check_pending_file_info`; the scripting
  API returns whatever is under the playhead). Auto queries never touch `ProjectPath`
  (manual "Load for current file" keeps its original behavior).
- **Validation + self-healing**: if a rendered source frame falls outside the known trim
  (clip was extended, or the query hit the wrong item), the trim is discarded and
  re-queried (rate-limited to every 3 s).
- The range is applied via `StabilizationManager::set_trim_ranges(vec![(start, end)])`
  (fractions 0..1 of `duration_ms`) + `invalidate_blocking_smoothing()` — the same
  pattern as the After Effects plugin ([adobe/src/lib.rs](adobe/src/lib.rs), search
  `set_trim_ranges`). Replace, don't union: apply only when the range moved more than
  3 frames (jitter tolerance). A range covering ~the whole clip is snapped to no-trim.
- **The zoom-relax mechanism is proven at the core level** by
  [common/tests/trim_zoom.rs](common/tests/trim_zoom.rs) (ignored; needs a real clip):

  ```bash
  GF_TEST_VIDEO=/path/to/GX010284.MP4 cargo test --release --test trim_zoom -- --ignored --nocapture
  ```

  On the reference clip, trimming the last 10% relaxes min FOV 0.63 → 0.85.

## Timestamps: `openfx/src/timestamp.rs`

The Render-action time math is a pure, unit-tested function (`compute_timestamp`):

- With `src_frame` (Resolve 20+): it is the time base; the legacy `speed_stretch`
  heuristic is disabled (it computes garbage ratios for trimmed clips — e.g. ~90× for a
  20 s section of a 30 min file), and the internal speed-ramp remap shifts the source
  frame directly instead of round-tripping through timeline time.
- Without `src_frame`: bit-identical to the historical behavior (pinned by tests against
  a transcribed reference implementation, including the ±3% `speed_stretch` whitelist).

## Aspect ratio / output sizing

- On Edit/Color, `GyroflowPluginBaseInstance::fill_output_to_host_aspect` is set: the
  stabilizer output becomes a **host-frame-aspect crop of the source** via
  `StabilizationManager::set_output_size(buffer_w, buffer_h)`. Core semantics (verified
  at rev 704744e): it keeps the REQUESTED aspect ratio and scales it to the largest size
  fitting inside the source, rotation-aware — so a 16:9 request on a 4000×3000 source
  yields 4000×2250 regardless of proxy buffer scale (proxy sizes can't get baked in).
- The render path also has a safety net: if the stabilizer output aspect ever mismatches
  the output buffer, it draws centered at the correct aspect instead of stretching
  (and the reduced-render-scale block composes with that rect instead of replacing it).
- `OutputWidth`/`OutputHeight` params are disabled and ignored on Edit/Color (they could
  contain stale/proxy values from old sessions). Fusion page behavior is unchanged.

## Manager cache semantics (common/src/lib.rs `stab_manager`)

- Cache key = `{project_path}{disable_stretch}{instance_id}`. Pieces of a cut clip share
  the key (and thus one `StabilizationManager`) until the user edits a parameter
  (`ever_changed` → new `instance_id`).
- `invalidate_blocking_smoothing()` only sets flags; the recompute happens inside the
  next `process_pixels` (visible in logs as `Max zoom iteration …` right after
  `Setting trim range …`).

## Debugging

- **Log file**: `~/Library/Application Support/Gyroflow/gyroflow-openfx.log`.
  - It is **truncated on every Resolve launch** — capture it before relaunching.
  - It starts with binary junk; use `grep -a`.
  - Timestamps are ~2 h behind local time (UTC-ish).
  - `simplelog` is configured to ignore targets *starting with* `ofx` (the ofx-rs crate),
    not `gyroflow_ofx` — plugin logs pass through.
- Useful greps: `Setting trim range`, `Timeline item trim`, `new stab manager`,
  `metal: src_size`, `fuscript`.
- "Stabilization overview" checkbox zooms the view OUT — adaptive-zoom changes are
  invisible while it's on. Untick before judging zoom.
- Resolve's Smart Render Cache can serve stale frames after a zoom re-solve; toggle any
  parameter to force a refresh.

## Build & install (macOS dev loop)

```bash
just ofx dev     # native-arch release build + ad-hoc-signed bundle (~25 s incremental)
sudo rm -rf /Library/OFX/Plugins/Gyroflow.ofx.bundle
sudo cp -R target/gyroflow-ofx-dev/Gyroflow.ofx.bundle /Library/OFX/Plugins/
# restart Resolve; remove + re-add the effect on test clips after sizing changes
```

- Toolchain: stable Rust (rustup) + `just` + Xcode CLT. First build is heavy
  (gyroflow-core: wgpu/OpenCL/telemetry-parser). `cargo test --release` in `openfx/` and
  `common/` runs the unit tests (15 total + the ignored real-file test).
- Release builds go through `openfx/Justfile` `deploy` (universal binary, real signing,
  notarization — CI only).
- Key pinned deps: `gyroflow-core` = gyroflow/gyroflow @ `704744e`;
  `ofx` = AdrianEddy/ofx-rs @ `0b15219` (source of the BMD property extensions —
  see `ofx/src/property.rs` there for what Resolve exposes).

## Known limitations / future work

- Trim query requires **Resolve Studio** (fuscript external scripting; set
  "Preferences → General → External scripting using" to Local). Non-Studio: trim of the
  visible frames still renders correctly (timestamps are source-based), but adaptive zoom
  solves over the full recording.
- The scripting API returns the item under the *playhead*: with several differently-
  trimmed copies of the same media, a query can briefly attach the wrong trim; the
  rendered-frame validation discards and re-queries. Residual ambiguity is bounded.
- Retimed/speed-ramped clips: `GetDuration()` is timeline frames, not source frames —
  trim range will be slightly off under retimes (timestamps remain correct via src_frame).
- Fusion page is untouched by all of this (it has the source-resolution native pipeline;
  use it for workflows that need explicit output size control).
- Upstreaming: this branch intentionally changes default behavior on Edit/Color
  (fill instead of stretch). An upstream PR probably wants it behind a checkbox default.
