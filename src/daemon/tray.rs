//! StatusNotifierItem tray icon for the daemon. Surfaces battery %, current
//! mode, kill-switch state; lets the user switch modes, toggle the kill
//! switch, or open the GUI without leaving their statusbar.
//!
//! Lives in the daemon (the long-lived process); the GUI gets spawned on
//! demand via `/proc/self/exe`. Runs on its own thread inside `ksni`'s
//! blocking runtime so the rest of the daemon stays sync.

use std::sync::mpsc::Sender;

use ksni::blocking::{Handle, TrayMethods};

use crate::daemon::battery::{BatteryInfo, ChargeState};

/// Messages flowing tray → daemon. Mirrors a subset of `proto::Request` but
/// kept separate so the menu callbacks don't need to know IPC details.
#[derive(Debug, Clone)]
pub enum TrayAction {
    SetMode(String),
    SetDisabled(bool),
    OpenGui,
}

/// State backing the tray UI. Mutated by `daemon::run` via `Handle::update`;
/// menu callbacks mutate it too (e.g. an optimistic check-toggle) and
/// dispatch the real work over `action_tx`.
pub struct TrayState {
    mode: String,
    modes: Vec<String>,
    disabled: bool,
    battery: Option<BatteryInfo>,
    action_tx: Sender<TrayAction>,
}

impl TrayState {
    pub fn new(
        mode: String,
        modes: Vec<String>,
        disabled: bool,
        battery: Option<BatteryInfo>,
        action_tx: Sender<TrayAction>,
    ) -> Self {
        Self { mode, modes, disabled, battery, action_tx }
    }

    pub fn set_mode(&mut self, mode: String) { self.mode = mode; }
    pub fn set_modes(&mut self, modes: Vec<String>) { self.modes = modes; }
    pub fn set_disabled(&mut self, disabled: bool) { self.disabled = disabled; }
    pub fn set_battery(&mut self, battery: Option<BatteryInfo>) { self.battery = battery; }

    fn tooltip(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(3);
        if let Some(b) = &self.battery {
            let glyph = match b.charge_state {
                ChargeState::Charging => "⚡",
                ChargeState::Discharging => "🔋",
                ChargeState::Full => "✓",
                _ => "",
            };
            let pct = b.capacity_pct.map(|p| format!("{p}%")).unwrap_or_default();
            parts.push(format!("{glyph} {pct}").trim().to_string());
        }
        parts.push(format!("Mode: {}", self.mode));
        if self.disabled {
            parts.push("Throttling OFF".into());
        }
        parts.join(" · ")
    }
}

impl ksni::Tray for TrayState {
    fn id(&self) -> String {
        "niri-battery-keeper".into()
    }

    fn title(&self) -> String {
        "Niri Battery Keeper".into()
    }

    fn icon_name(&self) -> String {
        // Resolved by the desktop icon-theme machinery against the SVG we
        // ship at /usr/share/icons/hicolor/scalable/apps/. On bare-binary
        // installs the GUI's "Install service" path drops the same SVG
        // under ~/.local/share/icons/hicolor/.
        "niri-battery-keeper".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            title: "Niri Battery Keeper".into(),
            description: self.tooltip(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.action_tx.send(TrayAction::OpenGui);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let selected = self
            .modes
            .iter()
            .position(|m| m == &self.mode)
            .unwrap_or(0);

        let mode_options: Vec<RadioItem> = self
            .modes
            .iter()
            .map(|m| RadioItem { label: m.clone(), ..Default::default() })
            .collect();

        let modes_snapshot = self.modes.clone();
        let mode_radio = RadioGroup {
            selected,
            select: Box::new(move |this: &mut Self, idx| {
                if let Some(name) = modes_snapshot.get(idx).cloned() {
                    this.mode = name.clone();
                    let _ = this.action_tx.send(TrayAction::SetMode(name));
                }
            }),
            options: mode_options,
            ..Default::default()
        };

        vec![
            StandardItem {
                label: "Open Niri Battery Keeper".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.action_tx.send(TrayAction::OpenGui);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Mode".into(),
                submenu: vec![mode_radio.into()],
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Throttling enabled".into(),
                checked: !self.disabled,
                activate: Box::new(|this: &mut Self| {
                    let new_disabled = !this.disabled;
                    this.disabled = new_disabled;
                    let _ = this.action_tx.send(TrayAction::SetDisabled(new_disabled));
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray service on its own internal thread. Returns the handle for
/// pushing updates and an mpsc receiver carrying user-driven actions. On
/// systems without a D-Bus session (TTY-only, headless) this fails — caller
/// should log and continue without a tray.
pub fn spawn(
    mode: String,
    modes: Vec<String>,
    disabled: bool,
    battery: Option<BatteryInfo>,
) -> Result<(Handle<TrayState>, std::sync::mpsc::Receiver<TrayAction>), Box<dyn std::error::Error>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let tray = TrayState::new(mode, modes, disabled, battery, tx);
    let handle = tray.spawn()?;
    Ok((handle, rx))
}
