//! Power and energy sampler. Polled by the main daemon loop ~1 Hz alongside
//! the existing 30 s battery tick. Gives the GUI three concurrent series for
//! a rolling timeline (battery W, CPU package W, platform W), a smoothed
//! time-remaining estimate, and a session Wh integral.
//!
//! Sources (all sysfs, no root once the existing udev rule for
//! `intel-rapl/*/energy_uj` is in place):
//!
//! - `/sys/class/power_supply/BAT*/{voltage_now,current_now}` — battery
//!   power magnitude in W. Sign of the flow comes from `status` (Charging
//!   vs Discharging), not the `current_now` value, because per-laptop
//!   driver conventions for the sign differ.
//! - `/sys/class/powercap/intel-rapl:0/energy_uj` — CPU package counter.
//! - `/sys/class/powercap/intel-rapl:N/energy_uj` where `name == "psys"` —
//!   whole-platform RAPL domain. Not present on every chip; absent fields
//!   render as `—` in the GUI.
//!
//! Per-app attribution is *not* here; that lives in the daemon's snapshot
//! path, which already reads `cpu.stat` per scope. This module just exposes
//! the current smoothed `pkg_w` so the snapshot can multiply by each
//! scope's CPU share.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::daemon::battery::{self, ChargeState};
use crate::proto::{EnergyInfo, EnergySample};

/// One history point per [`HISTORY_BUCKET`] seconds × [`RING_CAPACITY`] =
/// 60 minutes visible in the GUI. Long enough for a meaningful
/// "config A vs config B" comparative-test discharge curve, short enough
/// that the JSON payload stays in the tens of KB.
const RING_CAPACITY: usize = 360;
const HISTORY_BUCKET_S: f32 = 10.0;

/// Window for the time-remaining moving average. Past 60 ticks (~60 s) of
/// raw battery_w samples are averaged for the time estimate — long enough
/// for the prediction to be stable, short enough to follow a real change
/// (closed a heavy app, dropped brightness). Plain mean over a deque
/// rather than an EMA because EMAs seed from the first sample, and a
/// daemon that starts during a PSR-induced low-current moment will then
/// take minutes to converge to reality.
const TIME_WINDOW_TICKS: usize = 60;

/// Shorter tau for the pkg-watt EMA used by per-app `≈ W` attribution.
/// Long enough to kill single-poll jitter, short enough to follow a real
/// workload burst.
const PKG_EMA_TAU_S: f32 = 10.0;

pub struct EnergyMeter {
    last_sample_at: Option<Instant>,
    prev_pkg_uj: Option<u64>,
    prev_psys_uj: Option<u64>,
    pkg_max_uj: u64,
    psys_max_uj: u64,

    pkg_path: Option<PathBuf>,
    psys_path: Option<PathBuf>,
    bat_path: Option<PathBuf>,
    ac_online_path: Option<PathBuf>,

    /// Latest raw battery wattage — what the cards display. No smoothing
    /// here; users want to see brightness/CPU changes land within a poll.
    last_battery_w: Option<f32>,
    /// Latest raw CPU package wattage for the card.
    last_pkg_w: Option<f32>,
    /// Latest raw psys reading. No EMA — psys swings widely with brief
    /// background tasks and smoothing would mask the very thing the user
    /// is comparing across (e.g. a brightness toggle).
    last_psys_w: Option<f32>,
    /// Moving-average window for the time-remaining estimate. Mean of the
    /// last 60 raw battery_w samples; deterministic warmup, no seed bias.
    recent_battery_w: VecDeque<f32>,
    /// EMA of CPU package wattage. Used by per-app `≈ W` attribution
    /// (smoothed because per-app columns shouldn't whiplash poll-to-poll).
    ema_pkg_w: Option<f32>,

    /// Integrated discharge since the daemon started, in joules. Divided
    /// by 3600 on read to give Wh.
    session_discharge_j: f64,

