use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    GetState,
    SetMode { mode: String },
    SetConfig { config: Config },
    SetDisabled { disabled: bool },
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    State(DaemonState),
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonState {
    pub active_mode: String,
    pub config: Config,
    pub windows: Vec<WindowInfo>,
    /// Apps grouped with all their discovered scopes.
    pub apps: Vec<AppGroupInfo>,
    pub throttled_units: Vec<String>,
    /// Every leaf cgroup unit (scope/service) under the current user's
    /// `user@<uid>.service` slice, categorized by what the daemon is doing
    /// with it. Empty when the cgroup tree can't be read.
    #[serde(default)]
    pub system_units: Vec<SystemUnitInfo>,
    /// Live battery / CPU / platform power, time-to-empty estimate, and a
    /// rolling 10-minute sample buffer for the GUI's energy graph. Always
    /// present; individual fields are `Option<_>` for hardware that
    /// doesn't expose a given counter.
    #[serde(default)]
    pub energy: EnergyInfo,
}

/// One row of the rolling battery-level timeline. Newest sample has the
/// smallest `age_s`. One sample per ~10 s; the live wattage breakdown for
/// "what's draining right now" lives on [`EnergyInfo`] directly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnergySample {
    /// Seconds before the moment the snapshot was built. `0.0` is "now".
    /// Always computed at snapshot time from `now - at_unix`, so it stays
    /// honest across daemon restarts and AC plug/unplug events that
    /// would otherwise leave gaps in a position-derived age.
    pub age_s: f32,
    /// Battery percentage at this point. Computed as
    /// `charge_now / charge_full × 100` rather than the integer `capacity`
    /// file so the graph shows fractional slope rather than 1 %
    /// step-changes per minute.
    pub capacity_pct: Option<f32>,
    /// True when this sample's window was a discharging period. Drives
    /// per-bar tinting (amber discharging / green charging) in the GUI.
    pub discharging: bool,
    /// True when this sample's window was a charging period. Disjoint
    /// from `discharging`; both false means full/idle on AC.
    #[serde(default)]
    pub charging: bool,
    /// Average battery flow (W) over the sample's aggregation window.
    /// Useful for graph tooltips, not the main signal.
    pub battery_w: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnergyInfo {
    /// Instantaneous magnitude of battery flow in watts. Sign-less; use
    /// [`Self::charge_state`] to tell charge from discharge.
    pub battery_w: Option<f32>,
    /// CPU package power, watts.
    pub pkg_w: Option<f32>,
    /// Whole-platform RAPL (psys), watts. Absent on chips without the
    /// psys domain — most desktop SKUs, some older mobile parts.
    pub psys_w: Option<f32>,
    pub capacity_pct: Option<u8>,
    /// `"charging" | "discharging" | "full" | "not_charging" | "unknown"`.
    pub charge_state: String,
    /// AC adapter `online` flag. Independent of charge_state because some
    /// laptops report "Not charging" with AC plugged in at 100%.
    pub on_ac: bool,
    /// Smoothed seconds-to-empty (when discharging) or seconds-to-full
    /// (when charging). `None` for AC/idle or while the EMA hasn't warmed.
    pub time_remaining_s: Option<u32>,
    /// Watt-hours discharged from the battery during the current
    /// discharge session. Resets when AC is plugged in (so the value
    /// the user sees is "this discharge cycle", not a lifetime total).
    /// Persists across daemon restarts that happen mid-session.
    pub session_discharge_wh: f32,
    /// Watt-hours pushed into the battery during the current charging
    /// session. Mirror of [`Self::session_discharge_wh`]: resets when AC
    /// is unplugged. Persists across daemon restarts.
    #[serde(default)]
    pub session_charge_wh: f32,
    /// Active (awake) seconds on battery in the current discharge session.
    /// `None` while on AC or before any transition has been recorded.
    #[serde(default)]
    pub on_battery_active_s: Option<u32>,
    /// Active (awake) seconds on AC in the current charge session.
    /// `None` while on battery.
    #[serde(default)]
    pub on_ac_active_s: Option<u32>,
    /// Rolling battery-level samples (oldest first, newest last). Spans
    /// the current battery session including any post-plug recharge,
    /// capped at ~48 h of 10 s buckets. Survives daemon restarts.
    pub samples: Vec<EnergySample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: u64,
    pub app_id: String,
    pub title: String,
    pub pid: Option<i32>,
    pub focused: bool,
    pub unit: Option<String>,
    pub throttled: bool,
    pub excluded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppGroupInfo {
    pub app_id: String,
    pub window_count: usize,
    pub focused: bool,
    pub excluded: bool,
    pub any_throttled: bool,
    pub scopes: Vec<ScopeInfo>,
    /// Live CPU usage of this app summed across its scopes, in "percent of one
    /// core" (htop convention — a fully-busy 4-thread app reads ~400.0).
    /// `None` on the first poll after the GUI opens, or when no sample
    /// interval has elapsed yet.
    #[serde(default)]
    pub cpu_pct: Option<f32>,
    /// Approximate CPU-package wattage attributed to this app: scope CPU
    /// share × smoothed pkg W. Proportional estimate, not a per-process
    /// measurement — hardware doesn't measure that. `None` when either
    /// `cpu_pct` or `pkg_w` is unavailable.
    #[serde(default)]
    pub est_w: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub unit: String,
    pub pid_count: usize,
    pub throttled: bool,
    /// Real cgroup-v2 limits as currently set in the kernel (not the
    /// configured profile). `None` when the scope wasn't found during scan.
    #[serde(default)]
    pub limits: Option<CgroupLimits>,
    /// True when this scope is also assigned to another app_id — i.e. two
    /// Niri-tracked apps share the same systemd scope (xdg-open case).
    #[serde(default)]
    pub shared: bool,
    /// Live per-scope CPU%, same units as [`AppGroupInfo::cpu_pct`].
    #[serde(default)]
    pub cpu_pct: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SystemUnitCategory {
    /// Already shown under a Niri app in `apps[]`. GUI dedups by skipping.
    Managed { app_id: String },
    /// No Niri window, no protected pid — a background app/helper.
    Orphan,
    /// Contains a protected process (compositor, audio, portal…). Daemon
    /// will refuse to throttle it.
    Protected { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: i32,
    pub comm: String,
    pub cmdline: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CgroupLimits {
    /// e.g. `"unset"`, `"50%"`, `"5%"`.
    pub cpu_max: String,
    pub cpu_weight: Option<u32>,
    pub io_weight: Option<u32>,
    pub frozen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemUnitInfo {
    pub unit: String,
    pub category: SystemUnitCategory,
    pub pid_count: usize,
    /// Up to 16 sampled processes from this unit (avoids ballooning the IPC
    /// payload for units with hundreds of pids).
    pub processes: Vec<ProcessInfo>,
    pub limits: CgroupLimits,
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("niri-battery-keeper.sock")
}

pub mod client {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    pub fn send(req: &Request) -> Result<Response, Box<dyn std::error::Error>> {
        let path = socket_path();
        let mut sock = UnixStream::connect(&path)
            .map_err(|e| format!("cannot connect to daemon at {}: {e}", path.display()))?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        let payload = serde_json::to_string(req)?;
        sock.write_all(payload.as_bytes())?;
        sock.write_all(b"\n")?;
        sock.flush()?;
        let mut reader = BufReader::new(sock);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let resp: Response = serde_json::from_str(line.trim())?;
        Ok(resp)
    }
}
