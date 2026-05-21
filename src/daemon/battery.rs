//! Read battery state from sysfs. Single sample, no caching — the daemon
//! calls this on a 30 s tick and on tray-relevant state changes. Returns
//! `None` on desktops where no `power_supply` device is a battery.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

impl ChargeState {
    fn parse(s: &str) -> Self {
        match s.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Full" => Self::Full,
            "Not charging" => Self::NotCharging,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryInfo {
    /// 0..=100 if reported.
    pub capacity_pct: Option<u8>,
    pub charge_state: ChargeState,
}

/// Walk `/sys/class/power_supply/` once, sum capacity across all Battery-type
/// devices, and pick the "most active" charge state (Charging > Discharging >
/// anything else). Returns `None` if no battery is found.
pub fn read() -> Option<BatteryInfo> {
    let root = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(root).ok()?;

    let mut count = 0u32;
    let mut sum_pct: u32 = 0;
    let mut best_state = ChargeState::Unknown;

    for entry in entries.flatten() {
        let path = entry.path();
        let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        // Some entries (HID peripherals, UPSes) advertise type=Battery but no
        // capacity; skip those.
        let Some(pct) = fs::read_to_string(path.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let state = fs::read_to_string(path.join("status"))
            .map(|s| ChargeState::parse(&s))
            .unwrap_or(ChargeState::Unknown);

        sum_pct += pct.min(100);
        count += 1;
        // Aggregate state: Charging wins over Discharging wins over Full.
        // Two batteries on one laptop with mixed status is unusual but
        // surfacing the busier one is what users expect.
        best_state = match (best_state, state) {
            (_, ChargeState::Charging) => ChargeState::Charging,
            (ChargeState::Charging, _) => ChargeState::Charging,
            (_, ChargeState::Discharging) => ChargeState::Discharging,
            (ChargeState::Discharging, _) => ChargeState::Discharging,
            (ChargeState::Unknown, s) => s,
            (s, _) => s,
        };
    }

    if count == 0 {
        return None;
    }
    Some(BatteryInfo {
        capacity_pct: Some((sum_pct / count) as u8),
        charge_state: best_state,
    })
}
