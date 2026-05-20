//! Multi-call mode: when the main binary is invoked as `nbk-set-rapl` (i.e.
//! the file name of argv[0] matches), it acts as a privileged helper that
//! writes Intel RAPL power limits. Installed by [`crate::bootstrap::install_tdp`]
//! as a root-owned copy of the main binary in `/usr/local/bin/`. pkexec
//! invokes it under the polkit action `org.niri-battery-keeper.set-rapl`.
//!
//! Validates inputs hard — refuses values outside a sane microwatt band so a
//! bug in the GUI can't dial PL1 to 0 W and brick the session.

use std::fs;
use std::process::ExitCode;

const RAPL_BASE: &str = "/sys/class/powercap/intel-rapl:0";
const MIN_UW: u64 = 0;
const MAX_UW: u64 = 150_000_000;

pub const HELPER_NAME: &str = "nbk-set-rapl";

fn parse_uw(s: &str, name: &str) -> Result<u64, String> {
    let v: u64 = s.parse().map_err(|_| format!("{name}: not a number"))?;
    if !(MIN_UW..=MAX_UW).contains(&v) {
        return Err(format!(
            "{name}={v} uW outside [{MIN_UW},{MAX_UW}] — refusing"
        ));
    }
    Ok(v)
}

fn write(constraint: u32, uw: u64) -> Result<(), String> {
    let path = format!("{RAPL_BASE}/constraint_{constraint}_power_limit_uw");
    fs::write(&path, uw.to_string()).map_err(|e| format!("{path}: {e}"))
}

pub fn run(args: Vec<String>) -> ExitCode {
    if args.len() != 2 {
        eprintln!("usage: nbk-set-rapl <pl1_uw> <pl2_uw>");
        return ExitCode::from(2);
    }
    let pl1 = match parse_uw(&args[0], "pl1_uw") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let pl2 = match parse_uw(&args[1], "pl2_uw") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = write(0, pl1) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = write(1, pl2) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
