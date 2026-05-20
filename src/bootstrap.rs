//! `install` / `uninstall` subcommands.
//!
//! Self-bootstrap from a single binary: copy `/proc/self/exe` into
//! `~/.local/bin/`, write the embedded systemd user unit, then `enable --now`
//! it. Uninstall reverses everything (config dir is left alone unless the
//! user passes `--purge`).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_FILE_NAME: &str = "niri-battery-keeper.service";
const BIN_NAME: &str = "niri-battery-keeper";

/// Embedded copy of the systemd user unit. Single source of truth: the file
/// on disk under `systemd/` and the one written by `install` are the same
/// bytes.
const EMBEDDED_UNIT: &str = include_str!("../systemd/niri-battery-keeper.service");

fn home() -> Result<PathBuf, Box<dyn Error>> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn bin_target() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".local").join("bin").join(BIN_NAME))
}

fn unit_target() -> Result<PathBuf, Box<dyn Error>> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().unwrap_or_default().join(".config"));
    Ok(base.join("systemd").join("user").join(UNIT_FILE_NAME))
}

/// Cheap check: does the user-level systemd unit exist on disk?
///
/// Used by the GUI to decide whether to offer a one-click "Install service"
/// prompt. A missing unit is the clearest "not installed yet" signal — a unit
/// that exists but is masked/disabled is a deliberate user state we don't
/// want to second-guess.
pub fn is_installed() -> bool {
    unit_target()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Sanity-check that `systemctl --user` can reach a user manager. On
/// non-systemd distros this prints a helpful error instead of dumping a
/// cryptic systemctl message.
fn check_user_systemd() -> Result<(), Box<dyn Error>> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map_err(|e| format!("can't run systemctl: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "systemctl --user is not reachable ({}). \
             niri-battery-keeper needs systemd user services; this distro \
             or session doesn't appear to have them.\n\
             details: {}",
            out.status,
            stderr.trim(),
        )
        .into());
    }
    Ok(())
}

fn systemctl_user(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl --user {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "systemctl --user {} failed ({}): {}",
            args.join(" "),
            out.status,
            stderr.trim(),
        )
        .into());
    }
    Ok(())
}

/// Best-effort systemctl call — log failures but don't propagate them.
/// Used in the uninstall path so a half-installed state can still be cleaned.
fn systemctl_user_best_effort(args: &[&str]) {
    match Command::new("systemctl").arg("--user").args(args).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            log::debug!(
                "systemctl --user {} failed ({}): {}",
                args.join(" "),
                out.status,
                stderr.trim(),
            );
        }
        Err(e) => log::debug!("systemctl --user {}: {e}", args.join(" ")),
    }
}

fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    use std::io::Write;
    f.write_all(contents)?;
    f.sync_all()?;
    // Re-apply mode in case the file already existed with different perms.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn install() -> Result<(), Box<dyn Error>> {
    check_user_systemd()?;

    let src = std::env::current_exe()
        .map_err(|e| format!("can't locate own executable: {e}"))?;
    let bin = bin_target()?;
    let unit = unit_target()?;

    // Copy the binary into ~/.local/bin/. Skip if we're already running from
    // the destination so `install` is a safe re-run (e.g. after `cargo install`
    // or a previous bootstrap).
    let src_canon = std::fs::canonicalize(&src).unwrap_or_else(|_| src.clone());
    let bin_canon = std::fs::canonicalize(&bin).ok();
    if bin_canon.as_deref() == Some(src_canon.as_path()) {
        println!("binary already at {} — skipping copy", bin.display());
    } else {
        if let Some(parent) = bin.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = std::fs::read(&src)
            .map_err(|e| format!("reading {}: {e}", src.display()))?;
        write_with_mode(&bin, &bytes, 0o755)
            .map_err(|e| format!("writing {}: {e}", bin.display()))?;
        println!("installed binary → {}", bin.display());
    }

    write_with_mode(&unit, EMBEDDED_UNIT.as_bytes(), 0o644)
        .map_err(|e| format!("writing {}: {e}", unit.display()))?;
    println!("installed unit   → {}", unit.display());

    systemctl_user(&["daemon-reload"])?;
    systemctl_user(&["enable", "--now", UNIT_FILE_NAME])?;
    println!("enabled & started {UNIT_FILE_NAME}");
    println!("\nRun `niri-battery-keeper status` to verify the daemon is up.");
    Ok(())
}

/// Tear down only the systemd user service: `disable --now`, remove the
/// unit file, daemon-reload. Leaves the binary in `~/.local/bin/` and the
/// config dir alone — used by the GUI's "Remove service" button, where the
/// user wants to stop autostart but keep the app installed.
pub fn remove_service() -> Result<(), Box<dyn Error>> {
    // Best-effort: a half-installed state should still clean up.
    systemctl_user_best_effort(&["disable", "--now", UNIT_FILE_NAME]);

    let unit = unit_target()?;
    match std::fs::remove_file(&unit) {
        Ok(()) => println!("removed {}", unit.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no unit at {} — skipping", unit.display());
        }
        Err(e) => return Err(format!("removing {}: {e}", unit.display()).into()),
    }

    systemctl_user_best_effort(&["daemon-reload"]);
    Ok(())
}

pub fn uninstall(purge_config: bool) -> Result<(), Box<dyn Error>> {
    remove_service()?;

    let bin = bin_target()?;
    match std::fs::remove_file(&bin) {
        Ok(()) => println!("removed {}", bin.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no binary at {} — skipping", bin.display());
        }
        Err(e) => return Err(format!("removing {}: {e}", bin.display()).into()),
    }

    let cfg_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().unwrap_or_default().join(".config"))
        .join("niri-battery-keeper");

    if purge_config {
        match std::fs::remove_dir_all(&cfg_dir) {
            Ok(()) => println!("removed {}", cfg_dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("removing {}: {e}", cfg_dir.display()).into()),
        }
    } else if cfg_dir.exists() {
        println!(
            "\nConfig dir kept at {}.\n\
             Re-run with `uninstall --purge` to delete it too.",
            cfg_dir.display(),
        );
    }

    Ok(())
}
