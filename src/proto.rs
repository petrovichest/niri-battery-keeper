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

    pub fn print_status() -> Result<(), Box<dyn std::error::Error>> {
        let resp = send(&Request::GetState)?;
        match resp {
            Response::State(st) => {
                let switch = if st.config.disabled { "OFF (kill switch engaged)" } else { "on" };
                println!("Daemon: {switch}");
                println!("Mode: {}", st.active_mode);
                println!("Throttled units: {}", st.throttled_units.len());
                println!();
                println!("Apps: {}", st.apps.len());
                for app in &st.apps {
                    let tag = if app.focused { "F" }
                              else if app.excluded { "X" }
                              else if app.any_throttled { "T" }
                              else { "·" };
                    println!(
                        "  [{tag}] {}  ({} window(s), {} scope(s))",
                        app.app_id, app.window_count, app.scopes.len()
                    );
                    for s in &app.scopes {
                        let m = if s.throttled { "▰" } else { "▱" };
                        println!("        {m} {}  ({} pid)", s.unit, s.pid_count);
                    }
                }
                Ok(())
            }
            Response::Error { message } => Err(message.into()),
            Response::Ok => Ok(()),
        }
    }

    pub fn set_mode(mode: &str) -> Result<(), Box<dyn std::error::Error>> {
        let resp = send(&Request::SetMode { mode: mode.to_string() })?;
        match resp {
            Response::Ok | Response::State(_) => {
                println!("mode set to {mode}");
                Ok(())
            }
            Response::Error { message } => Err(message.into()),
        }
    }

    pub fn set_disabled(disabled: bool) -> Result<(), Box<dyn std::error::Error>> {
        let resp = send(&Request::SetDisabled { disabled })?;
        match resp {
            Response::Ok | Response::State(_) => {
                if disabled {
                    println!("kill switch engaged — all scopes will be released");
                } else {
                    println!("daemon re-enabled");
                }
                Ok(())
            }
            Response::Error { message } => Err(message.into()),
        }
    }
}
