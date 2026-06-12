# Changelog

All notable changes to this project will be documented in this file.
Format based on Keep a Changelog (https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Fixed
- Serial lock contention capped spectrum at ~37/s: `read_message` held the comm-state mutex for its full 50 ms readline while send paths needed the same lock to reach the queue. Handles now fetched under a short lock, I/O outside it. Invisible at 30 fps (cap > target), exposed by 60 fps. Verified on zukunft: 58.0/s, drops=0
- Binary cava read now blocks in `poll()` honoring the frame-interval timeout instead of caller-side 5 ms sleep-polling (same fix as mcub-c; 5 ms quantization cannot sustain 60 fps cadence)

### Changed
- MPD protocol usage modernized (parity with mcub-c, MPD 0.24.x): `toggle_pause` (deprecated no-arg `pause`) → explicit `pause(bool)`; `single`/`consume` parse now oneshot-aware (`v != "0"`; old `v == "1"` treated `oneshot` as off, inverting the toggle)
- mpd bridge no longer probes `status` every 10 ms loop iteration (~100 req/s); reconnects only when `mpd_connected` flag drops (`check_connection` removed)
- CAVA default framerate 30 → 60 (config default + shared/settings.json) — parity with mcub-c
- CAVA generated config: dropped `gravity` from `[smoothing]` (removed upstream in cava 0.10.x, was silently ignored)

### Removed
- Dead config knob `performance.queue_timeout_ms` (plumbed into `SerialQueue::new` as `_queue_timeout`, never read)

## [0.1.0] - 2026-05-26

### Added
- Project scaffolding: `Cargo.toml`, two-binary layout (`mcub-bridge-rust`, `mcub-watcher-rust`)
- Module tree mirroring `mcub-c/src/{core,modules}`
- `McubError` enum + `Result<T>` alias (`thiserror`-based)
- Version constant (`src/version.rs`, `0.1.0`)
- README with 5-badge header, layout, mcub-c diff table
- Dependencies declared: `serde`/`serde_json`, `nix`, `udev`, `mpd`, `glob`, `libc`, `thiserror`
- Release profile: LTO, single codegen-unit, strip, panic=abort
