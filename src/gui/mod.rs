use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::config::{AppRule, Config, CpuQuota, Profile};
use crate::cputopo::{self, Topology};
use crate::proto::{
    client, CgroupLimits, DaemonState, Request, Response, SystemUnitCategory, SystemUnitInfo,
};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Niri Battery Keeper")
            // Without `with_app_id`, eframe doesn't call `WindowAttributesExtWayland::with_name`,
            // so the xdg_toplevel ships without an app_id and Niri reports our window as
            // `"app_id": null`. The daemon then lists its own GUI as an unnamed app.
            .with_app_id(crate::SELF_APP_ID)
            .with_inner_size([720.0, 720.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        crate::SELF_APP_ID,
        options,
        Box::new(|cc| {
            configure_ui(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
    .map_err(|e| -> Box<dyn std::error::Error> { format!("eframe: {e}").into() })
}

fn configure_ui(ctx: &egui::Context) {
    let state = load_persisted_state();
    let zoom = state.zoom_factor.unwrap_or_else(|| detect_native_zoom(ctx));
    if (zoom - 1.0).abs() > f32::EPSILON {
        ctx.set_zoom_factor(zoom);
    }
    install_symbol_font_fallbacks(ctx);
}

/// egui's bundled fonts (Ubuntu-Light + emoji-icon-font + NotoEmoji) miss
/// several Geometric Shapes glyphs we use in labels (e.g. U+25B0 ▰, U+25B1 ▱,
/// U+25B8 ▸), so they render as tofu. Append the first available system symbol
/// fonts to both font families' fallback chains. Silent no-op if none found.
fn install_symbol_font_fallbacks(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Some(name) = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned())
        else {
            continue;
        };
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes));
        added.push(name);
    }
    if added.is_empty() {
        return;
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let chain = fonts.families.entry(family).or_default();
        for name in &added {
            if !chain.iter().any(|n| n == name) {
                chain.push(name.clone());
            }
        }
    }
    ctx.set_fonts(fonts);
}

/// Multiplier applied to every scroll delta (mouse wheel + touchpad).
/// Touchpad events come through egui as raw pixel deltas (`MouseWheelUnit::Point`)
/// and bypass `Options::line_scroll_speed`, so the only way to make touchpad
/// scrolling feel system-native is to scale the already-accumulated delta.
const SCROLL_BOOST: f32 = 4.0;

fn detect_native_zoom(ctx: &egui::Context) -> f32 {
    ctx.native_pixels_per_point()
        .filter(|p| (*p - 1.0).abs() > 0.05 && (0.5..8.0).contains(p))
        .unwrap_or(1.0)
}

fn gui_state_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            home.join(".config")
        });
    base.join("niri-battery-keeper").join("gui_state.toml")
}

#[derive(Clone, Default)]
struct PersistedGuiState {
    zoom_factor: Option<f32>,
    collapsed_orphans: bool,
    collapsed_protected: bool,
}

