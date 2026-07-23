# gyroflow-plugins (fork)

Fork of gyroflow/gyroflow-plugins adding native DaVinci Resolve trim + aspect handling to
the OpenFX plugin. Branch: `feature/resolve-trim-and-aspect`.

**Read [DEVELOPMENT.md](DEVELOPMENT.md) before changing anything in `openfx/` or
`common/`** — it documents hard-won empirical facts about how Resolve drives OFX filters
(source-frame render times, no RoD negotiation, instance recreation on edits, fuscript
quirks) that the design depends on. Re-deriving them costs hours of build/test cycles in
Resolve.

Quick facts:
- Build dev bundle: `just ofx dev` → `target/gyroflow-ofx-dev/Gyroflow.ofx.bundle`,
  install to `/Library/OFX/Plugins` (needs sudo), restart Resolve.
- Tests: `cargo test --release` in `openfx/` and `common/` (CARGO_TARGET_DIR=../target).
- Plugin log: `~/Library/Application Support/Gyroflow/gyroflow-openfx.log`
  (truncated per Resolve launch; use `grep -a`).
- Crates: `openfx/` (Resolve plugin), `common/` (shared base, gyroflow-core bridge),
  `adobe/`, `frei0r/` (untouched by fork features; `common` struct changes must keep
  them compiling — `adobe` constructs `GyroflowPluginBaseInstance` literally).