    /// In-progress aggregation for the next history point. Energy in
    /// joules / wall time in seconds = average wattage over the bucket.
    bucket_started_at: Option<Instant>,
    bucket_battery_j: f64,
    bucket_battery_dt_s: f32,
    bucket_discharging: bool,

    /// Charge-state and AC status are read inside `sample()` and stashed
    /// for `build_info()` so we don't double-hit sysfs on the same tick.
    cached_charge_state: ChargeState,
    cached_on_ac: bool,
    /// Was the laptop on AC at the previous tick? Used to detect the
    /// AC → battery transition so we can stamp `on_battery_since`.
    prev_on_ac: Option<bool>,
    /// Unix timestamp of the most recent AC-unplug. None when on AC, or
    /// when the daemon has never seen an AC transition and no persisted
    /// state is available. Persisted to disk so daemon restarts during a
    /// battery session don't reset the elapsed counter.
    on_battery_since_unix: Option<u64>,

    ring: Vec<EnergySample>,
    /// Index of the next slot to overwrite once `ring` reaches capacity.
    /// Meaningless while `ring.len() < RING_CAPACITY`.
    ring_head: usize,
}

impl Default for EnergyMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl EnergyMeter {
    pub fn new() -> Self {
        let (pkg_path, pkg_max_uj) = unzip_or_default(discover_rapl_zone("package-0"));
        let (psys_path, psys_max_uj) = unzip_or_default(discover_rapl_zone("psys"));
        let bat_path = discover_battery();
        let ac_online_path = discover_ac();

        if pkg_path.is_none() {
            log::info!("RAPL package-0 zone not found — pkg_w will be unavailable");
        }
        if psys_path.is_none() {
            log::debug!("RAPL psys zone not found — psys_w will be unavailable");
        }
        if bat_path.is_none() {
            log::info!("no battery found under /sys/class/power_supply/ — battery_w will be unavailable");
        }

        Self {
            last_sample_at: None,
            prev_pkg_uj: None,
            prev_psys_uj: None,
            pkg_max_uj,
            psys_max_uj,
            pkg_path,
            psys_path,
            bat_path,
            ac_online_path,
            last_battery_w: None,
            last_pkg_w: None,
            last_psys_w: None,
            recent_battery_w: VecDeque::with_capacity(TIME_WINDOW_TICKS),
            ema_pkg_w: None,
            session_discharge_j: 0.0,
            bucket_started_at: None,
            bucket_battery_j: 0.0,
            bucket_battery_dt_s: 0.0,
            bucket_discharging: false,
            cached_charge_state: ChargeState::Unknown,
            cached_on_ac: false,
            prev_on_ac: None,
            on_battery_since_unix: load_on_battery_since(),
            ring: Vec::with_capacity(RING_CAPACITY),
            ring_head: 0,
        }
    }

    /// Latest CPU package wattage (smoothed). Used by [`crate::daemon::State`]
    /// to scale per-scope CPU% into an "≈ W per app" estimate.
    pub fn current_pkg_w(&self) -> Option<f32> {
        self.ema_pkg_w
    }

    /// Read all sources, update EMAs, integrate Wh, push a sample into the
    /// ring. Called from the main loop's 1 Hz tick. Cheap: ~5 sysfs reads.
    pub fn sample(&mut self) {
        let now = Instant::now();
        let dt_s = self
            .last_sample_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);

        let pkg_w = sample_rapl_zone(
            self.pkg_path.as_deref(),
            self.pkg_max_uj,
            &mut self.prev_pkg_uj,
            dt_s,
        );
        let psys_w = sample_rapl_zone(
            self.psys_path.as_deref(),
            self.psys_max_uj,
            &mut self.prev_psys_uj,
            dt_s,
        );

        let battery_w_instant = self.sample_battery_w();
        let on_ac = self.read_ac_online();
        let charge_state = self.read_charge_state();

