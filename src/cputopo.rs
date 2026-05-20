//! Detect Intel hybrid CPU topology (P-cores / E-cores / LP E-cores) so the
//! GUI can offer "pin unfocused apps to efficient cores only" presets and
//! the throttler can convert those into a cpuset string for systemd's
//! `AllowedCPUs=`.
//!
//! Kernels expose hybrid clusters as separate `cpu_*` devices in sysfs:
//!   /sys/devices/cpu_core/cpus      → P-cores
//!   /sys/devices/cpu_atom/cpus      → E-cores
//!   /sys/devices/cpu_lowpower/cpus  → LP E-cores (Meteor/Arrow Lake SoC tile)
//!
//! Each file is a cpuset list ("0-3" / "4-11" / "12,13") suitable for
//! handing straight to systemd.

use std::fs;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct Topology {
    pub p_cores: Option<String>,
    pub e_cores: Option<String>,
    pub lp_cores: Option<String>,
}

impl Topology {
    pub fn is_hybrid(&self) -> bool {
        self.p_cores.is_some() && (self.e_cores.is_some() || self.lp_cores.is_some())
    }

    /// All efficient cores (E + LP) joined into one cpuset string.
    pub fn efficient(&self) -> Option<String> {
        match (&self.e_cores, &self.lp_cores) {
            (Some(e), Some(lp)) => Some(format!("{e},{lp}")),
            (Some(e), None) => Some(e.clone()),
            (None, Some(lp)) => Some(lp.clone()),
            (None, None) => None,
        }
    }
}

pub fn detect() -> &'static Topology {
    static CACHE: OnceLock<Topology> = OnceLock::new();
    CACHE.get_or_init(detect_once)
}

fn detect_once() -> Topology {
    Topology {
        p_cores: read_cpuset("/sys/devices/cpu_core/cpus"),
        e_cores: read_cpuset("/sys/devices/cpu_atom/cpus"),
        lp_cores: read_cpuset("/sys/devices/cpu_lowpower/cpus"),
    }
}

fn read_cpuset(path: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}
