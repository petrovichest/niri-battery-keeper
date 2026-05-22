//! Power and energy sampler. Polled by the main daemon loop ~1 Hz alongside
//! the existing 30 s battery tick. Gives the GUI three concurrent series for
//! a rolling timeline (battery W, CPU package W, platform W), a smoothed
//! time-remaining estimate, and per-cycle Wh integrals.
//!
//! State that the user cares about — discharge/charge counters, AC-since /
//! battery-since timestamps, the full sample timeline — is persisted to
//! `$XDG_DATA_HOME/niri-battery-keeper/battery_session.json` so a daemon
//! restart mid-session keeps the same numbers and the same graph.
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

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::daemon::battery::{self, ChargeState};
use crate::proto::{EnergyInfo, EnergySample};

/// Bucket length — one history point every 10 s. Trade-off between data
/// volume (a long discharge session writes a lot of buckets) and
/// resolution (smaller buckets show short workload bursts).
const HISTORY_BUCKET_S: f32 = 10.0;

/// Hard cap on the sample buffer. 24 h × 360 buckets/h = 8 640 samples.
/// JSON encoding is ~50 bytes/sample → ~450 KB worst case. Older samples
/// roll off the front when the cap is exceeded — the GUI graph shows the
/// last 24 h bucketed by hour, so anything older isn't useful anyway.
const MAX_SAMPLES: usize = 8_640;

/// Save persisted state every Nth bucket close. At 10 s buckets that's a
/// disk write every minute. A crash loses at most ~60 s of samples.
const SAVE_EVERY_N_BUCKETS: u32 = 6;

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

/// Per-tick dt larger than this is treated as a suspend gap rather than
/// the laptop being awake. We shift the on-bat/on-ac timestamps forward
/// by the excess so the displayed elapsed time only reflects awake work.
/// Normal ticks are ~1 s; the main loop occasionally stalls a bit, so
/// the cap is loose enough to survive a sluggish tick but tight enough
/// to exclude a real suspend (always dozens of seconds at minimum).
const SUSPEND_GAP_THRESHOLD_S: f32 = 10.0;

/// Stored form of one history point. We keep wall-clock `at_unix` so the
/// age field on the IPC sample can be derived honestly from `now - at`,
/// surviving daemon restarts and the small wall-time gap they introduce.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSample {
    at_unix: u64,
    #[serde(default)]
    capacity_pct: Option<f32>,
    #[serde(default)]
    discharging: bool,
    #[serde(default)]
    charging: bool,
    #[serde(default)]
    battery_w: Option<f32>,
}

