//! Self-bootstrap from a single binary.
//!
//! Copy `/proc/self/exe` into `~/.local/bin/`, write the embedded systemd
//! user unit, then `enable --now` it. Driven by the GUI's "Install service"
//! banner; [`remove_service`] is the corresponding teardown for the GUI's
//! "Uninstall service" button. The binary itself has no `install` /
//! `uninstall` CLI — everything lifecycle-related lives in the GUI.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_FILE_NAME: &str = "niri-battery-keeper.service";
const BIN_NAME: &str = "niri-battery-keeper";

/// Embedded copy of the systemd user unit. Single source of truth: the file
/// on disk under `systemd/` and the one written by `install` are the same
/// bytes.
const EMBEDDED_UNIT: &str = include_str!("../systemd/niri-battery-keeper.service");

/// Files installed by the TDP "Install helper" GUI flow. Paths are baked
/// into the embedded polkit policy below — keep these in sync.
const TDP_HELPER_PATH: &str = "/usr/local/bin/nbk-set-rapl";
const TDP_POLICY_PATH: &str =
    "/usr/share/polkit-1/actions/org.niri-battery-keeper.set-rapl.policy";
const TDP_UDEV_PATH: &str = "/etc/udev/rules.d/60-intel-rapl-energy.rules";

const TDP_POLICY: &str =
    include_str!("../assets/org.niri-battery-keeper.set-rapl.policy");
const TDP_UDEV: &str = include_str!("../assets/60-intel-rapl-energy.rules");

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

/// One-shot GUI install of the TDP root-helper stack. Invokes
/// `pkexec sh -c '…'` once; on success the user gets a working TDP tab
/// with a single root-password prompt covering:
///
///   1. the helper binary (a root-owned copy of /proc/self/exe placed at
///      [`TDP_HELPER_PATH`]; the multi-call dispatch in `main.rs` routes
///      argv[0] = nbk-set-rapl to [`crate::rapl_helper::run`]);
///   2. the polkit policy that allows pkexec to call the helper with
///      `auth_admin_keep` so subsequent Apply clicks don't re-prompt;
///   3. the udev rule that opens `energy_uj` to the wheel group so the
///      GUI's live wattage readout can read the counter without root;
///   4. udevadm reload + immediate chmod on existing RAPL nodes (the
///      reload alone won't re-fire ACTION=="add" for already-bound nodes
///      until next boot or hotplug).
pub fn install_tdp() -> Result<(), Box<dyn Error>> {
    // /proc/self/exe canonicalizes to the actual on-disk binary (typically
    // ~/.local/bin/niri-battery-keeper after `install()`). Using current_exe()
    // and not the symlink path keeps the install reproducible across the
    // user's shell PATH quirks.
    let src = std::env::current_exe()
        .and_then(|p| std::fs::canonicalize(&p).or(Ok(p)))
        .map_err(|e| format!("can't locate own executable: {e}"))?;
    let src_str = src
        .to_str()
        .ok_or("own executable path is not UTF-8 — refusing to shell-escape")?;
    if src_str.contains('\'') {
        return Err(format!(
            "own executable path contains a single quote ({src_str}); refusing"
        )
        .into());
    }

    let script = format!(
        r#"set -e
install -m 755 -o root -g root '{src}' '{helper}'
cat > '{policy}' <<'NBK_POLICY_EOF'
{policy_body}
NBK_POLICY_EOF
chmod 0644 '{policy}'
cat > '{udev}' <<'NBK_UDEV_EOF'
{udev_body}
NBK_UDEV_EOF
chmod 0644 '{udev}'
udevadm control --reload-rules || true
for f in /sys/class/powercap/intel-rapl:*/energy_uj; do
    [ -e "$f" ] && chgrp wheel "$f" 2>/dev/null && chmod g+r "$f" 2>/dev/null || true
done
"#,
        src = src_str,
        helper = TDP_HELPER_PATH,
        policy = TDP_POLICY_PATH,
        policy_body = TDP_POLICY.trim_end(),
        udev = TDP_UDEV_PATH,
        udev_body = TDP_UDEV.trim_end(),
    );

    let out = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .map_err(|e| format!("spawn pkexec: {e}"))?;

    if !out.status.success() {
        // pkexec exit codes: 126 user dismissed auth, 127 not authorized, 1
        // command failure. Surface stderr so the GUI can show the real reason.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let msg = stderr.trim();
        let detail = if msg.is_empty() {
            String::new()
        } else {
            format!(": {msg}")
        };
        return Err(format!("pkexec exited {code}{detail}").into());
    }
    Ok(())
}