        // Cards display raw "now" wattage. Moving-average window only
        // feeds the time-remaining estimate.
        if let Some(w) = battery_w_instant {
            self.last_battery_w = Some(w);
            self.recent_battery_w.push_back(w);
            if self.recent_battery_w.len() > TIME_WINDOW_TICKS {
                self.recent_battery_w.pop_front();
            }
        }
        if let Some(w) = pkg_w {
            self.last_pkg_w = Some(w);
            self.ema_pkg_w = Some(match self.ema_pkg_w {
                Some(prev) => ema_step(prev, w, dt_s, PKG_EMA_TAU_S),
                None => w,
            });
        }

        // Integrate session Wh only when actually discharging. "0.4 Wh used
        // while AC is plugged in" is meaningless and user-confusing.
        if matches!(charge_state, ChargeState::Discharging) && dt_s > 0.0 {
            if let Some(w) = battery_w_instant {
                self.session_discharge_j += (w as f64) * (dt_s as f64);
            }
        }

        // Latest raw psys for the live card. Bucketed history doesn't
        // need it — the graph plots %, not watts.
        if let Some(w) = psys_w {
            self.last_psys_w = Some(w);
        }
        let _ = pkg_w; // already folded into ema_pkg_w above

        // Aggregate into the current history bucket. A bucket is "mostly
        // discharging" if any tick during it saw the discharging state —
        // mixing charge/discharge inside one 10 s window is rare enough
        // that surfacing the discharge half is the user-visible signal.
        if self.bucket_started_at.is_none() {
            self.bucket_started_at = Some(now);
        }
        if let Some(w) = battery_w_instant {
            self.bucket_battery_j += (w as f64) * (dt_s as f64);
            self.bucket_battery_dt_s += dt_s;
        }
        if matches!(charge_state, ChargeState::Discharging) {
            self.bucket_discharging = true;
        }

