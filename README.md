# MCUB-Rust — MPD CAVA UART Bridge

[![Version](https://img.shields.io/badge/version-0.2.0-blue.svg)](https://github.com/thestreamdigger/mcub-rust)
[![License](https://img.shields.io/badge/license-GPL--3.0-green.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Platform](https://img.shields.io/badge/platform-Raspberry%20Pi-red.svg)](https://www.raspberrypi.org/)
[![Language](https://img.shields.io/badge/language-Rust-yellow.svg)](https://www.rust-lang.org/)
[![MPD](https://img.shields.io/badge/MPD-compatible-lightgrey.svg)](https://www.musicpd.org/)

Rust port of [mcub-c](https://github.com/thestreamdigger/mcub-c). Same protocol, same config format, same systemd layout. Two binaries.

Study-grade port: structure mirrors mcub-c file-by-file so behavior is comparable side-by-side, but internals use Rust idioms (Result/?, Drop, Mutex guards, channels, serde, enum protocol) where the language pays off.

## Quick Start

```bash
git clone https://github.com/thestreamdigger/mcub-rust.git
cd mcub-rust
./install.sh
```

## Build (manual)

```bash
sudo apt install build-essential pkg-config libudev-dev libmpdclient-dev
cargo build --release
sudo install -m 755 target/release/mcub-bridge-rust /usr/local/bin/
sudo install -m 755 target/release/mcub-watcher-rust /usr/local/bin/
```

## Usage

### Watcher (recommended)

```bash
mcub-watcher-rust                    # run daemon (auto-detect devices)
mcub-watcher-rust --status           # show device info
mcub-watcher-rust --cleanup          # stop all bridges
mcub-watcher-rust --no-wait-mpd      # skip MPD wait at boot
mcub-watcher-rust --version          # print version
```

### Bridge (manual)

```bash
mcub-bridge-rust <device> <bridge_type> [options]
```

Bridge types: `display`, `mpd`, `cava`, `hybrid`, `sysinfo`.

## Layout

```
src/
  core/       config_manager, logger, device_identifier, serial_comm,
              serial_queue, reconnection, cava_manager, signal_handler,
              action_runner
  modules/    cava_bridge, display_bridge, hybrid_bridge,
              mpd_bridge, sysinfo_bridge
  bin/        bridge.rs, watcher.rs
  error.rs    McubError + Result<T>
  version.rs  VERSION constant
shared/       settings.json (same format as mcub-c)
services/     systemd units
config/       sudoers.d
```

One-to-one with `mcub-c/src/{core,modules,watcher}` plus a Rust-flavored top-level (`lib.rs`, `error.rs`).

## Compared to mcub-c

External behavior identical (same MCUB protocol v2.3.0, same `settings.json`, same systemd unit name). Internal differences:

| mcub-c | mcub-rust |
|---|---|
| `int rc; if (rc != 0) return -1;` | `Result<T, McubError>` + `?` |
| `mcub_X_destroy()` manual | `impl Drop for X` (RAII) |
| `pthread_mutex_t` + lock/unlock | `Mutex<T>` guards |
| `pthread_cond_t` + linked list (`serial_queue`) | `mpsc::channel` |
| `cJSON_Parse` + node walk | `serde_json` + `#[derive(Deserialize)]` |
| `strcmp("play")` dispatch | `enum Command { Play, Stop, Exec{name}, … }` + `match` |
| Nullable pointers | `Option<T>` |
| Thread safety by convention | `Send`/`Sync` enforced by compiler |

Threading model unchanged: `std::thread::spawn` mirrors `pthread_create` (no async runtime).

## License

GPL-3.0-or-later.
