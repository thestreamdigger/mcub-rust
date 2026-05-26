# mcub-c vs mcub-rust

Side-by-side comparison of the C and Rust implementations of the same
project. Same protocol (MCUB v2.2.0), same `settings.json` schema.
Binaries and systemd unit suffixed per impl: `mcub-{bridge,watcher}-c` +
`mcub-watcher-c.service` vs `mcub-{bridge,watcher}-rust` +
`mcub-watcher-rust.service`.

## Method

- Build environment: WSL Debian 13, x86_64-linux-gnu.
- C: gcc 14.2.0, cmake 3.31.6 + ninja 1.12.1, `-O3` via `CMAKE_BUILD_TYPE=Release`.
- Rust: rustc 1.95.0 stable, `cargo build --release` (LTO, strip, single
  codegen-unit, panic=abort — see `Cargo.toml [profile.release]`).
- Both binaries built fresh on 2026-05-26.

Caveat: cross-arch numbers (aarch64 / armv7) and runtime behavior (RAM /
CPU / latency) require deployment to a Pi and are not part of this
document. See "Runtime (TODO)" below.

## Lines of code

Counted with `wc -l`. C numbers include both `.c` (implementation) and
`.h` (interfaces); Rust numbers are everything under `src/`.

| Layer | mcub-c | mcub-rust | Δ |
|---|---:|---:|---:|
| Core (config, logger, serial, etc) | 2 297 | 2 019 | **−12 %** |
| Modules (5 bridges + dispatcher) | 1 822 | 1 525 | **−16 %** |
| Watcher | 826 | 642 | **−22 %** |
| Misc (version, lib, error, mod) | 63 | 155 | +146 % (boilerplate) |
| **Total** | **5 008** | **4 341** | **−13 %** |

Per-file biggest drops:

| Module | C | Rust | Δ | Why |
|---|---:|---:|---:|---|
| `serial_queue` | 385 | 290 | −25 % | `BinaryHeap` + `Mutex<T>` + `Drop` |
| `serial_comm` | 302 | 232 | −23 % | `OwnedFd`, `Drop`, fewer `_destroy()` fns |
| `watcher` | 826 | 642 | −22 % | `HashMap<String,Cooldown>`, `Option<Active>`, match |
| `sysinfo_bridge` | 344 | 269 | −22 % | serde + `Option<T>` skips conditional `cJSON_AddNumber` |
| `logger` | 322 | 267 | −17 % | `LogLevel` enum + guard-based mutex |
| `mpd_bridge` | 594 | 485 | −18 % | serde envelope + `match` action dispatch |

Where Rust spent more lines:

| Module | C | Rust | Δ | Why |
|---|---:|---:|---:|---|
| `device_identifier` | 259 | 275 | +6 % | `OwnedFd`/`BorrowedFd` lifetime plumbing for `nix` |
| `hybrid_bridge` | 645 | 635 | −2 % | Mostly a wash: 4 threads with `Arc<Inner>` + `Mutex` cost setup but the bridge dispatcher (`SUPPORTED_COMMANDS` + `match`) saves it back |
| top-level boilerplate | 0 | 155 | n/a | `error.rs`, `lib.rs`, `version.rs`, `mod.rs` files have no C equivalent (header-as-interface is implicit) |

## Binary size (stripped, dynamically linked)

Both Linux x86_64 release builds, post-`strip`:

| Binary | mcub-c | mcub-rust | Ratio |
|---|---:|---:|---:|
| `mcub-bridge` | 73 KB | 928 KB | 12.7× |
| `mcub-watcher` | 52 KB | 747 KB | 14.4× |
| **Combined** | **125 KB** | **1 675 KB** | **13.4×** |

Rust binaries are larger because they statically link cjson (serde_json),
mpdclient (mpd crate), udev, glob, plus the Rust standard library and
panic infrastructure. C dynamically links libcjson, libmpdclient,
libudev — the binary itself is small but you also need the deps
installed system-wide.