        // Close out the bucket when ≥ HISTORY_BUCKET_S has elapsed.
        let close_bucket = self
            .bucket_started_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32() >= HISTORY_BUCKET_S)
            .unwrap_or(false);
        if close_bucket {
            let avg_w = if self.bucket_battery_dt_s > 0.0 {
                Some((self.bucket_battery_j / self.bucket_battery_dt_s as f64) as f32)
            } else {
                None
            };
            push_ring(
                &mut self.ring,
                &mut self.ring_head,
                EnergySample {
                    age_s: 0.0,
                    capacity_pct: read_capacity_pct(self.bat_path.as_deref()),
                    discharging: self.bucket_discharging,
                    battery_w: avg_w,
                },
            );
            self.bucket_started_at = Some(now);
            self.bucket_battery_j = 0.0;
            self.bucket_battery_dt_s = 0.0;
            self.bucket_discharging = false;
        }

        // Detect AC <-> battery transitions for the "on battery for"
        // counter. Skip the very first tick — prev_on_ac is None then, so
        // there's no transition to mark. After the first tick:
        //  - AC → battery: stamp the unplug moment, persist to disk.
        //  - battery → AC: clear the stamp.
        //  - first tick already on battery and no persisted timestamp:
        //    stamp "now" as a conservative best-effort start.
        if let Some(prev) = self.prev_on_ac {
            if prev && !on_ac {
                let ts = now_unix();
                self.on_battery_since_unix = Some(ts);
                save_on_battery_since(Some(ts));
            } else if !prev && on_ac {
                self.on_battery_since_unix = None;
                save_on_battery_since(None);
            }
        } else if !on_ac && self.on_battery_since_unix.is_none() {
            // Daemon started while already on battery, with no persisted
            // unplug timestamp: best we can do is record "since now".
            // Subsequent restarts will keep using this until the next plug.
            let ts = now_unix();
            self.on_battery_since_unix = Some(ts);
            save_on_battery_since(Some(ts));
        }
        self.prev_on_ac = Some(on_ac);

        self.last_sample_at = Some(now);
        self.cached_charge_state = charge_state;
        self.cached_on_ac = on_ac;
    }

    /// Build an [`EnergyInfo`] for the IPC reply.
    pub fn build_info(&self) -> EnergyInfo {
        let samples = ring_as_oldest_first(&self.ring, self.ring_head);

        let capacity_pct = battery::read().and_then(|b| b.capacity_pct);

        // Cards show raw "now" values — smoothing here hid brightness
        // changes and caused early-seed convergence bugs.
        let battery_now = self.last_battery_w;
        let pkg_now = self.last_pkg_w;
        let psys_now = self.last_psys_w;

        let time_remaining_s = self.estimate_time_remaining_s();

        EnergyInfo {
            battery_w: battery_now,
            pkg_w: pkg_now,
            psys_w: psys_now,
            capacity_pct,
            charge_state: charge_state_str(self.cached_charge_state).to_string(),
            on_ac: self.cached_on_ac,
            time_remaining_s,
            session_discharge_wh: (self.session_discharge_j / 3600.0) as f32,
            on_battery_since_unix: self.on_battery_since_unix,
            samples,
        }
    }


    fn sample_battery_w(&self) -> Option<f32> {
        let bat = self.bat_path.as_ref()?;
        let v_uv = read_u64(&bat.join("voltage_now"))?;
        let i_ua = read_u64(&bat.join("current_now"))?;
        // µV × µA = pW; /1e12 → W. Magnitude only — direction is taken
        // from `status` because some drivers always report positive
        // current_now regardless of charge/discharge.
        let w = (v_uv as f64) * (i_ua as f64) / 1e12;
        if w.is_finite() && (0.0..=200.0).contains(&w) {
            Some(w as f32)
        } else {
            None
        }
    }

    fn read_ac_online(&self) -> bool {
        match self.ac_online_path.as_ref() {
            Some(p) => matches!(
                fs::read_to_string(p).ok().as_deref().map(str::trim),
                Some("1")
            ),
            None => false,
        }
    }

    fn read_charge_state(&self) -> ChargeState {
        match self.bat_path.as_ref() {
            Some(p) => fs::read_to_string(p.join("status"))
                .map(|s| ChargeState::parse_str(&s))
                .unwrap_or(ChargeState::Unknown),
            None => ChargeState::Unknown,
        }
    }

    fn estimate_time_remaining_s(&self) -> Option<u32> {
        // Mean of the last ≤60 raw battery_w samples. Wait until we have
        // a meaningful sample count — projecting from 2 readings will
        // bounce around for the first ten seconds.
        if self.recent_battery_w.len() < 10 {
            return None;
        }
        let sum: f32 = self.recent_battery_w.iter().sum();
        let w_avg = sum / self.recent_battery_w.len() as f32;
        if w_avg < 0.5 {
            // Below the noise floor — projecting "infinity hours" is
            // worse than no projection at all.
            return None;
        }
        let bat = self.bat_path.as_ref()?;
        let charge_now = read_u64(&bat.join("charge_now"))? as f64; // µAh
        let charge_full = read_u64(&bat.join("charge_full"))? as f64;
        let voltage = read_u64(&bat.join("voltage_now"))? as f64 / 1e6;
        if voltage <= 0.0 || charge_full <= 0.0 {
            return None;
        }
        match self.cached_charge_state {
            ChargeState::Discharging => {
                // remaining energy in Wh = charge_now (µAh) × V (V) / 1e6
                let wh = charge_now * voltage / 1e6;
                let hours = wh / w_avg as f64;
                clamp_seconds(hours * 3600.0)
            }
            ChargeState::Charging => {
                let missing_ah = (charge_full - charge_now).max(0.0);
                let wh = missing_ah * voltage / 1e6;
                let hours = wh / w_avg as f64;
                clamp_seconds(hours * 3600.0)
            }
            _ => None,
        }
    }
}

