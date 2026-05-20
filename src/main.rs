mod config;
mod cputopo;
mod proto;
mod daemon;
mod gui;
mod bootstrap;

use std::process::ExitCode;

/// Wayland `app_id` of our own GUI window. Used by the GUI to tag its
/// xdg_toplevel and by the daemon to skip the GUI's own window when
/// building its managed-apps list — otherwise we'd see ourselves as an
/// unfocused app and try to throttle the very process the user is
/// looking at.
pub const SELF_APP_ID: &str = "niri-battery-keeper";

fn print_usage() {
    eprintln!(
        "niri-battery-keeper — focus-driven CPU/IO governor for unfocused apps on Niri\n\n\
         Usage:\n  \
           niri-battery-keeper          open the GUI\n  \
           niri-battery-keeper daemon   run the background service (used by systemd)\n\n\
         Everything else — install, mode switching, kill switch, uninstall —\n\
         lives in the GUI."
    );
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    let result: Result<(), Box<dyn std::error::Error>> = match args.as_slice() {
        [] => gui::run(),
        [cmd] if cmd == "daemon" => daemon::run(),
        [cmd] if cmd == "--help" || cmd == "-h" => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        _ => {
            print_usage();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
