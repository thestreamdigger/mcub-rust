use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, OnceLock};

use nix::sys::wait::waitpid;
use nix::unistd::{fork, ForkResult};

use crate::core::config_manager::ActionEntry;
use crate::core::logger::Logger;
use crate::{log_error, log_info, log_ok, log_warning};

struct RunnerState {
    actions: HashMap<String, ActionEntry>,
    logger: Arc<Logger>,
}

static STATE: OnceLock<RunnerState> = OnceLock::new();

pub fn init(actions: HashMap<String, ActionEntry>, logger: Arc<Logger>) {
    // NOTE: mcub-c uses signal(SIGCHLD, SIG_IGN) for kernel auto-reap of action
    // grandchildren. That strategy breaks std::process::Command in Rust because
    // its internal waitpid returns ECHILD. We use double-fork instead: action
    // becomes orphan adopted by init, which reaps it. SIGCHLD stays at default,
    // so Command::output() and similar continue working in the bridge process.
    let first_init = STATE.set(RunnerState {
        actions,
        logger: Arc::clone(&logger),
    }).is_ok();
    if !first_init {
        return;
    }
    let state = STATE.get().unwrap();
    if !state.actions.is_empty() {
        log_ok!(logger, "actions: {} loaded", state.actions.len());
    } else {
        log_info!(logger, "actions: none configured");
    }
}

pub fn dispatch(name: &str) {
    if name.is_empty() {
        return;
    }
    let Some(state) = STATE.get() else {
        return;
    };
    let Some(entry) = state.actions.get(name) else {
        log_warning!(state.logger, "exec: unknown '{}'", name);
        return;
    };

    let command = entry.command.clone();
    let action_name = name.to_string();

    match unsafe { fork() } {
        Err(_) => {
            log_error!(state.logger, "exec fork failed");
        }
        Ok(ForkResult::Parent { child }) => {
            // Reap the intermediate child immediately (it exits right after the
            // second fork). The grandchild that actually runs the action gets
            // adopted by init.
            let _ = waitpid(child, None);
            log_info!(state.logger, "exec: {}", name);
        }
        Ok(ForkResult::Child) => {
            // Intermediate: fork once more and exit, so grandchild is orphaned.
            match unsafe { fork() } {
                Ok(ForkResult::Parent { .. }) => unsafe { libc::_exit(0) },
                Err(_) => unsafe { libc::_exit(1) },
                Ok(ForkResult::Child) => {
                    std::env::set_var("MCUB_ACTION_NAME", &action_name);
                    let shell = CString::new("/bin/sh").unwrap();
                    let dash_c = CString::new("-c").unwrap();
                    let cmd = CString::new(command).unwrap();
                    let _ = nix::unistd::execv(&shell, &[shell.clone(), dash_c, cmd]);
                    unsafe { libc::_exit(1) };
                }
            }
        }
    }
}