fn load_persisted_state() -> PersistedGuiState {
    let Ok(text) = std::fs::read_to_string(gui_state_path()) else {
        return PersistedGuiState::default();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return PersistedGuiState::default();
    };
    PersistedGuiState {
        zoom_factor: table
            .get("zoom_factor")
            .and_then(|v| v.as_float())
            .map(|v| v as f32)
            .filter(|v| (0.5..8.0).contains(v)),
        collapsed_orphans: table
            .get("collapsed_orphans")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        collapsed_protected: table
            .get("collapsed_protected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn save_persisted_state(s: &PersistedGuiState) -> std::io::Result<()> {
    let path = gui_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut table = toml::Table::new();
    if let Some(z) = s.zoom_factor {
        table.insert("zoom_factor".into(), toml::Value::Float(z as f64));
    }
    table.insert(
        "collapsed_orphans".into(),
        toml::Value::Boolean(s.collapsed_orphans),
    );
    table.insert(
        "collapsed_protected".into(),
        toml::Value::Boolean(s.collapsed_protected),
    );
    std::fs::write(path, toml::to_string_pretty(&table).unwrap_or_default())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Apps,
    Presets,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetEditMode {
    Simple,
    Advanced,
}

struct App {
    /// Last state received from daemon (read-only baseline).
    server: Option<DaemonState>,
    /// Local edited copy. None until first successful poll.
    draft: Option<DaemonState>,
    last_poll: Instant,
    error: Option<String>,
    view: View,
    preset_edit_mode: PresetEditMode,
    /// In-progress rename buffers for the preset editor, keyed by the current
    /// canonical preset name. Only entries actively being edited live here.
    preset_name_drafts: BTreeMap<String, String>,
    scale_settled: bool,
    persisted: PersistedGuiState,
    /// Cached result of [`crate::bootstrap::is_installed`]. We refresh this
    /// alongside the daemon poll so the install card disappears once the
    /// unit file lands.
    service_installed: bool,
    /// Outcome of the last "Install service" click, surfaced inline in the
    /// install card. `None` until the user clicks the button at least once.
    install_status: Option<Result<String, String>>,
    /// Modal flag: is the "Remove service?" confirmation window open?
    show_remove_service_confirm: bool,
}

impl App {
    fn new() -> Self {
        let persisted = load_persisted_state();
        let mut me = Self {
            server: None,
            draft: None,
            last_poll: Instant::now() - Duration::from_secs(10),
            error: None,
            view: View::Apps,
            preset_edit_mode: PresetEditMode::Simple,
            preset_name_drafts: BTreeMap::new(),
            scale_settled: false,
            persisted,
            service_installed: crate::bootstrap::is_installed(),
            install_status: None,
            show_remove_service_confirm: false,
        };
        me.poll();
        me
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        // Cheap stat; refreshed each poll so the install card disappears
        // shortly after a successful install (or the unit being removed
        // externally).
        self.service_installed = crate::bootstrap::is_installed();
        match client::send(&Request::GetState) {
            Ok(Response::State(st)) => {
                self.error = None;
                // Preserve draft if user has unsaved changes; otherwise sync.
                if self.is_dirty() {
                    // Keep draft but refresh window list and runtime flags from server.
                    if let Some(draft) = &mut self.draft {
                        sync_runtime_into_draft(draft, &st);
                    }
                } else {
                    self.draft = Some(st.clone());
                }
                self.server = Some(st);
            }
            Ok(Response::Error { message }) => {
                self.error = Some(message);
            }
            Ok(_) => {}
            Err(e) => {
                self.error = Some(format!("daemon not reachable: {e}"));
            }
        }
    }

    fn is_dirty(&self) -> bool {
        match (&self.server, &self.draft) {
            (Some(s), Some(d)) => s.config != d.config,
            _ => false,
        }
    }

    fn apply(&mut self) {
        let Some(draft) = &self.draft else { return };
        match client::send(&Request::SetConfig { config: draft.config.clone() }) {
            Ok(Response::Ok | Response::State(_)) => {
                self.error = None;
                // Force re-poll on next frame
                self.last_poll = Instant::now() - Duration::from_secs(10);
            }
            Ok(Response::Error { message }) => {
                self.error = Some(format!("apply failed: {message}"));
            }
            Err(e) => {
                self.error = Some(format!("apply failed: {e}"));
            }
        }
    }

    fn discard(&mut self) {
        if let Some(server) = &self.server {
            self.draft = Some(server.clone());
        }
    }

    /// Restart the systemd user unit so a fresh binary picks up (or to recover
    /// from a wedged daemon). Blocks until systemctl returns — usually
    /// sub-second, but the new daemon's stale-sweep can add another second on
    /// top. Force the next poll immediately so the footer reflects reality.
    fn restart_daemon(&mut self) {
        use std::process::Command;
        let result = Command::new("systemctl")
            .args(["--user", "restart", "niri-battery-keeper.service"])
            .output();
        match result {
            Ok(out) if out.status.success() => {
                self.error = None;
                self.last_poll = Instant::now() - Duration::from_secs(10);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let msg = stderr.trim();
                self.error = Some(if msg.is_empty() {
                    format!("restart failed ({})", out.status)
                } else {
                    format!("restart failed: {msg}")
                });
            }
            Err(e) => {
                self.error = Some(format!("restart failed: {e}"));
            }
        }
    }

    /// Run `bootstrap::install()` from the GUI thread. Sub-second in the
    /// happy path (a few `systemctl --user` calls); we accept the brief UI
    /// stall in exchange for keeping the install state machine simple.
    fn install_service(&mut self) {
        match crate::bootstrap::install() {
            Ok(()) => {
                self.install_status = Some(Ok(
                    "Service installed and started.".to_string()
                ));
                self.service_installed = true;
                // Force the next poll immediately so the footer flips from
                // "daemon not reachable" to "● running" without a 1-second pause.
                self.last_poll = Instant::now() - Duration::from_secs(10);
            }
            Err(e) => {
                self.install_status = Some(Err(e.to_string()));
            }
        }
    }

    /// Inverse of [`Self::install_service`] — but only at the service level.
    /// Disables the systemd unit, removes the unit file, runs daemon-reload.
    /// Leaves the binary in `~/.local/bin/` so the GUI can stay open and
    /// re-install in one click. For a full wipe (binary + config) the user
    /// removes those files by hand — no CLI uninstall path any more.
    fn remove_service(&mut self) {
        match crate::bootstrap::remove_service() {
            Ok(()) => {
                self.service_installed = false;
                // Park the install card in a clean state — the previous
                // "Service installed and started." would look stale next to
                // an "Install service" button.
                self.install_status = None;
                self.error = None;
                self.last_poll = Instant::now() - Duration::from_secs(10);
            }
            Err(e) => {
                self.error = Some(format!("remove service failed: {e}"));
            }
        }
    }
}

fn sync_runtime_into_draft(draft: &mut DaemonState, server: &DaemonState) {
    draft.windows = server.windows.clone();
    draft.throttled_units = server.throttled_units.clone();
    draft.apps = server.apps.clone();
    draft.system_units = server.system_units.clone();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.scale_settled {
            self.scale_settled = true;
            if self.persisted.zoom_factor.is_none() {
                let want = detect_native_zoom(ctx);
                if (want - ctx.zoom_factor()).abs() > 0.01 {
                    ctx.set_zoom_factor(want);
                }
            }
            self.persisted.zoom_factor = Some(ctx.zoom_factor());
        }

        let current_zoom = ctx.zoom_factor();
        if self
            .persisted
            .zoom_factor
            .map(|z| (current_zoom - z).abs() > 0.001)
            .unwrap_or(true)
        {
            self.persisted.zoom_factor = Some(current_zoom);
            if let Err(e) = save_persisted_state(&self.persisted) {
                log::warn!("could not persist gui state: {e}");
            }
        }

        ctx.input_mut(|i| {
            i.smooth_scroll_delta *= SCROLL_BOOST;
            i.raw_scroll_delta *= SCROLL_BOOST;
        });

        // Periodic re-poll for live state.
        if self.last_poll.elapsed() > Duration::from_secs(1) {
            self.poll();
        }
        ctx.request_repaint_after(Duration::from_millis(1000));

        let dirty = self.is_dirty();

        egui::TopBottomPanel::top("mode_bar").show(ctx, |ui| {
            ui.add_space(6.0);

            // Kill switch — committed immediately, not via the Apply button,
            // because the whole point is "I want it off right now".
            let disabled_now = self.draft.as_ref()
                .map(|d| d.config.disabled).unwrap_or(false);
            ui.horizontal(|ui| {
                let (label, fill, fg) = if disabled_now {
                    ("● Kill switch: ON  (click to re-enable)",
                     Color32::from_rgb(180, 60, 60), Color32::WHITE)
                } else {
                    ("○ Kill switch: off  (click to disable everything)",
                     Color32::from_rgb(50, 70, 50), Color32::from_rgb(220, 220, 220))
                };
                let btn = egui::Button::new(RichText::new(label).color(fg).strong())
                    .fill(fill)
                    .min_size(egui::vec2(ui.available_width(), 26.0));
                if ui.add(btn).clicked() {
                    let new_val = !disabled_now;
                    // Optimistically update draft + server copies so the UI
                    // doesn't bounce until the next poll lands.
                    if let Some(d) = &mut self.draft { d.config.disabled = new_val; }
                    if let Some(s) = &mut self.server { s.config.disabled = new_val; }
                    match client::send(&Request::SetDisabled { disabled: new_val }) {
                        Ok(Response::Ok | Response::State(_)) => {
                            self.error = None;
                            self.last_poll = Instant::now() - Duration::from_secs(10);
                        }
                        Ok(Response::Error { message }) => {
                            self.error = Some(format!("kill switch: {message}"));
                        }
                        Err(e) => {
                            self.error = Some(format!("kill switch: {e}"));
                        }
                    }
                }
            });
            ui.add_space(6.0);

            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Mode:").strong());
                let modes: Vec<String> = self.draft.as_ref()
                    .map(|d| d.config.modes.keys().cloned().collect())
                    .unwrap_or_default();
                for m in modes {
                    let selected = self.draft.as_ref()
                        .map(|d| d.config.active_mode == m)
                        .unwrap_or(false);
                    let resp = ui.add_enabled(
                        !disabled_now,
                        egui::SelectableLabel::new(selected, &m),
                    );
                    if resp.clicked() {
                        if let Some(d) = &mut self.draft {
                            d.config.active_mode = m;
                        }
                    }
                }
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.selectable_label(self.view == View::Apps, RichText::new("Apps").size(14.0)).clicked() {
                    self.view = View::Apps;
                }
                if ui.selectable_label(self.view == View::Presets, RichText::new("Presets").size(14.0)).clicked() {
                    self.view = View::Presets;
                }
            });
            ui.add_space(4.0);
        });

        if !self.service_installed {
            egui::TopBottomPanel::top("install_banner")
                .frame(
                    egui::Frame::default()
                        .fill(Color32::from_rgb(35, 50, 70))
                        .inner_margin(egui::Margin::symmetric(10.0, 8.0)),
                )
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new("niri-battery-keeper isn't installed as a systemd user service yet.")
                            .color(Color32::WHITE)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(
                            "Until you install it, the daemon won't start on login and \
                             this window can't talk to it.",
                        )
                        .color(Color32::from_rgb(200, 210, 225))
                        .small(),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(
                            RichText::new("Install service").color(Color32::WHITE).strong(),
                        )
                        .fill(Color32::from_rgb(60, 120, 200));
                        if ui
                            .add(btn)
                            .on_hover_text(
                                "Copies the binary into ~/.local/bin/, writes the systemd \
                                 user unit, then runs daemon-reload + enable --now.",
                            )
                            .clicked()
                        {
                            self.install_service();
                        }
                        if let Some(Err(msg)) = &self.install_status {
                            ui.colored_label(
                                Color32::from_rgb(255, 140, 140),
                                format!("install failed: {msg}"),
                            );
                        } else if let Some(Ok(msg)) = &self.install_status {
                            ui.colored_label(Color32::from_rgb(160, 230, 160), msg);
                        }
                    });
                });
        }

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (status_text, color) = match (&self.error, &self.server) {
                    (Some(e), _) => (e.clone(), Color32::from_rgb(220, 80, 80)),
                    (None, Some(s)) if s.config.disabled => (
                        format!("daemon: ● running   kill switch: ON   throttled: {}", s.throttled_units.len()),
                        Color32::from_rgb(220, 160, 70),
                    ),
                    (None, Some(s)) => (
                        format!("daemon: ● running   throttled: {}", s.throttled_units.len()),
                        Color32::from_rgb(120, 200, 120),
                    ),
                    (None, None) => ("daemon: ○ connecting…".to_string(), Color32::GRAY),
                };
                ui.colored_label(color, status_text);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let apply_btn = egui::Button::new(
                        RichText::new("Apply").color(Color32::WHITE),
                    ).fill(if dirty { Color32::from_rgb(60, 120, 200) } else { Color32::DARK_GRAY });
                    if ui.add_enabled(dirty, apply_btn).clicked() {
                        self.apply();
                    }
                    if ui.add_enabled(dirty, egui::Button::new("Discard")).clicked() {
                        self.discard();
                    }
                    ui.separator();
                    if self.service_installed {
                        let remove_btn = egui::Button::new(
                            RichText::new("Remove service…").color(Color32::from_rgb(255, 200, 200)),
                        )
                        .fill(Color32::from_rgb(110, 50, 50));
                        if ui
                            .add(remove_btn)
                            .on_hover_text(
                                "Stop the systemd user service and remove the unit file. \
                                 Binary stays in ~/.local/bin/ so you can re-enable in one \
                                 click. Asks for confirmation.",
                            )
                            .clicked()
                        {
                            self.show_remove_service_confirm = true;
                        }
                    }
                    if ui
                        .button("↻ Restart daemon")
                        .on_hover_text(
                            "systemctl --user restart niri-battery-keeper.service\n\
                             Use after updating the binary or to recover from a wedged daemon.",
                        )
                        .clicked()
                    {
                        self.restart_daemon();
                    }
                });
            });
            ui.add_space(4.0);
        });

        let collapsed_before = (
            self.persisted.collapsed_orphans,
            self.persisted.collapsed_protected,
        );

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.draft.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting to daemon…");
                });
                return;
            }
            ScrollArea::vertical().show(ui, |ui| {
                let view = self.view;
                let preset_mode = &mut self.preset_edit_mode;
                let drafts = &mut self.preset_name_drafts;
                let collapsed_orphans = &mut self.persisted.collapsed_orphans;
                let collapsed_protected = &mut self.persisted.collapsed_protected;
                let draft_ref = self.draft.as_mut().unwrap();
                match view {
                    View::Apps => draw_app_list(
                        ui,
                        draft_ref,
                        collapsed_orphans,
                        collapsed_protected,
                    ),
                    View::Presets => {
                        draw_preset_editor(ui, &mut draft_ref.config, preset_mode, drafts)
                    }
                }
            });
        });

        let collapsed_after = (
            self.persisted.collapsed_orphans,
            self.persisted.collapsed_protected,
        );
        if collapsed_after != collapsed_before {
            if let Err(e) = save_persisted_state(&self.persisted) {
                log::warn!("could not persist gui state: {e}");
            }
        }

        if self.show_remove_service_confirm {
            self.draw_remove_service_modal(ctx);
        }
    }
}

