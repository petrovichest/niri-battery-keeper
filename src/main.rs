mod config;
mod proto;
mod daemon;
mod gui;

use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "niri-battery-keeper — focus-driven CPU/IO governor for unfocused apps on Niri\n\n\
         Usage:\n  \
           niri-battery-keeper                run GUI\n  \
           niri-battery-keeper daemon         run daemon (for systemd user service)\n  \
           niri-battery-keeper mode <name>    switch global mode (off|minimal|pause|…)\n  \
           niri-battery-keeper status         print daemon state and exit\n  \
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
