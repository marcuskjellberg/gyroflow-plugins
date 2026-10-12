# Gyroflow Simple (OpenFX) — handover

## Where things are
- Branch: `feature/simple-plugin` (from `feature/resolve-trim-and-aspect`). Nothing committed yet.
- Existing plugin (`openfx/`) is unchanged in behavior. It runs on the newer Gyroflow core because `common/` is pinned to `79765f4`.
- New plugin: `openfx-simple/`. Bundle name `GyroflowSimple.ofx.bundle`, identifier `xyz.gyroflow.simple.ofx`, effect label "Gyroflow Simple".
- Camera table: `common/src/cameras.rs`. MISSION 1 PRO (Linear has our calibrated profile embedded from `common/res/lens_profiles/`; Wide has none). Osmo Nano is a placeholder.

## Spec (what the simple plugin does)
Goal: as much stabilization as possible without cropping below 1080p detail, with almost no settings.

Panel (everything else hidden, IDs kept so projects stay compatible):
| Control | Range | Default | Maps to |
|---|---|---|---|
| Stabilization | 0–100 | 85 | core smoothness 0.001–1.0 (Default algorithm) |
| Horizon level | 0–1 | 0.9 | horizon lock amount 0–100% |
| Flip 180° | on/off | off | hidden Rotation 0/180 |
| Reframe X / Y | ±100 | 0 | zoom-center offset; ±100 = frame touches the source edge at the zoom limit |
| Stabilization overview | on/off | off | shared param |

Fixed engine settings (`openfx-simple/src/simple.rs`, independent of presets saved by the Gyroflow app):
- Zoom limit = the zoom that keeps 1920×1080 of source detail visible: `min(long/1920, short/1080)` of the output crop, clamped to 115–400%. 4000×3000 GoPro in a 16:9 timeline: 4000×2250 crop → 208%.
- Max-zoom limiter runs 15 iterations (core default 5 often stopped above the limit). When a moment would need more zoom, that moment gets less smoothing instead.
- Dynamic zoom (4 s window, envelope follower): zooms in only around shaky moments.
- Applied through the plugin's keyframe cache (`SmoothingParamSmoothness`, `LockHorizonAmount`, `MaxZoom`) every render; a change triggers one blocking recompute.
- Status shows `OK · zoom up to N%`.

Trim (`SourceRanges` in `fuscript.rs`): one scripting query lists the source range of every timeline item using this file (no dependency on the playhead, which caused `attempt to index a nil value`). The rendered source frame picks the right cut. Re-queries only if the frame is in none of the ranges (5 s rate limit), never when Resolve reports no items. Results are shared between instances for 2 s, and all fuscript calls are serialized (concurrent calls are the suspected cause of a Resolve crash).

Verified offline on GX010636 (`real_clip_zoom_stays_within_limit`, ignored test): max zoom 100.9% at Stabilization 0, 171.9% at 50, 207.0% at 85/0.9, 206.6% at 100/1.0 — always under the 208.3% limit.

## History / fixes
- mimalloc is an optional feature of `common`; the simple plugin builds without it (two copies crashed Resolve).
- Trim queries use source frames (`GetSourceStartFrame`/`GetSourceEndFrame`), not timeline frames. The original plugin still has the timeline-frame bug.
- "Timeline fps mismatch!" only shows when Resolve gives no source frame.
- Core setters like `set_zooming_center_*` only do a non-blocking invalidation; the plugin must call `invalidate_blocking_*` or nothing changes until another setting does.
- Resolve sends `InstanceChanged("InstanceId")` synchronously inside CreateInstance, before the instance data exists. It's ignored; touching instance data there crashed Resolve.
- Flip with horizon lock on already-upright footage flickers ±45°. Not a target use case.
- Diagnostic log lines (keep until verified): `InstanceChanged`, `Flip 180`, `Reframe`, `Applying Settings`, `Zoom solved`, `Timeline items using`, `Timeline item for source frame`.

## Still open
1. Verified in Resolve (12 Oct): new panel works, trim is picked up per cut, zoom stays under the limit, no crashes. On a 16:9 timeline the output stays a 16:9 crop of the 4:3 source (chosen over pillarboxed 4:3; a 4:3 timeline keeps the full frame).
2. Stabilization overview: draw the timeline frame over the 4:3 source.
3. Standalone check: run on a machine/user without the Gyroflow app.
4. Upside-down auto-detection from GRAV (needs an inverted sample clip).
5. Wide lens profile and DJI Osmo Nano profile.
6. Resolve hands the plugin a timeline-resolution buffer (1920×1080 on a 1080p timeline), so source detail is already reduced before stabilization. Not fixable on the Edit page.

## Known issues
- "Path is empty" errors appear at instance start. The original plugin shows the same errors, so they may be normal startup.

## Build and install
```bash
cd ~/Developer/Gyroflow/gyroflow-plugins/openfx-simple && CARGO_TARGET_DIR=../target just dev
sudo rm -rf /Library/OFX/Plugins/GyroflowSimple.ofx.bundle && sudo cp -R ~/Developer/Gyroflow/gyroflow-plugins/target/gyroflow-ofx-simple-dev/GyroflowSimple.ofx.bundle /Library/OFX/Plugins/
```
Unit tests: `cd openfx-simple && CARGO_TARGET_DIR=../target cargo test --release` (and the same in `common`).
Real-clip check: `GF_TEST_VIDEO=/path/to/GX010636.MP4 CARGO_TARGET_DIR=../target cargo test --release real_clip -- --ignored --nocapture` in `openfx-simple`.

## Backups
- Original plugin: `~/Desktop/Gyroflow.ofx.bundle.backup`.

## Logs
- Simple plugin: `~/Library/Application Support/Gyroflow/gyroflow-openfx-simple.log` (truncated each Resolve launch).
- Original plugin: `~/Library/Application Support/Gyroflow/gyroflow-openfx.log`.
- Crash reports: Resolve writes its own to `~/Library/Application Support/Blackmagic Design/DaVinci Resolve/crash_archive.txt` (macOS `.ips` reports often don't appear).
