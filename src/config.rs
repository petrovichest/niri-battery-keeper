use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub active_mode: String,
    pub modes: BTreeMap<String, Profile>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub apps: BTreeMap<String, AppRule>,
}

/// What to do with an unfocused app.
///
/// `none`     — leave it alone.
/// `throttle` — apply CPUQuota + CPUWeight + IOWeight.
/// `pause`    — freeze the cgroup; app keeps memory but uses 0% CPU until refocused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Profile {
    None,
    Throttle {
        /// e.g. "25%". 100% = one full core. Empty string ⇒ unset (unlimited).
        cpu_quota: CpuQuota,
        /// 1..=10000. Systemd default 100.
        cpu_weight: u32,
        /// 1..=10000. Systemd default 100.
        io_weight: u32,
    },
    Pause,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct CpuQuota(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    #[serde(default = "default_true")]
    pub manage_all: bool,
    #[serde(default = "default_excludes")]
    pub hardcoded_exclude: Vec<String>,
    #[serde(default = "default_grace")]
    pub unfocus_grace_ms: u64,
    #[serde(default = "default_true")]
    pub skip_shared_scopes: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            manage_all: true,
            hardcoded_exclude: default_excludes(),
            unfocus_grace_ms: default_grace(),
            skip_shared_scopes: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "override", rename_all = "snake_case")]
pub enum AppRule {
    UseMode,
    Profile { profile: String },
    Exclude,
}

fn default_true() -> bool { true }
fn default_grace() -> u64 { 1500 }
fn default_excludes() -> Vec<String> {
    [
        "mpv", "io.mpv.Mpv", "vlc", "org.videolan.VLC",
        "obs", "com.obsproject.Studio", "Spotify",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        let mut modes = BTreeMap::new();
        modes.insert("off".into(), Profile::None);
        modes.insert(
            "minimal".into(),
            Profile::Throttle {
                cpu_quota: CpuQuota("5%".into()),
                cpu_weight: 5,
                io_weight: 5,
            },
        );
        modes.insert("pause".into(), Profile::Pause);
        Config {
            active_mode: "off".into(),
            modes,
            policy: Policy::default(),
            apps: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                home.join(".config")
            });
        base.join("niri-battery-keeper").join("config.toml")
    }

    pub fn load_or_default() -> Self {
        let path = Self::path();
        match Self::load_from(&path) {
            Ok(cfg) => cfg,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound) => {
                log::info!("config not found at {}, using defaults", path.display());
                let cfg = Config::default();
                if let Err(e) = cfg.save_to(&path) {
                    log::warn!("could not write default config: {e}");
                }
                cfg
            }
            Err(e) => {
                log::error!("failed to load config {}: {}", path.display(), e);
                Config::default()
            }
        }
    }

    pub fn load_from(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, text)
    }

    /// Effective profile for a given app_id. `Profile::None` (or excluded ⇒
    /// `None`) both mean "leave this app alone".
    pub fn resolve_profile(&self, app_id: &str) -> Option<Profile> {
        if self.policy.hardcoded_exclude.iter().any(|s| s == app_id) {
            return None;
        }
        match self.apps.get(app_id) {
            Some(AppRule::Exclude) => None,
            Some(AppRule::Profile { profile }) => self.modes.get(profile).cloned(),
            Some(AppRule::UseMode) | None => {
                if !self.policy.manage_all && !self.apps.contains_key(app_id) {
                    return None;
                }
                self.modes.get(&self.active_mode).cloned()
            }
        }
    }
}