impl App {
    fn draw_remove_service_modal(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let mut open = self.show_remove_service_confirm;
        egui::Window::new("Remove systemd service?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .show(ctx, |ui| {
                ui.label("This will:");
                ui.add_space(2.0);
                ui.label(
                    RichText::new("  • stop the running daemon (systemctl --user disable --now)")
                        .small(),
                );
                ui.label(
                    RichText::new("  • delete ~/.config/systemd/user/niri-battery-keeper.service")
                        .monospace()
                        .small(),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "The binary stays in ~/.local/bin/ — re-enable any time via the \
                         \"Install service\" banner that reappears. To wipe the binary \
                         and config too, remove ~/.local/bin/niri-battery-keeper and \
                         ~/.config/niri-battery-keeper/ by hand.",
                    )
                    .weak()
                    .small(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_remove_service_confirm = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let confirm = egui::Button::new(
                            RichText::new("Remove service").color(Color32::WHITE).strong(),
                        )
                        .fill(Color32::from_rgb(180, 60, 60));
                        if ui.add(confirm).clicked() {
                            self.show_remove_service_confirm = false;
                            self.remove_service();
                        }
                    });
                });
            });
        if !open {
            self.show_remove_service_confirm = false;
        }
    }
}

fn draw_app_list(
    ui: &mut egui::Ui,
    draft: &mut DaemonState,
    collapsed_orphans: &mut bool,
    collapsed_protected: &mut bool,
) {
    use std::collections::BTreeSet;

    let presets: BTreeSet<String> = draft.config.modes.keys().cloned().collect();

    // Window titles, grouped by app_id (for the expanded view).
    let mut titles_by_app: std::collections::BTreeMap<String, Vec<&crate::proto::WindowInfo>> =
        std::collections::BTreeMap::new();
    for w in &draft.windows {
        titles_by_app.entry(w.app_id.clone()).or_default().push(w);
    }

    let apps_clone: Vec<crate::proto::AppGroupInfo> = draft.apps.clone();

    for app in apps_clone {
        let app_id = app.app_id.clone();
        let badge = if app.focused {
            ("active", Color32::from_rgb(120, 200, 120))
        } else if app.excluded {
            ("excluded", Color32::from_rgb(150, 150, 150))
        } else if app.any_throttled {
            ("managed", Color32::from_rgb(220, 160, 70))
        } else {
            ("waiting", Color32::GRAY)
        };

        let any_shared = app.scopes.iter().any(|s| s.shared);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&app_id).strong().size(15.0));
                ui.label(
                    RichText::new(format!("[{}]", badge.0))
                        .color(badge.1)
                        .small()
                );
                if any_shared {
                    ui.label(
                        RichText::new("[shared]")
                            .color(Color32::from_rgb(220, 160, 70))
                            .small(),
                    )
                    .on_hover_text(
                        "One or more scopes are also owned by another Niri app — \
                         throttling this app may affect the other (and vice versa).",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let scope_count = app.scopes.len();
                    let total_pids: usize = app.scopes.iter().map(|s| s.pid_count).sum();
                    let cpu_str = match app.cpu_pct {
                        Some(p) => format!("{p:.1}% cpu"),
                        None => "— cpu".to_string(),
                    };
                    ui.label(
                        RichText::new(format!(
                            "{cpu_str} · {} window(s) · {scope_count} scope(s) · {total_pids} pid(s)",
                            app.window_count
                        ))
                        .weak()
                    );
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Rule:").weak());
                let current = draft.config.apps.get(&app_id).cloned();
                let label = match &current {
                    None | Some(AppRule::UseMode) => "use mode".to_string(),
                    Some(AppRule::Profile { profile }) => format!("profile: {profile}"),
                    Some(AppRule::Exclude) => "exclude".to_string(),
                };
                egui::ComboBox::from_id_salt(format!("rule-{app_id}"))
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(matches!(current, None | Some(AppRule::UseMode)), "use mode").clicked() {
                            draft.config.apps.remove(&app_id);
                        }
                        ui.separator();
                        for p in &presets {
                            let sel = matches!(&current, Some(AppRule::Profile { profile }) if profile == p);
                            if ui.selectable_label(sel, format!("profile: {p}")).clicked() {
                                draft.config.apps.insert(app_id.clone(), AppRule::Profile { profile: p.clone() });
                            }
                        }
                        ui.separator();
                        if ui.selectable_label(matches!(current, Some(AppRule::Exclude)), "exclude").clicked() {
                            draft.config.apps.insert(app_id.clone(), AppRule::Exclude);
                        }
                    });
            });

            egui::CollapsingHeader::new(RichText::new("Details").weak().small())
                .id_salt(format!("details-{app_id}"))
                .default_open(false)
                .show(ui, |ui| {
                    if let Some(wins) = titles_by_app.get(&app_id) {
                        ui.label(RichText::new("Windows:").weak().small());
                        for w in wins {
                            let mark = if w.focused { "▸" } else { " " };
                            ui.label(
                                RichText::new(format!("  {mark} {} (pid {:?})", w.title, w.pid))
                                    .weak()
                                    .small()
                            );
                        }
                    }
                    ui.add_space(2.0);
                    ui.label(RichText::new("Managed scopes:").weak().small());
                    if app.scopes.is_empty() {
                        ui.label(RichText::new("  (none discovered)").weak().small());
                    } else {
                        for s in &app.scopes {
                            let state_text = if s.throttled { "▰" } else { "▱" };
                            let color = if s.throttled {
                                Color32::from_rgb(220, 160, 70)
                            } else {
                                Color32::from_rgb(120, 200, 120)
                            };
                            ui.horizontal(|ui| {
                                ui.colored_label(color, state_text);
                                let shared_tag = if s.shared { "  [shared]" } else { "" };
                                let cpu_tag = match s.cpu_pct {
                                    Some(p) => format!("  {p:.1}% cpu"),
                                    None => "  — cpu".to_string(),
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{}  ({} pid){cpu_tag}{shared_tag}",
                                        s.unit, s.pid_count
                                    ))
                                    .weak()
                                    .small(),
                                );
                            });
                            if let Some(limits) = &s.limits {
                                ui.label(
                                    RichText::new(format!("      {}", format_limits(limits)))
                                        .weak()
                                        .small(),
                                );
                            }
                        }
                    }
                });
        });
        ui.add_space(4.0);
    }

    draw_system_sections(ui, &draft.system_units, collapsed_orphans, collapsed_protected);
}