### Dynamic linking footprint

```
mcub-c bridge:   libcjson.so.1, libmpdclient.so.2, libc.so.6  (3 deps)
mcub-rust bridge: libgcc_s.so.1, libc.so.6                     (1 ext dep)
```

Combined runtime cost (binary + dyn libs the system needs to load):
- C: 125 KB binary + ~250 KB of system libs already loaded for moOde
  anyway → effective cost ~125 KB on disk and shared in RAM with other
  consumers.
- Rust: 1.7 MB binary, libgcc loaded by virtually every process →
  effective cost ~1.7 MB on disk, no shared lib savings.

On a 512 MB Pi 3 the binary size is irrelevant; on a Pico-class target
it would matter, but mcub is a Pi-host project where this doesn't.

## Threading model

Both ports use OS threads (1:1 with `pthread_create` ↔ `std::thread::spawn`).
No async runtime in either. Hybrid bridge spawns the same four worker threads:

| C (pthread) | Rust (std::thread) |
|---|---|
| `pthread_create` | `thread::Builder::new().name(…).spawn(…)` |
| `pthread_mutex_t` + manual lock/unlock | `Mutex<T>` returning RAII guard |
| `pthread_cond_t` + `pthread_cond_timedwait` | `Condvar::wait_timeout` |
| `pthread_tryjoin_np` | `JoinHandle::is_finished()` |
| `pthread_sigmask` | `nix::sys::signal::SigSet::thread_block` |
| Heap-allocated linked list (`serial_queue`) | `std::collections::BinaryHeap` |

## Idiomatic wins in Rust

Concrete examples from the port — places where Rust's type system
removed entire categories of code:

1. **Resource cleanup.** Every C `mcub_X_destroy(*x)` becomes
   `impl Drop for X`. Watcher's bridge child process, MPD client, CAVA
   subprocess, serial fd — all clean up automatically when the owning
   struct goes out of scope. Counted savings: 4 `_destroy` functions
   plus their explicit calls (≈ 60 lines).

2. **Optional fields in sysinfo JSON.** C sysinfo:
   ```c
   if (cpu >= 0) cJSON_AddNumberToObject(d, "cpu", cpu);
   if (temp >= 0) cJSON_AddNumberToObject(d, "temp", ...);
   /* ... repeated for 9 fields */
   ```
   Rust: `Option<i32>` with `#[serde(skip_serializing_if = "Option::is_none")]`
   on each field. The sentinel-`-1`-means-"unavailable" convention is
   expressed in the type. ~35 lines collapsed to ~10.

3. **Command dispatch.** C mpd_bridge: 11 `strcmp(action, "X") == 0`
   branches with `else if` chains. Rust: one `match` on `&str`. Same
   shape, exhaustively checked at compile time.

4. **Min-heap queue.** `serial_queue.c` hand-rolls a binary heap with
   `heap_push`/`heap_pop`/`heap_swap`/`heap_less` (~70 lines). Rust uses
   `BinaryHeap<QueueItem>` from std (0 lines of heap code, just an `impl
   Ord`). The custom `Ord` impl reverses priority for min-heap semantics
   and uses sequence as tie-breaker — same FIFO-within-priority
   guarantee.

5. **Send/Sync enforcement.** Hybrid bridge has 4 threads sharing one
   `Arc<Inner>` containing mutex-wrapped fields. The compiler verifies
   that everything sent across threads is `Send + Sync`. In C the
   equivalent is "we checked it carefully, hope the next refactor
   doesn't break it."

6. **No null pointers.** Every C return that uses `nullptr` for "not
   found" / "not connected" / "no response" becomes `Option<T>` in Rust.
   The `?` operator + `let Some(x) = … else { return … }` make the
   happy path linear. Lines saved per call site: 2-4.

## Runtime on Pi 5 (zukunft, aarch64, Debian 13.3)