fn sample_rapl_zone(
    path: Option<&Path>,
    max_uj: u64,
    prev_slot: &mut Option<u64>,
    dt_s: f32,
) -> Option<f32> {
    let path = path?;
    let cur = fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()?;
    if dt_s <= 0.0 {
        // First call after construction (or after a wallclock anomaly):
        // seed the previous reading so the next call has a baseline.
        *prev_slot = Some(cur);
        return None;
    }
    let result = if let Some(prev) = *prev_slot {
        let delta_uj = if cur >= prev {
            cur - prev
        } else if max_uj > 0 {
            // Counter wrap. Sub-second polling at <100 W will never see it
            // on a healthy chip, but firmware bugs occasionally reset the
            // counter mid-run; the wraparound math is still cheap.
            (max_uj - prev).saturating_add(cur)
        } else {
            *prev_slot = Some(cur);
            return None;
        };
        let w = (delta_uj as f64 / 1_000_000.0 / dt_s as f64) as f32;
        // Cap at 200 W per series — a bogus wrap result of 4 GW would
        // otherwise wreck the auto-scaled plot Y axis for 10 minutes.
        if w.is_finite() && (0.0..=200.0).contains(&w) {
            Some(w)
        } else {
            None
        }
    } else {
        None
    };
    *prev_slot = Some(cur);
    result
}

fn ema_step(prev: f32, new: f32, dt_s: f32, tau_s: f32) -> f32 {
    if dt_s <= 0.0 || tau_s <= 0.0 {
        return new;
    }
    let alpha = 1.0 - (-dt_s / tau_s).exp();
    prev + alpha * (new - prev)
}

fn push_ring(ring: &mut Vec<EnergySample>, head: &mut usize, sample: EnergySample) {
    if ring.len() < RING_CAPACITY {
        ring.push(sample);
        *head = ring.len() % RING_CAPACITY;
    } else {
        ring[*head] = sample;
        *head = (*head + 1) % RING_CAPACITY;
    }
}

fn ring_as_oldest_first(ring: &[EnergySample], head: usize) -> Vec<EnergySample> {
    // Flatten the ring oldest→newest and stamp each entry with seconds-ago
    // at the moment of the snapshot. We don't store per-sample timestamps;
    // the sampler closes a bucket every HISTORY_BUCKET_S, so position from
    // newest × bucket length is the age in seconds.
    if ring.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<EnergySample> = Vec::with_capacity(ring.len());
    if ring.len() < RING_CAPACITY {
        out.extend_from_slice(ring);
    } else {
        out.extend_from_slice(&ring[head..]);
        out.extend_from_slice(&ring[..head]);
    }
    let n = out.len();
    for (i, s) in out.iter_mut().enumerate() {
        s.age_s = (n - 1 - i) as f32 * HISTORY_BUCKET_S;
    }
    out
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Read battery percentage as a float — `charge_now / charge_full × 100`.
/// Falls back to the integer `capacity` file if the charge counters aren't
/// available (some non-laptop "battery"-type devices). Float resolution
/// matters because at low discharge rates the integer capacity stays put
/// for whole minutes and the discharge graph would render as a staircase.
fn read_capacity_pct(bat: Option<&Path>) -> Option<f32> {
    let bat = bat?;
    if let (Some(now), Some(full)) = (
        read_u64(&bat.join("charge_now")),
        read_u64(&bat.join("charge_full")),
    ) {
        if full > 0 {
            return Some((now as f64 / full as f64 * 100.0) as f32);
        }
    }
    // energy_now/energy_full is the alternative pair on some drivers.
    if let (Some(now), Some(full)) = (
        read_u64(&bat.join("energy_now")),
        read_u64(&bat.join("energy_full")),
    ) {
        if full > 0 {
            return Some((now as f64 / full as f64 * 100.0) as f32);
        }
    }
    read_u64(&bat.join("capacity")).map(|v| v as f32)
}

fn discover_battery() -> Option<PathBuf> {
    let root = Path::new("/sys/class/power_supply");
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        // Need both voltage_now and current_now for the W = V×I calc;
        // peripheral "batteries" (UPSes, HID devices) often expose only one.
        if path.join("voltage_now").exists() && path.join("current_now").exists() {
            return Some(path);
        }
    }
    None
}