/// Render "Apps without windows" and "Desktop environment (protected)"
/// sections below the Niri-tracked app list. Each section has a clickable
/// header that toggles a caller-owned collapsed flag (persisted in
/// `gui_state.toml`), so the user can hide whichever group clutters their view.
fn draw_system_sections(
    ui: &mut egui::Ui,
    units: &[SystemUnitInfo],
    collapsed_orphans: &mut bool,
    collapsed_protected: &mut bool,
) {
    let mut orphans: Vec<&SystemUnitInfo> = Vec::new();
    let mut protected: Vec<&SystemUnitInfo> = Vec::new();
    for u in units {
        match &u.category {
            SystemUnitCategory::Managed { .. } => {}
            SystemUnitCategory::Orphan => orphans.push(u),
            SystemUnitCategory::Protected { .. } => protected.push(u),
        }
    }

    if !orphans.is_empty() {
        ui.add_space(6.0);
        ui.separator();
        collapsible_section_header(
            ui,
            &format!("Apps without windows ({})", orphans.len()),
            collapsed_orphans,
        );
        if !*collapsed_orphans {
            for u in orphans {
                draw_system_unit_card(ui, u, "orphan", Color32::from_rgb(150, 150, 150));
            }
        }
    }

    if !protected.is_empty() {
        ui.add_space(6.0);
        ui.separator();
        collapsible_section_header(
            ui,
            &format!("Desktop environment (protected, {})", protected.len()),
            collapsed_protected,
        );
        if !*collapsed_protected {
            for u in protected {
                let label = match &u.category {
                    SystemUnitCategory::Protected { reason } => format!("protected: {reason}"),
                    _ => "protected".to_string(),
                };
                draw_system_unit_card(ui, u, &label, Color32::from_rgb(120, 160, 220));
            }
        }
    }
}

