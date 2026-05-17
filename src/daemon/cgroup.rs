use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;

/// Process names (as reported by `/proc/<pid>/comm`) that must never be
/// managed by the daemon — even if Niri reports them as windows. This list
/// guards the compositor, the shell, portals and audio/notification daemons
/// against accidental throttling or freezing.
///
/// Note: `/proc/comm` truncates names to 15 chars + NUL; long process names
/// are matched by their truncated form (e.g. `xdg-desktop-po`).
const PROTECTED_COMMS: &[&str] = &[
    // Wayland compositors / shells
    "niri", "Niri",
    "qs", "quickshell", "Quickshell",
    "sway", "river", "Hyprland", "labwc", "wayfire",
    "kwin_x11", "kwin_wayland", "gnome-shell", "plasmashell",
    // Portals / IPC
    "xdg-desktop-po",            // truncated xdg-desktop-portal*
    "dbus-daemon", "dbus-broker",
    // Status bars / launchers / notifications
    "waybar", "eww", "polybar",
    "fuzzel", "wofi", "rofi", "tofi", "anyrun",
    "swaync", "mako", "dunst",
    // Audio
    "pipewire", "pipewire-pulse", "wireplumber", "pulseaudio",
    // Auth / session
    "polkit-gnome-au", "polkit-kde-auth", "polkit-mate-aut", "lxpolkit",
    "systemd", "systemd-logind",
    // Display managers
    "lightdm", "sddm", "greetd", "tuigreet", "gdm", "gdm-x-session",
];

pub fn is_protected_pid(pid: i32) -> bool {
    let comm = match fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return false,
    };
    if comm.is_empty() {
        return false;
    }
    PROTECTED_COMMS.iter().any(|p| *p == comm)
}

/// Resolve a PID to its leaf systemd unit name (e.g. `app-niri-alacritty-307709.scope`).
///
/// Returns `None` when:
/// - the PID itself belongs to a protected process,
/// - the resolved scope **contains** a protected process (e.g. DMS launched
///   the app via xdg-open and Firefox now lives in DMS's scope — touching
///   that scope would freeze DMS too),
/// - `/proc/<pid>` is gone,
/// - the process is not inside its own per-app scope under `app.slice`,
/// - the cgroup file is malformed.
pub fn resolve_unit(pid: i32) -> Option<String> {
    if is_protected_pid(pid) {
        log::debug!("pid {pid} is a protected process; skipping");
        return None;
    }
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let (scope, cg_rel_path) = parse_cgroup_to_unit_and_path(&text)?;
    if scope_contains_protected_pid(&cg_rel_path) {
        log::debug!("scope {scope} contains a protected pid; skipping");
        return None;
    }
    Some(scope)
}

/// Read `/sys/fs/cgroup/<cg_rel_path>/cgroup.procs` and check if any PID in
/// that scope is protected. If any is, the entire scope is off-limits.
fn scope_contains_protected_pid(cg_rel_path: &str) -> bool {
    let fs_path = format!("/sys/fs/cgroup{cg_rel_path}/cgroup.procs");
    let text = match fs::read_to_string(&fs_path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for pid_str in text.split_whitespace() {
        if let Ok(pid) = pid_str.parse::<i32>() {
            if is_protected_pid(pid) {
                return true;
            }
        }
    }
    false
}

fn parse_cgroup_to_unit_and_path(cgroup_text: &str) -> Option<(String, String)> {
    let line = cgroup_text
        .lines()
        .find(|l| l.starts_with("0::"))
        .or_else(|| cgroup_text.lines().next())?;
    let path = line.splitn(3, ':').nth(2)?.trim();
    let leaf = path.rsplit('/').next()?.trim();
    if leaf.is_empty() { return None; }
    if !path.contains("/app.slice/") { return None; }
    if !(leaf.ends_with(".scope") && (leaf.starts_with("app-") || leaf.starts_with("run-"))) {
        return None;
    }
    Some((leaf.to_string(), path.to_string()))
}

fn parse_cgroup_to_unit(cgroup_text: &str) -> Option<String> {
    parse_cgroup_to_unit_and_path(cgroup_text).map(|(u, _)| u)
}

/// Small thread-safe cache `pid -> Option<unit>`.
/// `Option<None>` is cached too — to avoid re-reading /proc for processes that
/// will never have a unit (e.g. niri itself).
#[derive(Default)]
pub struct UnitCache {
    inner: Mutex<HashMap<i32, Option<String>>>,
}

impl UnitCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup(&self, pid: i32) -> Option<String> {
        {
            let guard = self.inner.lock().unwrap();
            if let Some(v) = guard.get(&pid) {
                return v.clone();
            }
        }
        let resolved = resolve_unit(pid);
        let mut guard = self.inner.lock().unwrap();
        guard.insert(pid, resolved.clone());
        resolved
    }

    pub fn invalidate(&self, pid: i32) {
        self.inner.lock().unwrap().remove(&pid);
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// Resolve every PID in the process tree rooted at `root_pid` to its
    /// systemd unit. Returns a deduplicated map `unit -> count of PIDs in it`
    /// (cgroup-shared infra processes contribute their count too, which is
    /// useful for the GUI's "(N pids)" display).
    pub fn resolve_scopes_for_tree(&self, root_pid: i32) -> std::collections::HashMap<String, usize> {
        use std::collections::HashMap;
        let mut out: HashMap<String, usize> = HashMap::new();
        for pid in super::proctree::descendants(root_pid) {
            if let Some(unit) = self.lookup(pid) {
                *out.entry(unit).or_insert(0) += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_scope() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-niri-alacritty-307709.scope\n";
        assert_eq!(parse_cgroup_to_unit(s).as_deref(), Some("app-niri-alacritty-307709.scope"));
    }

    #[test]
    fn parses_run_scope() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/run-p1828-i8399.scope";
        assert_eq!(parse_cgroup_to_unit(s).as_deref(), Some("run-p1828-i8399.scope"));
    }

    #[test]
    fn rejects_session_scope() {
        let s = "0::/user.slice/user-1000.slice/session-2.scope";
        assert_eq!(parse_cgroup_to_unit(s), None);
    }

    #[test]
    fn rejects_app_slice_itself() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/app.slice";
        assert_eq!(parse_cgroup_to_unit(s), None);
    }

    #[test]
    fn rejects_user_slice_root() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/init.scope";
        assert_eq!(parse_cgroup_to_unit(s), None);
    }
}
