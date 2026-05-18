//! Read every leaf cgroup under `user@<uid>.service` and return its current
//! limits + PIDs. Used by the GUI to show "what's running on my system",
//! not just what we manage via Niri windows.
//!
//! cgroup-v2 only — we walk the unified hierarchy at `/sys/fs/cgroup`.

use std::fs;
use std::path::{Path, PathBuf};

use super::cgroup;

/// One leaf cgroup unit (anything ending in `.scope` or `.service`) under
/// the current user's `user@<uid>.service` slice.
#[derive(Debug, Clone)]
pub struct ScannedUnit {
    /// Leaf directory name, e.g. `app-firefox-1234.scope`, `pipewire.service`.
    pub unit: String,
    pub pids: Vec<i32>,
    pub limits: UnitLimits,
}

#[derive(Debug, Clone, Default)]
pub struct UnitLimits {
    /// Human-readable `cpu.max`. `"unset"` when systemd hasn't set a quota,
    /// `"50%"`, `"5%"`, etc. otherwise. `"?"` on parse error.
    pub cpu_max: String,
    /// `cpu.weight`. 100 is the default; `None` if the file is missing.
    pub cpu_weight: Option<u32>,
    /// First numeric value from `io.weight` (typically `"default <n>"`).
    pub io_weight: Option<u32>,
    /// `cgroup.freeze` == 1.
    pub frozen: bool,
}

/// Locate `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/`.
pub fn user_slice_root() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    if path.is_dir() { Some(path) } else { None }
}

/// Walk the user@<uid>.service subtree and return every leaf cgroup that
/// looks like a unit (name ends in `.scope` or `.service`). A leaf in our
/// sense is any such cgroup, even if it has child cgroups — we don't go
/// deeper, because in practice systemd-managed units don't nest further.
pub fn scan() -> Vec<ScannedUnit> {
    let Some(root) = user_slice_root() else {
        log::warn!("user.slice not found under /sys/fs/cgroup; system scan empty");
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&root, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<ScannedUnit>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if is_unit_name(&name) {
            out.push(read_unit(&name, &path));
            // Don't descend — systemd units are treated as opaque.
        } else {
            // A nested slice (app.slice, background.slice, session.slice…).
            walk(&path, out);
        }
    }
}

fn is_unit_name(name: &str) -> bool {
    name.ends_with(".scope") || name.ends_with(".service")
}

fn read_unit(name: &str, dir: &Path) -> ScannedUnit {
    let pids = cgroup::read_cgroup_procs(&cg_rel_path(dir));
    let limits = UnitLimits {
        cpu_max: parse_cpu_max(&read_file(dir, "cpu.max")),
        cpu_weight: parse_weight(&read_file(dir, "cpu.weight")),
        io_weight: parse_weight(&read_file(dir, "io.weight")),
        frozen: read_file(dir, "cgroup.freeze").trim() == "1",
    };
    ScannedUnit {
        unit: name.to_string(),
        pids,
        limits,
    }
}

/// Convert a path like `/sys/fs/cgroup/user.slice/.../foo.scope` to
/// `/user.slice/.../foo.scope` (what other helpers in cgroup.rs expect).
fn cg_rel_path(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    match s.strip_prefix("/sys/fs/cgroup") {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        Some(rest) => format!("/{rest}"),
        None => s.into_owned(),
    }
}

fn read_file(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// `cpu.max` is `"<quota> <period>"` or `"max <period>"`. Both fields are
/// in microseconds. Returns a friendly string like `"50%"` or `"unset"`.
pub fn parse_cpu_max(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return "?".into();
    }
    let mut it = trimmed.split_whitespace();
    let quota = match it.next() {
        Some("max") => return "unset".into(),
        Some(q) => q,
        None => return "?".into(),
    };
    let period = it.next().unwrap_or("100000");
    let (q, p) = match (quota.parse::<u64>(), period.parse::<u64>()) {
        (Ok(q), Ok(p)) if p > 0 => (q, p),
        _ => return "?".into(),
    };
    // q/p * 100, rounded to nearest %.
    let pct = (q as f64 / p as f64) * 100.0;
    if pct.fract() < 0.5 {
        format!("{}%", pct.trunc() as u64)
    } else {
        format!("{}%", pct.trunc() as u64 + 1)
    }
}

/// Either a bare number (`cpu.weight`) or `"default <n>"` / `"<n>"` lines
/// (`io.weight`). Returns the first parseable number.
pub fn parse_weight(s: &str) -> Option<u32> {
    for tok in s.split_whitespace() {
        if let Ok(n) = tok.parse::<u32>() {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_unset() {
        assert_eq!(parse_cpu_max("max 100000\n"), "unset");
    }

    #[test]
    fn cpu_max_half_core() {
        assert_eq!(parse_cpu_max("50000 100000"), "50%");
    }

    #[test]
    fn cpu_max_5_percent() {
        assert_eq!(parse_cpu_max("5000 100000\n"), "5%");
    }

    #[test]
    fn cpu_max_two_cores() {
        assert_eq!(parse_cpu_max("200000 100000"), "200%");
    }

    #[test]
    fn cpu_max_empty() {
        assert_eq!(parse_cpu_max(""), "?");
        assert_eq!(parse_cpu_max("   \n"), "?");
    }

    #[test]
    fn weight_bare_number() {
        assert_eq!(parse_weight("100\n"), Some(100));
    }

    #[test]
    fn weight_default_prefix() {
        assert_eq!(parse_weight("default 100\n"), Some(100));
    }

    #[test]
    fn weight_default_with_overrides() {
        // io.weight can have per-device overrides on later lines.
        assert_eq!(parse_weight("default 100\n8:0 200\n"), Some(100));
    }

    #[test]
    fn weight_missing() {
        assert_eq!(parse_weight(""), None);
    }

    #[test]
    fn is_unit_name_matches() {
        assert!(is_unit_name("app-firefox-1234.scope"));
        assert!(is_unit_name("pipewire.service"));
        assert!(is_unit_name("init.scope"));
        assert!(!is_unit_name("app.slice"));
        assert!(!is_unit_name("background.slice"));
        assert!(!is_unit_name("user@1000.service.d"));
    }
}