fn collapsible_section_header(ui: &mut egui::Ui, title: &str, collapsed: &mut bool) {
    let arrow = if *collapsed { "▶" } else { "▼" };
    let resp = ui
        .add(
            egui::Label::new(
                RichText::new(format!("{arrow}  — {title} —"))
                    .weak()
                    .strong(),
            )
            .sense(egui::Sense::click()),
        )
        .on_hover_text(if *collapsed {
            "Click to expand"
        } else {
            "Click to collapse"
        });
    if resp.clicked() {
        *collapsed = !*collapsed;
    }
}

fn draw_system_unit_card(
    ui: &mut egui::Ui,
    u: &SystemUnitInfo,
    badge_label: &str,
    badge_color: Color32,
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&u.unit).strong().size(13.0));
            ui.label(
                RichText::new(format!("[{badge_label}]"))
                    .color(badge_color)
                    .small(),
            );
            if u.limits.frozen {
                ui.label(
                    RichText::new("[frozen]")
                        .color(Color32::from_rgb(100, 180, 220))
                        .small(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} pid", u.pid_count))
                        .weak()
                        .small(),
                );
            });
        });

        egui::CollapsingHeader::new(RichText::new("Details").weak().small())
            .id_salt(format!("sys-details-{}", u.unit))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!("cgroup: {}", format_limits(&u.limits)))
                        .weak()
                        .small(),
                );
                ui.add_space(2.0);
                ui.label(RichText::new("Processes:").weak().small());
                if u.processes.is_empty() {
                    ui.label(RichText::new("  (none)").weak().small());
                } else {
                    for p in &u.processes {
                        ui.label(
                            RichText::new(format!(
                                "  {} · {} · {}",
                                p.pid,
                                p.comm,
                                if p.cmdline.is_empty() { "—" } else { &p.cmdline }
                            ))
                            .weak()
                            .small(),
                        );
                    }
                    if u.pid_count > u.processes.len() {
                        ui.label(
                            RichText::new(format!(
                                "  … and {} more",
                                u.pid_count - u.processes.len()
                            ))
                            .weak()
                            .small(),
                        );
                    }
                }
            });
    });
    ui.add_space(4.0);
}

