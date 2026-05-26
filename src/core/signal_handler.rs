use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

type CleanupFn = Box<dyn Fn() + Send + Sync>;
static CLEANUP: OnceLock<Mutex<Option<CleanupFn>>> = OnceLock::new();

extern "C" fn handler(_: libc::c_int) {
    SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
    if let Some(cell) = CLEANUP.get() {
        if let Ok(guard) = cell.lock() {
            if let Some(f) = guard.as_ref() {
                f();
            }
        }
    }
    unsafe { libc::_exit(0) };
}

pub fn setup<F>(cleanup: F)
where
    F: Fn() + Send + Sync + 'static,
{
    CLEANUP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("cleanup mutex poisoned")
        .replace(Box::new(cleanup));

    let action = SigAction::new(SigHandler::Handler(handler), SaFlags::empty(), SigSet::empty());
    unsafe {
        let _ = signal::sigaction(Signal::SIGINT, &action);
        let _ = signal::sigaction(Signal::SIGTERM, &action);
        let _ = signal::sigaction(Signal::SIGUSR1, &action);
    }
}

pub fn block_in_thread() {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGUSR1);
    let _ = set.thread_block();
}

pub fn received() -> bool {
    SIGNAL_RECEIVED.load(Ordering::SeqCst)
}