Measured 2026-05-26 on a Pi 5 (BCM2712, 8 GB) running moOde, with a
known-good hybrid device (DFRobot keypad, binary mode, 16 bars)
connected to `/dev/ttyACM0`. Both versions built locally on the Pi via
their `install.sh`, then swapped at `/usr/local/bin/`, service
restarted, measured after 30 s steady state.

### Binary size (aarch64 stripped)

| Binary | mcub-c | mcub-rust | Ratio |
|---|---:|---:|---:|
| `mcub-bridge` | 67 KB | 836 KB | 12.5× |
| `mcub-watcher` | 67 KB | 708 KB | 10.6× |
| **Combined** | **134 KB** | **1 544 KB** | **11.5×** |

### RSS at idle (hybrid bridge, no MPD playback)

| Process | mcub-c | mcub-rust | Δ |
|---|---:|---:|---:|
| `mcub-watcher` (1 thread) | 2 144 KB | 2 480 KB | +16 % |
| `mcub-bridge hybrid` (5–6 threads) | 2 336 KB | 2 928 KB | +25 % |
| **Combined RSS** | **4 480 KB** | **5 408 KB** | **+21 %** |

### Threading

Both bridges spawn the same worker set:

| Workers | C | Rust |
|---|---:|---:|
| main | 1 | 1 |
| mpd_checker | 1 | 1 |
| cava_reader | 1 | 1 |
| command_processor | 1 | 1 |
| sysinfo_checker | 1 | 1 |
| serial_queue worker | 1 | 1 |
| **Total** | **6** | **5–6 (see note)** |

Note: Rust shows 5 threads when cava is still in "deferred" state
(loopback not yet ready) — the cava_reader thread is only spawned
after `cava_manager.start()` succeeds. C version spawns the same thread
but starts cava synchronously at init, so the count is 6 from the
start. Behavior is equivalent, timing of the count differs.

### CPU at idle (no active MPD playback, no key presses)

| Process | mcub-c | mcub-rust |
|---|---:|---:|
| `mcub-watcher` | 0.0 % | 0.0 % |
| `mcub-bridge hybrid` | 0.2 % | 0.1 % |

Both essentially zero — the work is event-driven (serial poll, MPD
status poll every 1 s).

### Findings

- **RSS overhead in Rust: ~1 MB extra combined** (~21 % more). Acceptable
  on any Pi with ≥ 512 MB RAM. The overhead is from the Rust standard
  library + statically linked deps; C dynamically links libcjson,
  libmpdclient, libudev which are loaded once system-wide.
- **Binary 11.5× larger on disk**, same reason. Pi storage is cheap.
- **CPU footprint identical** — both implementations sit at the same
  ~0 % idle and produce the same serial output rate. Performance
  parity confirmed for the idle case.
- **Boot-time serial framing identical**: both implementations see
  garbled JSON on the first identify attempt after USB enumeration
  (device boots while we're already reading), and both recover on the
  second `--retry` cycle via the same retry path.

### Still TODO (needs MPD playback)

- [ ] CPU% under active CAVA frames (30 FPS binary spectrum).
- [ ] End-to-end command latency (key press → MPD action → status
      update visible on device): median + p99.
- [ ] Spectrum frame drop rate at 30 FPS (cava bridge, binary mode).
- [ ] Long-duration stability (24 h+ uptime, RSS drift check).

Method:
```bash
pictrl --host zukunft.local x "mpc play && sleep 60 && ps -eo rss,pcpu,comm | grep mcub"
```

## What this exercise was for

Study of how much of C-idiomatic resource discipline and explicit state
machinery a strongly-typed language with ownership can absorb. Honest
result: about 13 % fewer total lines, mostly concentrated in the
boilerplate-heavy modules (resource cleanup, JSON envelopes, command
dispatch, fixed-size collection management). The watcher's HashMap+match
state machine and the queue's `BinaryHeap` are the single biggest
wins — both express "what the code does" instead of "how it does it."

Binary footprint is the obvious cost: 12-14× larger executables. For a
self-contained Pi service this is fine. For a memory-constrained
embedded target it would not be.