fn format_limits(l: &CgroupLimits) -> String {
    let cpu_w = l
        .cpu_weight
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    let io_w = l
        .io_weight
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    format!(
        "cpu.max={}  cpu.weight={}  io.weight={}{}",
        l.cpu_max,
        cpu_w,
        io_w,
        if l.frozen { "  [FROZEN]" } else { "" }
    )
}

fn draw_preset_editor(
    ui: &mut egui::Ui,
    config: &mut Config,
    mode: &mut PresetEditMode,
    name_drafts: &mut BTreeMap<String, String>,
) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Edit mode:").strong());
        if ui.selectable_label(*mode == PresetEditMode::Simple, "Simple").clicked() {
            *mode = PresetEditMode::Simple;
        }
        if ui.selectable_label(*mode == PresetEditMode::Advanced, "Advanced").clicked() {
            *mode = PresetEditMode::Advanced;
        }
    });
    ui.add_space(4.0);

    match mode {
        PresetEditMode::Simple => {
            ui.label(RichText::new("Each preset uses one percentage that caps CPU and lowers CPU/IO scheduling weights together.").weak().small());
        }
        PresetEditMode::Advanced => {
            ui.label(RichText::new("Each preset throttles unfocused apps with three independent knobs: CPU quota (% of one core), CPU scheduling weight, and IO scheduling weight.").weak().small());
        }
    }
    ui.add_space(8.0);

    let is_simple = *mode == PresetEditMode::Simple;
    // Pin-CPUs column is only meaningful on hybrid CPUs (Intel P/E split).
    // Detected once at startup.
    let topo = cputopo::detect();
    let show_pinning = !is_simple && topo.is_hybrid();
    let num_columns = if is_simple { 3 } else if show_pinning { 6 } else { 5 };

    egui::Grid::new("presets-grid")
        .num_columns(num_columns)
        .striped(true)
        .min_col_width(60.0)
        .show(ui, |ui| {
            ui.label(RichText::new("Name").strong());
            if is_simple {
                ui.label(RichText::new("Resources %").strong());
            } else {
                ui.label(RichText::new("CPU %").strong());
                ui.label(RichText::new("CPU Weight").strong());
                ui.label(RichText::new("IO Weight").strong());
                if show_pinning {
                    let mut tip = String::from(
                        "Restrict unfocused apps to a subset of CPU cores.\n\n\
                         Modern Intel CPUs ship two types of cores:\n\
                           • Performance cores — fast, hot, power-hungry.\n\
                           • Efficient cores — slower, cool, sip power.\n\n\
                         Pinning background apps to the efficient cluster lets \
                         your performance cores idle (and clock down), which \
                         cuts heat and extends battery life. The numbers in \
                         brackets are logical CPU IDs as the kernel sees them \
                         — e.g. 4-11 means CPUs 4 through 11.\n\n\
                         Detected on this machine:"
                    );
                    if let Some(p) = &topo.p_cores {
                        tip.push_str(&format!("\n  Performance: {p}"));
                    }
                    if let Some(e) = &topo.e_cores {
                        tip.push_str(&format!("\n  Efficient: {e}"));
                    }
                    if let Some(lp) = &topo.lp_cores {
                        tip.push_str(&format!("\n  Low-power: {lp}"));
                    }
                    ui.label(RichText::new("Run on").strong()).on_hover_text(tip);
                }
            }
            ui.label("");
            ui.end_row();

            let names: Vec<String> = config
                .modes
                .iter()
                .filter(|(_, p)| matches!(p, Profile::Throttle { .. }))
                .map(|(k, _)| k.clone())
                .collect();
            let mut to_remove: Option<String> = None;
            let mut to_rename: Option<(String, String)> = None;
            for name in names {
                // Name as editable TextEdit, backed by a per-row draft buffer.
                let draft = name_drafts
                    .entry(name.clone())
                    .or_insert_with(|| name.clone());
                let resp = ui.add(
                    egui::TextEdit::singleline(draft)
                        .id_salt(format!("preset-name-{name}"))
                        .desired_width(120.0),
                );
                if resp.lost_focus() {
                    let trimmed = draft.trim().to_string();
                    if trimmed.is_empty() || trimmed == name {
                        *draft = name.clone();
                    } else if config.modes.contains_key(&trimmed) {
                        // Collision with another existing preset — silently revert.
                        *draft = name.clone();
                    } else {
                        to_rename = Some((name.clone(), trimmed));
                    }
                }

                let Some(Profile::Throttle { cpu_quota, cpu_weight, io_weight, allowed_cpus }) =
                    config.modes.get_mut(&name)
                else {
                    // Shouldn't happen — we filtered to Throttle above.
                    ui.end_row();
                    continue;
                };

                if is_simple {
                    let mut pct = parse_pct(&cpu_quota.0).unwrap_or(50).clamp(1, 100);
                    if ui.add(egui::DragValue::new(&mut pct).range(1..=100).suffix("%")).changed() {
                        let n = pct.max(1) as u32;
                        cpu_quota.0 = format!("{n}%");
                        *cpu_weight = n;
                        *io_weight = n;
                    }
                } else {
                    let mut quota_pct = parse_pct(&cpu_quota.0).unwrap_or(50);
                    if ui.add(egui::DragValue::new(&mut quota_pct).range(1..=1000).suffix("%")).changed() {
                        cpu_quota.0 = format!("{quota_pct}%");
                    }
                    let mut w = *cpu_weight as i32;
                    if ui.add(egui::DragValue::new(&mut w).range(1..=10000)).changed() {
                        *cpu_weight = w.max(1) as u32;
                    }
                    let mut iow = *io_weight as i32;
                    if ui.add(egui::DragValue::new(&mut iow).range(1..=10000)).changed() {
                        *io_weight = iow.max(1) as u32;
                    }
                    if show_pinning {
                        draw_cpu_pin_cell(ui, &name, allowed_cpus, topo);
                    }
                }

                if ui.button("✕").clicked() && config.modes.len() > 1 {
                    to_remove = Some(name.clone());
                }
                ui.end_row();
            }
            if let Some((old, new)) = to_rename {
                if let Some(profile) = config.modes.remove(&old) {
                    config.modes.insert(new.clone(), profile);
                }
                if config.active_mode == old {
                    config.active_mode = new.clone();
                }
                for rule in config.apps.values_mut() {
                    if let AppRule::Profile { profile } = rule {
                        if *profile == old {
                            *profile = new.clone();
                        }
                    }
                }
                name_drafts.remove(&old);
            }
            if let Some(name) = to_remove {
                config.modes.remove(&name);
                if config.active_mode == name {
                    if let Some(first) = config.modes.keys().next().cloned() {
                        config.active_mode = first;
                    }
                }
                name_drafts.remove(&name);
            }
            // Evict stale draft entries for presets that no longer exist.
            name_drafts.retain(|k, _| config.modes.contains_key(k));
        });

    ui.add_space(8.0);
    if ui.button("+ Add preset").clicked() {
        let base = Profile::Throttle {
            cpu_quota: CpuQuota("50%".into()),
            cpu_weight: 50,
            io_weight: 50,
            allowed_cpus: None,
        };
        let mut name = "custom".to_string();
        let mut n = 1;
        while config.modes.contains_key(&name) {
            n += 1;
            name = format!("custom{n}");
        }
        config.modes.insert(name, base);
    }
}