fn discover_ac() -> Option<PathBuf> {
    let root = Path::new("/sys/class/power_supply");
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
        let kind = kind.trim();
        if kind == "Mains" || kind == "USB" {
            let online = path.join("online");
            if online.exists() {
                return Some(online);
            }
        }
    }
    None
}

/// Walk /sys/class/powercap/intel-rapl:* looking for a zone whose `name`
/// matches. Returns the energy_uj path plus its wrap value.
fn discover_rapl_zone(want_name: &str) -> Option<(PathBuf, u64)> {
    let root = Path::new("/sys/class/powercap");
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let stem = path.file_name()?.to_str()?;
        if !stem.starts_with("intel-rapl:") {
            continue;
        }
        // Skip the MMIO duplicate zone — same energy_uj as package-0 but
        // also named "package-0", would otherwise match twice.
        if stem.starts_with("intel-rapl-mmio") {
            continue;
        }
        let name = fs::read_to_string(path.join("name")).unwrap_or_default();
        if name.trim() != want_name {
            continue;
        }
        let energy_path = path.join("energy_uj");
        if !energy_path.exists() {
            continue;
        }
        let max = read_u64(&path.join("max_energy_range_uj")).unwrap_or(0);
        return Some((energy_path, max));
    }
    None
}

fn charge_state_str(s: ChargeState) -> &'static str {
    match s {
        ChargeState::Charging => "charging",
        ChargeState::Discharging => "discharging",
        ChargeState::Full => "full",
        ChargeState::NotCharging => "not_charging",
        ChargeState::Unknown => "unknown",
    }
}

fn clamp_seconds(s: f64) -> Option<u32> {
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    // > 99 hours is functionally "n/a" — usually means the wattage estimate
    // is near zero (battery just unplugged, AC just plugged in) and the
    // EMA hasn't ramped yet.
    if s > 99.0 * 3600.0 {
        return None;
    }
    Some(s.round() as u32)
}

fn unzip_or_default<A, B: Default>(opt: Option<(A, B)>) -> (Option<A>, B) {
    match opt {
        Some((a, b)) => (Some(a), b),
        None => (None, B::default()),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/niri-battery-keeper/runtime.toml` — small persisted
/// state for things that need to survive a daemon restart. Currently
/// holds the on-battery-since timestamp.
fn runtime_state_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("niri-battery-keeper").join("runtime.toml"))
}

fn load_on_battery_since() -> Option<u64> {
    let path = runtime_state_path()?;
    let text = fs::read_to_string(&path).ok()?;
    let table: toml::Table = text.parse().ok()?;
    table
        .get("on_battery_since_unix")
        .and_then(|v| v.as_integer())
        .and_then(|v| if v >= 0 { Some(v as u64) } else { None })
}

fn save_on_battery_since(ts: Option<u64>) {
    let Some(path) = runtime_state_path() else { return };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log::warn!("runtime state dir {}: {e}", parent.display());
            return;
        }
    }
    // Single key — keep it as plain text so a hand-edit doesn't need a
    // TOML parser. Format chosen so future fields can be appended without
    // a migration.
    let body = match ts {
        Some(t) => format!("on_battery_since_unix = {t}\n"),
        None => String::new(),
    };
    // Write through a tmp file + rename for atomic update — half-written
    // state on a power-loss crash is the one thing that would defeat the
    // whole point of persisting.
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = fs::write(&tmp, &body) {
        log::warn!("write runtime state to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        log::warn!("rename runtime state to {}: {e}", path.display());
    }
}
