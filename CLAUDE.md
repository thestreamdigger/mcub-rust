# CLAUDE.md — mcub-rust

## Purpose

Rust port of `mcub-c` (sibling directory). **Study-grade**: built to compare C and Rust idiomatically on the same problem, not to replace mcub-c.

## Port Rules

**External: 1:1 with mcub-c**
- Same file tree (`src/core/X.rs` ↔ `mcub-c/src/core/X.c`)
- Same public function names per module
- Same wire protocol (MCUB v2.2.0)
- Same `settings.json` format; systemd unit suffixed per impl (`mcub-watcher-c` vs `mcub-watcher-rust`)
- Same threading model: `std::thread::spawn` mirrors `pthread_create` (no tokio)

**Internal: idiomatic Rust where it pays off**
- Error returns → `Result<T, McubError>` + `?`
- `destroy()` functions → `impl Drop`
- `pthread_mutex_t` + lock/unlock → `Mutex<T>` guards
- `pthread_cond_t` + linked list (`serial_queue`) → `mpsc::channel`
- `cJSON` walking → `serde_json` + `#[derive(Deserialize)]`
- `strcmp` dispatch → `enum` + `match`
- Nullable pointers → `Option<T>`

**Not allowed**
- Refactoring architecture (no merging hybrid_bridge+mpd_bridge, no splitting watcher.c)
- Adding features mcub-c doesn't have
- async/tokio (breaks pthread parity)
- Generic abstractions over what C already does concretely

## Versioning

Own linear SemVer track. Starts at `0.1.0`. Independent from mcub-c's `2.2.0`. Wire protocol version (`MCUB_PROTOCOL_VERSION`) stays synced with mcub_common.

## Build & Deploy

```bash
# Local check
cargo check
cargo build --release

# Deploy to Pi (TBD: install.sh adapted from mcub-c)
pictrl deploy C:\dev\maintain\mcub\mcub-rust /home/pi/mcub-rust --build "cargo build --release"
```

## Comparativo (objetivo do projeto)

When both ports work, measure on the same Pi:
- Stripped binary size (mcub-c vs mcub-rust each binary)
- Resident RSS at idle and under load
- CPU% with active serial + CAVA + MPD polling
- Latency: serial event → bridge action

Results go in `docs/compare-c-vs-rust.md`.

## Key Learnings

(populate as port progresses — gotchas Rust-side that surprised us vs C)
