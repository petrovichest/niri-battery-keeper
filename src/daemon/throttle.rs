use std::collections::HashMap;
use std::process::Command;

use crate::config::Profile;

/// Tracks which units we've applied a profile to, so we know what to undo
/// on shutdown and can avoid redundant `systemctl` calls.
#[derive(Default)]
pub struct Throttler {
    /// unit -> last applied profile
    applied: HashMap<String, Profile>,
}

impl Throttler {
    pub fn new() -> Self { Self::default() }

    /// Apply `profile` to `unit`. Switches between throttle/freeze/none as needed.
    pub fn apply(&mut self, unit: &str, profile: &Profile) -> std::io::Result<()> {
        // No-op when nothing changes.
        if self.applied.get(unit) == Some(profile) {
            return Ok(());
        }

        // Tear down the previous action, if it differs in *kind* from the new one.
        let prev = self.applied.get(unit).cloned();
        match (&prev, profile) {
            (Some(Profile::Pause), Profile::Pause) => {}
            (Some(Profile::Pause), _) => thaw(unit)?,
            (Some(Profile::Throttle { .. }), Profile::Pause)
            | (Some(Profile::Throttle { .. }), Profile::None) => clear_properties(unit)?,
            _ => {}
        }

        // Apply the new action.
        match profile {
            Profile::None => {
                self.applied.remove(unit);
                log::debug!("throttle clear: unit={unit}");
                return Ok(());
            }
            Profile::Throttle { cpu_quota, cpu_weight, io_weight, allowed_cpus } => {
                set_throttle(unit, &cpu_quota.0, *cpu_weight, *io_weight, allowed_cpus.as_deref())?;
                log::debug!(
                    "throttle apply: unit={unit} cpu_quota={} cpu_weight={cpu_weight} io_weight={io_weight} allowed_cpus={}",
                    cpu_quota.0,
                    allowed_cpus.as_deref().unwrap_or("-")
                );
            }
            Profile::Pause => {
                freeze(unit)?;
                log::debug!("throttle pause: unit={unit}");
            }
        }
        self.applied.insert(unit.to_string(), profile.clone());
        Ok(())
    }

    /// Undo whatever we did to `unit`. Best-effort: errors are logged.
    pub fn reset(&mut self, unit: &str) {
        let prev = self.applied.remove(unit);
        let result = match prev {
            Some(Profile::Pause) => thaw(unit),
            Some(Profile::Throttle { .. }) => clear_properties(unit),
            Some(Profile::None) | None => return,
        };
        match result {
            Ok(()) => log::debug!("throttle reset: unit={unit}"),
            Err(e) => log::warn!("reset {unit} failed: {e}"),
        }
    }

    pub fn reset_all(&mut self) {
        let units: Vec<String> = self.applied.keys().cloned().collect();
        for u in units {
            self.reset(&u);
        }
    }

    pub fn throttled_units(&self) -> Vec<String> {
        self.applied.keys().cloned().collect()
    }

    pub fn is_throttled(&self, unit: &str) -> bool {
        self.applied.contains_key(unit)
    }

    /// Forcibly thaw + clear properties on `unit`, regardless of what we
    /// think is applied. Used at daemon startup to take a clean slate over
    /// scopes we're about to manage.
    pub fn force_clear(&mut self, unit: &str) {
        // Best-effort: errors are common here (unit may not be frozen, or we
        // never owned its properties). Log only on real surprises.
        let _ = thaw(unit);
        let _ = clear_properties(unit);
        self.applied.remove(unit);
    }
}

fn set_throttle(
    unit: &str,
    cpu_quota: &str,
    cpu_weight: u32,
    io_weight: u32,
    allowed_cpus: Option<&str>,
) -> std::io::Result<()> {
    // `AllowedCPUs=<list>` pins the scope to a cpuset (E-cores only, etc.);
    // an empty value tells systemd to unset, restoring "all CPUs".
    let cpus = allowed_cpus.map(str::trim).unwrap_or("");
    run_systemctl(&[
        "--user", "set-property", "--runtime", unit,
        &format!("CPUQuota={cpu_quota}"),
        &format!("CPUWeight={cpu_weight}"),
        &format!("IOWeight={io_weight}"),
        &format!("AllowedCPUs={cpus}"),
    ])
}

/// Empty values on the right of `=` tell systemd to unset the property.
fn clear_properties(unit: &str) -> std::io::Result<()> {
    run_systemctl(&[
        "--user", "set-property", "--runtime", unit,
        "CPUQuota=", "CPUWeight=", "IOWeight=", "AllowedCPUs=",
    ])
}

fn freeze(unit: &str) -> std::io::Result<()> {
    run_systemctl(&["--user", "freeze", unit])
}

fn thaw(unit: &str) -> std::io::Result<()> {
    run_systemctl(&["--user", "thaw", unit])
}

fn run_systemctl(args: &[&str]) -> std::io::Result<()> {
    let output = Command::new("systemctl").args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("systemctl {}: {}", args.join(" "), stderr.trim()),
        ));
    }
    Ok(())
}
