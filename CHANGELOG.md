# Changelog

All notable changes to this project will be documented in this file.
Format based on Keep a Changelog (https://keepachangelog.com/en/1.0.0/).

## [0.1.0] - 2026-05-26

### Added
- Project scaffolding: `Cargo.toml`, two-binary layout (`mcub-bridge-rust`, `mcub-watcher-rust`)
- Module tree mirroring `mcub-c/src/{core,modules}`
- `McubError` enum + `Result<T>` alias (`thiserror`-based)
- Version constant (`src/version.rs`, `0.1.0`)
- README with 5-badge header, layout, mcub-c diff table
- Dependencies declared: `serde`/`serde_json`, `nix`, `udev`, `mpd`, `glob`, `libc`, `thiserror`
- Release profile: LTO, single codegen-unit, strip, panic=abort
