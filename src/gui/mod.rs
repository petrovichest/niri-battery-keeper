use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::config::{AppRule, Config, CpuQuota, Profile};
use crate::proto::{client, DaemonState, Request, Response};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Niri Throttle")
            .with_inner_size([720.0, 720.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "niri-battery-keeper",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
    .map_err(|e| -> Box<dyn std::error::Error> { format!("eframe: {e}").into() })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Apps,
    Presets,
}

struct App {
    /// Last state received from daemon (read-only baseline).
    server: Option<DaemonState>,
    /// Local edited copy. None until first successful poll.
    draft: Option<DaemonState>,
    last_poll: Instant,
    error: Option<String>,
    view: View,
}

impl App {
    fn new() -> Self {
        let mut me = Self {
            server: None,
            draft: None,
            last_poll: Instant::now() - Duration::from_secs(10),
            error: None,
            view: View::Apps,
        };
        me.poll();
        me
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
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
}

fn sync_runtime_into_draft(draft: &mut DaemonState, server: &DaemonState) {
    draft.windows = server.windows.clone();
    draft.throttled_units = server.throttled_units.clone();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Periodic re-poll for live state.
        if self.last_poll.elapsed() > Duration::from_secs(1) {
            self.poll();
        }
        ctx.request_repaint_after(Duration::from_millis(1000));

        let dirty = self.is_dirty();

        egui::TopBottomPanel::top("mode_bar").show(ctx, |ui| {
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
                    if ui.selectable_label(selected, &m).clicked() {
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

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (status_text, color) = match (&self.error, &self.server) {
                    (Some(e), _) => (e.clone(), Color32::from_rgb(220, 80, 80)),
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
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.draft.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting to daemon…");
                });
                return;
            }
            ScrollArea::vertical().show(ui, |ui| {
                let draft_ref = self.draft.as_mut().unwrap();
                match self.view {
                    View::Apps => draw_app_list(ui, draft_ref),
                    View::Presets => draw_preset_editor(ui, &mut draft_ref.config),
                }
            });
        });
    }
}

fn draw_app_list(ui: &mut egui::Ui, draft: &mut DaemonState) {
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

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&app_id).strong().size(15.0));
                ui.label(
                    RichText::new(format!("[{}]", badge.0))
                        .color(badge.1)
                        .small()
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let scope_count = app.scopes.len();
                    let total_pids: usize = app.scopes.iter().map(|s| s.pid_count).sum();
                    ui.label(
                        RichText::new(format!(
                            "{} window(s) · {scope_count} scope(s) · {total_pids} pid(s)",
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
                                ui.label(
                                    RichText::new(format!("{}  ({} pid)", s.unit, s.pid_count))
                                        .weak()
                                        .small()
                                );
                            });
                        }
                    }
                });
        });
        ui.add_space(4.0);
    }
}

fn draw_preset_editor(ui: &mut egui::Ui, config: &mut Config) {
    ui.add_space(4.0);
    ui.label(RichText::new("Each preset chooses one of three actions for unfocused apps:").weak().small());
    ui.label(RichText::new("  • Off  — leave the app alone").weak().small());
    ui.label(RichText::new("  • Throttle  — cap CPU% and weights").weak().small());
    ui.label(RichText::new("  • Pause  — freeze the app entirely (0% CPU)").weak().small());
    ui.add_space(8.0);

    egui::Grid::new("presets-grid")
        .num_columns(6)
        .striped(true)
        .min_col_width(60.0)
        .show(ui, |ui| {
            ui.label(RichText::new("Name").strong());
            ui.label(RichText::new("Action").strong());
            ui.label(RichText::new("CPU %").strong());
            ui.label(RichText::new("CPU Weight").strong());
            ui.label(RichText::new("IO Weight").strong());
            ui.label("");
            ui.end_row();

            let names: Vec<String> = config.modes.keys().cloned().collect();
            let mut to_remove: Option<String> = None;
            for name in names {
                ui.label(&name);

                let p = config.modes.get(&name).cloned().unwrap();
                let current_kind = match &p {
                    Profile::None => "off",
                    Profile::Throttle { .. } => "throttle",
                    Profile::Pause => "pause",
                };
                let mut new_profile = p.clone();
                egui::ComboBox::from_id_salt(format!("action-{name}"))
                    .selected_text(current_kind)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(current_kind == "off", "off").clicked() {
                            new_profile = Profile::None;
                        }
                        if ui.selectable_label(current_kind == "throttle", "throttle").clicked() {
                            if !matches!(p, Profile::Throttle { .. }) {
                                new_profile = Profile::Throttle {
                                    cpu_quota: CpuQuota("50%".into()),
                                    cpu_weight: 50,
                                    io_weight: 50,
                                };
                            }
                        }
                        if ui.selectable_label(current_kind == "pause", "pause").clicked() {
                            new_profile = Profile::Pause;
                        }
                    });

                match &mut new_profile {
                    Profile::Throttle { cpu_quota, cpu_weight, io_weight } => {
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
                    }
                    _ => {
                        ui.label(RichText::new("—").weak());
                        ui.label(RichText::new("—").weak());
                        ui.label(RichText::new("—").weak());
                    }
                }

                if ui.button("✕").clicked() && config.modes.len() > 1 {
                    to_remove = Some(name.clone());
                }
                ui.end_row();

                if new_profile != p {
                    config.modes.insert(name, new_profile);
                }
            }
            if let Some(name) = to_remove {
                config.modes.remove(&name);
                if config.active_mode == name {
                    if let Some(first) = config.modes.keys().next().cloned() {
                        config.active_mode = first;
                    }
                }
            }
        });

    ui.add_space(8.0);
    if ui.button("+ Add preset").clicked() {
        let base = Profile::Throttle {
            cpu_quota: CpuQuota("50%".into()),
            cpu_weight: 50,
            io_weight: 50,
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