fn parse_pct(s: &str) -> Option<i32> {
    let trimmed = s.trim().trim_end_matches('%');
    trimmed.parse::<i32>().ok()
}

#[derive(Copy, Clone, PartialEq)]
enum CpuPinChoice {
    Any,
    Efficient,
    Performance,
    Custom,
}

fn current_pin_choice(value: &Option<String>, topo: &Topology) -> CpuPinChoice {
    match value.as_deref() {
        None | Some("") => CpuPinChoice::Any,
        Some(s) if topo.efficient().as_deref() == Some(s) => CpuPinChoice::Efficient,
        Some(s) if topo.p_cores.as_deref() == Some(s) => CpuPinChoice::Performance,
        _ => CpuPinChoice::Custom,
    }
}

fn draw_cpu_pin_cell(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut Option<String>,
    topo: &Topology,
) {
    let mut choice = current_pin_choice(value, topo);
    // `Custom` is only ever produced by a hand-edited config — surface it as
    // read-only selected_text so we don't silently clobber the user's value,
    // but don't tempt them to pick it from the dropdown.
    let selected = match choice {
        CpuPinChoice::Any => "All cores",
        CpuPinChoice::Efficient => "Efficient",
        CpuPinChoice::Performance => "Performance",
        CpuPinChoice::Custom => "Custom",
    };
    egui::ComboBox::from_id_salt(format!("cpupin-{id}"))
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut choice, CpuPinChoice::Any, "All cores")
                .on_hover_text("No pinning — kernel scheduler picks freely.");
            if let Some(eff) = topo.efficient() {
                ui.selectable_value(
                    &mut choice,
                    CpuPinChoice::Efficient,
                    format!("Efficient cores  ({eff})"),
                )
                .on_hover_text(
                    "Pin to E-cores (low power, cool). \
                     Best for background apps you want quiet."
                );
            }
            if let Some(p) = topo.p_cores.clone() {
                ui.selectable_value(
                    &mut choice,
                    CpuPinChoice::Performance,
                    format!("Performance cores  ({p})"),
                )
                .on_hover_text(
                    "Pin to P-cores (fast, hot). \
                     Rarely useful here — these are the cores you want free."
                );
            }
        });
    match choice {
        CpuPinChoice::Any => *value = None,
        CpuPinChoice::Efficient => *value = topo.efficient(),
        CpuPinChoice::Performance => *value = topo.p_cores.clone(),
        CpuPinChoice::Custom => {} // hand-edited config, leave intact
    }
}