/// Everything that needs to survive a daemon restart. Written atomically
/// to `battery_session.json` every minute and on state transitions.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    on_battery_since_unix: Option<u64>,
    #[serde(default)]
    on_ac_since_unix: Option<u64>,
    #[serde(default)]
    session_discharge_j: f64,
    #[serde(default)]
    session_charge_j: f64,
    #[serde(default)]
    samples: Vec<StoredSample>,
}

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

    /// Joules out of the battery during the current discharge cycle. Resets
    /// the instant AC is plugged in (so "Session used" reads as "this
    /// discharge cycle", not a lifetime total). Persisted.
    session_discharge_j: f64,
    /// Joules into the battery during the current charging cycle. Mirror
    /// of the discharge counter: resets on unplug, persisted.
    session_charge_j: f64,

    /// In-progress aggregation for the next history point. Energy in
    /// joules / wall time in seconds = average wattage over the bucket.
    bucket_started_at: Option<Instant>,
    bucket_battery_j: f64,
    bucket_battery_dt_s: f32,
    bucket_discharging: bool,
    bucket_charging: bool,
    /// Sample count modulo `SAVE_EVERY_N_BUCKETS`. We persist when this
    /// hits zero (and on transitions). Avoids rewriting the whole JSON
    /// blob every 10 s.
    buckets_since_save: u32,

    /// Charge-state and AC status are read inside `sample()` and stashed
    /// for `build_info()` so we don't double-hit sysfs on the same tick.
    cached_charge_state: ChargeState,
    cached_on_ac: bool,
    /// Was the laptop on AC at the previous tick? Used to detect the
    /// AC ↔ battery transitions that drive the symmetric counters.
    prev_on_ac: Option<bool>,
    /// Unix timestamp of the most recent AC-unplug. None when on AC, or
    /// when the daemon has never seen an AC transition and no persisted
    /// state is available.
    on_battery_since_unix: Option<u64>,
    /// Unix timestamp of the most recent AC-plug. Mirror of the above.
    on_ac_since_unix: Option<u64>,

    /// Rolling sample timeline. Oldest first; new samples appended to the
    /// back; once the buffer exceeds `MAX_SAMPLES` the oldest entries are
    /// dropped from the front.
    samples: Vec<StoredSample>,
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

        let persisted = load_persisted();

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
            session_discharge_j: persisted.session_discharge_j,
            session_charge_j: persisted.session_charge_j,
            bucket_started_at: None,
            bucket_battery_j: 0.0,
            bucket_battery_dt_s: 0.0,
            bucket_discharging: false,
            bucket_charging: false,
            buckets_since_save: 0,
            cached_charge_state: ChargeState::Unknown,
            cached_on_ac: false,
            prev_on_ac: None,
            on_battery_since_unix: persisted.on_battery_since_unix,
            on_ac_since_unix: persisted.on_ac_since_unix,
            samples: persisted.samples,
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

        // Suspend handling: if the gap since the previous tick is bigger
        // than a normal poll interval, the laptop was asleep — push the
        // on-bat/on-ac anchors forward so `now - since` excludes the
        // sleep window. Without this the counter reads wall-clock since
        // unplug, which on a laptop that's spent the night closed is off
        // by hours.
        if dt_s > SUSPEND_GAP_THRESHOLD_S {
            let skip = (dt_s - 1.0) as u64;
            if let Some(ts) = self.on_battery_since_unix.as_mut() {
                *ts = ts.saturating_add(skip);
            }
            if let Some(ts) = self.on_ac_since_unix.as_mut() {
                *ts = ts.saturating_add(skip);
            }
        }

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

        // Per-cycle Wh counters: discharge while the kernel reports
        // Discharging, charge while it reports Charging. Idle-on-AC and
        // Full both leave both counters alone (no flow either way).
        if dt_s > 0.0 {
            if let Some(w) = battery_w_instant {
                match charge_state {
                    ChargeState::Discharging => {
                        self.session_discharge_j += (w as f64) * (dt_s as f64);
                    }
                    ChargeState::Charging => {
                        self.session_charge_j += (w as f64) * (dt_s as f64);
                    }
                    _ => {}
                }
            }
        }

        // Latest raw psys for the live card. Bucketed history doesn't
        // need it — the graph plots %, not watts.
        if let Some(w) = psys_w {
            self.last_psys_w = Some(w);
        }
        let _ = pkg_w; // already folded into ema_pkg_w above

        // Aggregate into the current history bucket. A bucket is "mostly
        // discharging/charging" if any tick during it saw that state —
        // mixing inside one 10 s window is rare enough that surfacing
        // the active flow is the user-visible signal.
        if self.bucket_started_at.is_none() {
            self.bucket_started_at = Some(now);
        }
        if let Some(w) = battery_w_instant {
            self.bucket_battery_j += (w as f64) * (dt_s as f64);
            self.bucket_battery_dt_s += dt_s;
        }
        match charge_state {
            ChargeState::Discharging => self.bucket_discharging = true,
            ChargeState::Charging => self.bucket_charging = true,
            _ => {}
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
            self.samples.push(StoredSample {
                at_unix: now_unix(),
                capacity_pct: read_capacity_pct(self.bat_path.as_deref()),
                discharging: self.bucket_discharging,
                charging: self.bucket_charging,
                battery_w: avg_w,
            });
            // Drop from the front if we've blown the cap. Single drain
            // is cheaper than repeated remove(0) for the >1 case (e.g.
            // a long-paused laptop catching up after wakeup).
            if self.samples.len() > MAX_SAMPLES {
                let excess = self.samples.len() - MAX_SAMPLES;
                self.samples.drain(0..excess);
            }
            self.bucket_started_at = Some(now);
            self.bucket_battery_j = 0.0;
            self.bucket_battery_dt_s = 0.0;
            self.bucket_discharging = false;
            self.bucket_charging = false;

            self.buckets_since_save = self.buckets_since_save.wrapping_add(1);
            if self.buckets_since_save % SAVE_EVERY_N_BUCKETS == 0 {
                self.save_state();
            }
        }

        // Detect AC ↔ battery transitions for the symmetric counters and
        // timestamps. Skip the very first tick (prev_on_ac is None — no
        // transition to mark). After the first tick:
        //  - AC → battery: stamp the unplug moment, clear AC-since,
        //    reset discharge counter to start a fresh discharge cycle.
        //  - battery → AC: stamp the plug moment, clear battery-since,
        //    reset charge counter to start a fresh charging cycle.
        //  - first tick already on AC/battery with no persisted stamp:
        //    record "now" as a conservative best-effort start.
        let mut transition_happened = false;
        if let Some(prev) = self.prev_on_ac {
            if prev && !on_ac {
                let ts = now_unix();
                self.on_battery_since_unix = Some(ts);
                self.on_ac_since_unix = None;
                self.session_discharge_j = 0.0;
                transition_happened = true;
            } else if !prev && on_ac {
                let ts = now_unix();
                self.on_ac_since_unix = Some(ts);
                self.on_battery_since_unix = None;
                self.session_charge_j = 0.0;
                transition_happened = true;
            }
        } else {
            // First tick of the daemon. Backfill the relevant timestamp
            // if it's missing — gives the cards something to show before
            // the user has plugged or unplugged this session.
            let ts = now_unix();
            if on_ac && self.on_ac_since_unix.is_none() {
                self.on_ac_since_unix = Some(ts);
                transition_happened = true;
            }
            if !on_ac && self.on_battery_since_unix.is_none() {
                self.on_battery_since_unix = Some(ts);
                transition_happened = true;
            }
        }
        self.prev_on_ac = Some(on_ac);

        if transition_happened {
            self.save_state();
        }

        self.last_sample_at = Some(now);
        self.cached_charge_state = charge_state;
        self.cached_on_ac = on_ac;
    }

    /// Build an [`EnergyInfo`] for the IPC reply.
    pub fn build_info(&self) -> EnergyInfo {
        let samples = samples_for_ipc(&self.samples);

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
            session_charge_wh: (self.session_charge_j / 3600.0) as f32,
            on_battery_since_unix: self.on_battery_since_unix,
            on_ac_since_unix: self.on_ac_since_unix,
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

    fn save_state(&self) {
        let state = PersistedState {
            on_battery_since_unix: self.on_battery_since_unix,
            on_ac_since_unix: self.on_ac_since_unix,
            session_discharge_j: self.session_discharge_j,
            session_charge_j: self.session_charge_j,
            samples: self.samples.clone(),
        };
        save_persisted(&state);
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

/// Project the internal stored samples to the IPC sample type, computing
/// `age_s = now - at_unix` for each entry. Stays honest across daemon
/// restarts: the freshly-started daemon's "now" is a few seconds past the
/// last saved sample, so the gap shows up naturally in age space rather
/// than being papered over.
fn samples_for_ipc(stored: &[StoredSample]) -> Vec<EnergySample> {
    if stored.is_empty() {
        return Vec::new();
    }
    let now = now_unix();
    stored
        .iter()
        .map(|s| EnergySample {
            age_s: now.saturating_sub(s.at_unix) as f32,
            capacity_pct: s.capacity_pct,
            discharging: s.discharging,
            charging: s.charging,
            battery_w: s.battery_w,
        })
        .collect()
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

fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("niri-battery-keeper"))
}

fn session_state_path() -> Option<PathBuf> {
    Some(data_dir()?.join("battery_session.json"))
}

/// Old single-key TOML file we used in 0.3.x. Read at startup so users
/// upgrading from that version don't lose the on_battery_since timestamp.
/// Not written any more.
fn legacy_runtime_path() -> Option<PathBuf> {
    Some(data_dir()?.join("runtime.toml"))
}

fn load_persisted() -> PersistedState {
    if let Some(path) = session_state_path() {
        if let Ok(text) = fs::read_to_string(&path) {
            match serde_json::from_str::<PersistedState>(&text) {
                Ok(mut state) => {
                    // Defensive clamp: a hand-edited or corrupted file
                    // shouldn't blow up the in-memory buffer.
                    if state.samples.len() > MAX_SAMPLES {
                        let excess = state.samples.len() - MAX_SAMPLES;
                        state.samples.drain(0..excess);
                    }
                    return state;
                }
                Err(e) => {
                    log::warn!(
                        "battery session state at {} is unparseable ({e}); starting empty",
                        path.display()
                    );
                }
            }
        }
    }
    // Migration path: pre-0.4 stored only on_battery_since_unix in TOML.
    // Read that one field so a daemon upgrade doesn't reset the counter.
    if let Some(path) = legacy_runtime_path() {
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(table) = text.parse::<toml::Table>() {
                let on_battery_since_unix = table
                    .get("on_battery_since_unix")
                    .and_then(|v| v.as_integer())
                    .and_then(|v| if v >= 0 { Some(v as u64) } else { None });
                return PersistedState {
                    on_battery_since_unix,
                    ..Default::default()
                };
            }
        }
    }
    PersistedState::default()
}

fn save_persisted(state: &PersistedState) {
    let Some(path) = session_state_path() else { return };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log::warn!("battery session state dir {}: {e}", parent.display());
            return;
        }
    }
    let body = match serde_json::to_string(state) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("serialize battery session state: {e}");
            return;
        }
    };
    // tmp + rename for atomic update — half-written state on a
    // power-loss crash would defeat the whole point of persisting.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, body.as_bytes()) {
        log::warn!("write battery session state to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        log::warn!("rename battery session state to {}: {e}", path.display());
    }
}
