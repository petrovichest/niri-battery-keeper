mod config;
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
           niri-battery-keeper                run GUI\n  \
           niri-battery-keeper daemon         run daemon (for systemd user service)\n  \
           niri-battery-keeper mode <name>    switch global mode (off|minimal|pause|…)\n  \
           niri-battery-keeper status         print daemon state and exit\n  \
           niri-battery-keeper disable        kill switch ON — release every scope, stop applying anything\n  \
           niri-battery-keeper enable         kill switch OFF — resume normal operation\n  \
           niri-battery-keeper install        copy self into ~/.local/bin, write systemd unit, enable --now\n  \
           niri-battery-keeper uninstall      reverse `install`; add --purge to also delete the config dir\n  \
           niri-battery-keeper --help         show this help"
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
        [cmd] if cmd == "status" => proto::client::print_status(),
        [cmd, mode] if cmd == "mode" => proto::client::set_mode(mode),
        [cmd] if cmd == "disable" => proto::client::set_disabled(true),
        [cmd] if cmd == "enable" => proto::client::set_disabled(false),
        [cmd] if cmd == "install" => bootstrap::install(),
        [cmd] if cmd == "uninstall" => bootstrap::uninstall(false),
        [cmd, flag] if cmd == "uninstall" && (flag == "--purge" || flag == "-p") => {
            bootstrap::uninstall(true)
        }
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
