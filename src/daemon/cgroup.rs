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
    protected_reason(pid).is_some()
}

/// If `pid` is a protected process, return its `/proc/<pid>/comm` value.
/// Lets callers (GUI) explain *why* a scope was skipped.
pub fn protected_reason(pid: i32) -> Option<String> {
    let comm = read_comm(pid);
    if comm.is_empty() {
        return None;
    }
    if PROTECTED_COMMS.iter().any(|p| *p == comm) {
        Some(comm)
    } else {
        None
    }
}

/// Read `/proc/<pid>/comm`. Returns "" on error or for dead pids.
pub fn read_comm(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Read `/proc/<pid>/cmdline`, replacing NUL separators with spaces.
/// Truncated to `max_len` bytes (with a "…" suffix when truncated).
pub fn read_cmdline(pid: i32, max_len: usize) -> String {
    let bytes = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    // Trim trailing NULs, replace internal NULs with spaces.
    let trimmed: Vec<u8> = bytes
        .into_iter()
        .rev()
        .skip_while(|b| *b == 0)
        .collect::<Vec<u8>>()
        .into_iter()
        .rev()
        .map(|b| if b == 0 { b' ' } else { b })
        .collect();
    let s = String::from_utf8_lossy(&trimmed).into_owned();
    if s.len() > max_len {
        let mut cut = max_len.saturating_sub(1);
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    } else {
        s
    }
}

/// Read every PID listed in `/sys/fs/cgroup<cg_rel_path>/cgroup.procs`.
/// Returns an empty vec on read errors (cgroup gone / no permission).
pub fn read_cgroup_procs(cg_rel_path: &str) -> Vec<i32> {
    let fs_path = format!("/sys/fs/cgroup{cg_rel_path}/cgroup.procs");
    let text = match fs::read_to_string(&fs_path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.split_whitespace()
        .filter_map(|s| s.parse::<i32>().ok())
        .collect()
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

/// Check if any PID inside `/sys/fs/cgroup<cg_rel_path>/cgroup.procs` is
/// protected. If any is, the entire scope is off-limits.
fn scope_contains_protected_pid(cg_rel_path: &str) -> bool {
    read_cgroup_procs(cg_rel_path)
        .into_iter()
        .any(is_protected_pid)
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
