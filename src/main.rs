#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

mod composition;
mod config;
mod diagnostics;
mod hardware;
mod render;
mod shell;
mod taskbar_host;
mod telemetry;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--probe") {
        diagnostics::probe(&args);
        return;
    }
    if let Err(error) = shell::run(&args) {
        let directory = config::diagnostics_dir();
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("startup-error.txt");
        let _ = std::fs::write(path, format!("Taskbar Monitor startup failed: {error}\n"));
    }
}
