use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use mcub::VERSION;
use mcub::core::config_manager::Config;
use mcub::core::logger::Logger;
use mcub::core::signal_handler;
use mcub::modules::cava_bridge::CavaBridge;
use mcub::modules::hybrid_bridge::HybridBridge;
use mcub::modules::mpd_bridge::MpdBridge;
use mcub::modules::sysinfo_bridge::SysinfoBridge;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        return print_usage(&args[0]);
    }

    let bridge_type = &args[1];
    match bridge_type.as_str() {
        "mpd" | "cava" | "hybrid" | "sysinfo" => {}
        "--version" | "-v" => {
            println!("mcub-bridge-rust {VERSION}");
            return ExitCode::SUCCESS;
        }
        _ => return print_usage(&args[0]),
    }

    let base_dir = resolve_base_dir(&args[0]);
    let settings_path = base_dir.join("shared/settings.json");
    if !settings_path.exists() {
        eprintln!("mcub-bridge-rust: settings.json not found at {}", settings_path.display());
        eprintln!("  set MCUB_BASE_DIR env var to project root");
        return ExitCode::from(1);
    }

    signal_handler::setup(|| {});

    let config = match Config::load(Some(&base_dir)) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("mcub-bridge-rust: config error: {e}");
            return ExitCode::from(1);
        }
    };

    let logger = Arc::new(Logger::new(Some(&config), "BRIDGE", Some(&base_dir)));
    logger.info(&format!("MCUB {} BRIDGE v{}", bridge_type.to_uppercase(), VERSION));

    let exit_code = match bridge_type.as_str() {
        "sysinfo" => {
            let mut bridge = SysinfoBridge::new(Arc::clone(&config), Arc::clone(&logger));
            bridge.run()
        }
        "cava" => {
            let mut bridge = CavaBridge::new(Arc::clone(&config), Arc::clone(&logger));
            bridge.run()
        }
        "mpd" => {
            let bridge = MpdBridge::new(Arc::clone(&config), Arc::clone(&logger));
            bridge.run()
        }
        "hybrid" => {
            let bridge = HybridBridge::new(Arc::clone(&config), Arc::clone(&logger));
            bridge.run()
        }
        _ => unreachable!(),
    };

    ExitCode::from(exit_code as u8)
}

fn print_usage(prog: &str) -> ExitCode {
    eprintln!("Usage: {prog} <mpd|cava|hybrid|sysinfo>");
    ExitCode::from(1)
}

fn resolve_base_dir(argv0: &str) -> PathBuf {
    if let Ok(env) = env::var("MCUB_BASE_DIR") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let exe = Path::new(argv0);
    match exe.parent() {
        Some(dir) => dir.join(".."),
        None => PathBuf::from(".."),
    }
}
