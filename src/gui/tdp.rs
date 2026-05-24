//! TDP tab: read live RAPL/coretemp state, expose two sliders for PL1/PL2,
//! and invoke `pkexec /usr/local/bin/nbk-set-rapl` on Apply. Stateless w.r.t.
//! the daemon — everything here is user-readable sysfs plus a one-shot
//! privileged helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Rounding};
use egui_plot::{GridMark, HPlacement, Plot, PlotPoint};

use crate::proto::EnergyInfo;

const RAPL_BASE: &str = "/sys/class/powercap/intel-rapl:0";
const HELPER_PATH: &str = "/usr/local/bin/nbk-set-rapl";

const PL1_MIN_W: u32 = 0;
const PL1_MAX_W: u32 = 60;
const PL2_MIN_W: u32 = 0;
const PL2_MAX_W: u32 = 90;

/// How often to re-check whether a polkit agent is running. pgrep is cheap
/// but not free — once a second is plenty for a status indicator.
const AGENT_RECHECK: Duration = Duration::from_secs(2);

pub struct TdpState {
    pl1_draft_w: u32,
    pl2_draft_w: u32,
    snapshot: Snapshot,
    last_poll: Instant,
    energy_sample: Option<EnergySample>,
    coretemp_path: Option<PathBuf>,
    apply: Option<ApplyJob>,
    last_apply: Option<Result<String, String>>,
    helper_available: bool,
    install_status: Option<Result<String, String>>,
    /// In-flight `bootstrap::install_tdp` call. Blocks the install button
    /// while running, and `tick()` clears it when the pkexec child reports
    /// back. We don't actually poll a child here — install is synchronous,
    /// so this is just `Some` for the brief moment the button is clicked.
    install_busy: bool,
    polkit_agent_running: bool,
    last_agent_check: Instant,
}

#[derive(Default, Clone)]
struct Snapshot {
    pl1_uw: Option<u64>,
    pl2_uw: Option<u64>,
    pl3_uw: Option<u64>,
    temp_c: Option<f32>,
    power_w: Option<f32>,
}

struct EnergySample {
    uj: u64,
    at: Instant,
}

struct ApplyJob {
    child: Child,
    started_at: Instant,
}

impl TdpState {
    pub fn new() -> Self {
        let mut me = Self {
            pl1_draft_w: 0,
            pl2_draft_w: 0,
            snapshot: Snapshot::default(),
            last_poll: Instant::now() - Duration::from_secs(10),
            energy_sample: None,
            coretemp_path: discover_coretemp(),
            apply: None,
            last_apply: None,
            helper_available: Path::new(HELPER_PATH).exists(),
            install_status: None,
            install_busy: false,
            polkit_agent_running: polkit_agent_running(),
            last_agent_check: Instant::now(),
        };
        me.refresh();
        if let Some(pl1) = me.snapshot.pl1_uw {
            me.pl1_draft_w = uw_to_w(pl1).clamp(PL1_MIN_W, PL1_MAX_W);
        }
        if let Some(pl2) = me.snapshot.pl2_uw {
            me.pl2_draft_w = uw_to_w(pl2).clamp(PL2_MIN_W, PL2_MAX_W);
        }
        me
    }

    pub fn tick(&mut self) {
        if self.last_poll.elapsed() >= Duration::from_millis(900) {
            self.refresh();
        }
        if self.last_agent_check.elapsed() >= AGENT_RECHECK {
            self.polkit_agent_running = polkit_agent_running();
            self.last_agent_check = Instant::now();
        }
        if let Some(job) = &mut self.apply {
            match job.child.try_wait() {
                Ok(Some(status)) => {
                    let started = job.started_at;
                    self.apply = None;
                    let elapsed = started.elapsed();
                    if status.success() {
                        self.last_apply = Some(Ok(format!(
                            "Applied in {:.1}s",
                            elapsed.as_secs_f32()
                        )));
                        self.last_poll = Instant::now() - Duration::from_secs(10);
                    } else {
                        let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                        self.last_apply = Some(Err(format!("pkexec exited {code}")));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    self.apply = None;
                    self.last_apply = Some(Err(format!("waitpid: {e}")));
                }
            }
        }
    }

    fn refresh(&mut self) {
        self.last_poll = Instant::now();
        self.snapshot.pl1_uw = read_u64(&format!("{RAPL_BASE}/constraint_0_power_limit_uw"));
        self.snapshot.pl2_uw = read_u64(&format!("{RAPL_BASE}/constraint_1_power_limit_uw"));
        self.snapshot.pl3_uw = read_u64(&format!("{RAPL_BASE}/constraint_2_power_limit_uw"));
        self.snapshot.temp_c = self.coretemp_path.as_ref().and_then(|p| {
            read_u64(p.to_str()?).map(|m| m as f32 / 1000.0)
        });

        // Energy delta → average power over the sample interval. energy_uj is a
        // monotonic counter in microjoules that wraps at max_energy_range_uj —
        // for sub-second samples we'll never see the wrap in practice.
        let now = Instant::now();
        if let Some(cur) = read_u64(&format!("{RAPL_BASE}/energy_uj")) {
            if let Some(prev) = &self.energy_sample {
                let dt = now.saturating_duration_since(prev.at).as_secs_f64();
                if dt > 0.05 && cur >= prev.uj {
                    let duj = (cur - prev.uj) as f64;
                    self.snapshot.power_w = Some((duj / 1_000_000.0 / dt) as f32);
                }
            }
            self.energy_sample = Some(EnergySample { uj: cur, at: now });
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, energy: Option<&EnergyInfo>) {
        ui.add_space(8.0);

        // Polkit agent banner — only when missing. Shown above everything else
        // because without an agent, neither install nor Apply can show a
        // password prompt (pkexec exits immediately on "no authentication
        // agent").
        if !self.polkit_agent_running {
            self.draw_polkit_agent_banner(ui);
            ui.add_space(8.0);
        }

        // Energy section first — comparative-test workflow is "watch the
        // graph, move the slider, watch the graph respond". Putting it on
        // top makes that loop obvious. The section reads daemon-sampled
        // data (sysfs only, no helper needed) so it shows even when the
        // TDP helper isn't installed yet.
        if let Some(e) = energy {
            self.draw_energy_section(ui, e);
            ui.add_space(12.0);
        }

        if !self.helper_available {
            self.draw_install_card(ui);
            return;
        }



        // ─── Live readouts ───────────────────────────────────────────────
        egui::Frame::default()
            .fill(Color32::from_rgb(30, 35, 45))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    cell(ui, "Pkg temp", self.snapshot.temp_c.map(|t| format!("{t:.0} °C")));
                    ui.add_space(24.0);
                    cell(ui, "Pkg power", self.snapshot.power_w.map(|p| format!("{p:.1} W")));
                    ui.add_space(24.0);
                    cell(
                        ui,
                        "Active limits",
                        Some(format!(
                            "PL1 {}  PL2 {}  PL3 {}",
                            fmt_w(self.snapshot.pl1_uw),
                            fmt_w(self.snapshot.pl2_uw),
                            fmt_w(self.snapshot.pl3_uw),
                        )),
                    );
                });
            });

        ui.add_space(14.0);
        ui.label(RichText::new("Power limits").strong());
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Sustained (PL1)").monospace());
            ui.add_space(8.0);
            ui.add(
                egui::Slider::new(&mut self.pl1_draft_w, PL1_MIN_W..=PL1_MAX_W)
                    .suffix(" W")
                    .clamping(egui::SliderClamping::Always),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Burst (PL2)    ").monospace());
            ui.add_space(8.0);
            ui.add(
                egui::Slider::new(&mut self.pl2_draft_w, PL2_MIN_W..=PL2_MAX_W)
                    .suffix(" W")
                    .clamping(egui::SliderClamping::Always),
            );
        });

        // Keep PL2 ≥ PL1: burst limit below sustained makes no sense and
        // some firmware silently rejects it.
        if self.pl2_draft_w < self.pl1_draft_w {
            self.pl2_draft_w = self.pl1_draft_w;
        }

        ui.add_space(10.0);

        let in_flight = self.apply.is_some();
        let dirty = self.is_dirty();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(if in_flight {
                "Applying…"
            } else {
                "Apply"
            });
            if ui
                .add_enabled(!in_flight && dirty, btn)
                .on_hover_text(
                    "pkexec /usr/local/bin/nbk-set-rapl <PL1> <PL2>\n\
                     Polkit will prompt for the root password the first time \
                     (then cached for 5 minutes).",
                )
                .clicked()
            {
                self.spawn_apply();
            }
            if ui
                .add_enabled(!in_flight && dirty, egui::Button::new("Reset"))
                .on_hover_text("Discard slider changes, snap back to what RAPL currently reports.")
                .clicked()
            {
                if let Some(pl1) = self.snapshot.pl1_uw {
                    self.pl1_draft_w = uw_to_w(pl1).clamp(PL1_MIN_W, PL1_MAX_W);
                }
                if let Some(pl2) = self.snapshot.pl2_uw {
                    self.pl2_draft_w = uw_to_w(pl2).clamp(PL2_MIN_W, PL2_MAX_W);
                }
            }
        });

        if let Some(result) = &self.last_apply {
            ui.add_space(6.0);
            match result {
                Ok(msg) => {
                    ui.label(RichText::new(msg).color(Color32::from_rgb(140, 220, 140)).small());
                }
                Err(msg) => {
                    ui.label(RichText::new(msg).color(Color32::from_rgb(240, 140, 140)).small());
                }
            }
        }

        ui.add_space(14.0);
        ui.label(
            RichText::new(
                "TDP limits reset to OEM defaults on reboot — re-apply your preferred \
                 values after each boot, or wire up a systemd unit later.",
            )
            .weak()
            .small()
            .italics(),
        );
    }

    fn is_dirty(&self) -> bool {
        let cur_pl1 = self.snapshot.pl1_uw.map(uw_to_w);
        let cur_pl2 = self.snapshot.pl2_uw.map(uw_to_w);
        cur_pl1 != Some(self.pl1_draft_w) || cur_pl2 != Some(self.pl2_draft_w)
    }

    fn spawn_apply(&mut self) {
        let pl1_uw = (self.pl1_draft_w as u64) * 1_000_000;
        let pl2_uw = (self.pl2_draft_w as u64) * 1_000_000;
        let child = Command::new("pkexec")
            .arg(HELPER_PATH)
            .arg(pl1_uw.to_string())
            .arg(pl2_uw.to_string())
            .spawn();
        match child {
            Ok(child) => {
                self.apply = Some(ApplyJob { child, started_at: Instant::now() });
                self.last_apply = None;
            }
            Err(e) => {
                self.last_apply = Some(Err(format!("spawn pkexec: {e}")));
            }
        }
    }
}

impl TdpState {
    fn draw_install_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .fill(Color32::from_rgb(35, 50, 70))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Set up TDP control")
                        .strong()
                        .color(Color32::from_rgb(180, 210, 255)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "TDP control needs a small root-owned helper plus a polkit \
                         policy. One click below installs all of it via pkexec — \
                         you'll be prompted for the root password exactly once."
                    )
                    .small(),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!(
                        "  • copy this binary → {HELPER_PATH}  (root-owned, multi-call)\n  \
                         • install polkit policy with auth_admin_keep (5 min cache)\n  \
                         • install udev rule so the wattage readout works without root"
                    ))
                    .monospace()
                    .small()
                    .weak(),
                );
                ui.add_space(8.0);

                let busy = self.install_busy;
                let agent_ok = self.polkit_agent_running;
                let btn = egui::Button::new(if busy {
                    "Installing…"
                } else {
                    "Install TDP helper"
                });
                if ui
                    .add_enabled(!busy && agent_ok, btn)
                    .on_hover_text(if agent_ok {
                        "pkexec sh -c '…'  — single root-password prompt installs \
                         the helper, polkit policy, and udev rule."
                    } else {
                        "Install a polkit authentication agent first (see banner above)."
                    })
                    .clicked()
                {
                    self.run_install();
                }

                if let Some(result) = &self.install_status {
                    ui.add_space(6.0);
                    match result {
                        Ok(msg) => {
                            ui.label(
                                RichText::new(msg)
                                    .color(Color32::from_rgb(140, 220, 140))
                                    .small(),
                            );
                        }
                        Err(msg) => {
                            ui.label(
                                RichText::new(msg)
                                    .color(Color32::from_rgb(240, 140, 140))
                                    .small(),
                            );
                        }
                    }
                }
            });
    }

    fn draw_polkit_agent_banner(&self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .fill(Color32::from_rgb(70, 50, 30))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("No polkit authentication agent detected")
                        .strong()
                        .color(Color32::from_rgb(255, 210, 160)),
                );
                ui.label(
                    RichText::new(
                        "Without an agent, the system cannot show a password \
                         dialog — Install and Apply will fail silently. Install \
                         one and start it in your Niri session:"
                    )
                    .small(),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "  paru -S hyprpolkitagent\n  \
                         systemctl --user enable --now hyprpolkitagent.service"
                    )
                    .monospace()
                    .small(),
                );
            });
    }

    fn run_install(&mut self) {
        self.install_busy = true;
        self.install_status = None;
        // bootstrap::install_tdp() blocks on pkexec, which itself blocks on
        // the user's password prompt. egui's frame loop is paused during that
        // wait, but the call is one-shot so re-entering the frame on return
        // is fine. Most installs take under a second once the prompt is past.
        let result = crate::bootstrap::install_tdp();
        self.install_busy = false;
        match result {
            Ok(()) => {
                self.install_status =
                    Some(Ok("Installed. TDP control is ready.".into()));
                self.helper_available = true;
                // Force RAPL re-read on next tick so the sliders snap to the
                // freshly-readable energy_uj counter.
                self.last_poll = Instant::now() - Duration::from_secs(10);
                self.energy_sample = None;
            }
            Err(e) => {
                self.install_status = Some(Err(format!("Install failed: {e}")));
            }
        }
    }
}

fn polkit_agent_running() -> bool {
    // pgrep -f matches against the full command line. Covers hyprpolkitagent,
    // polkit-gnome-authentication-agent-1, polkit-kde-authentication-agent-1,
    // mate-polkit, lxqt-policykit-agent, xfce-polkit, etc. The "agent" suffix
    // is the load-bearing word — bare `polkit*` would match polkitd itself.
    let out = Command::new("pgrep")
        .arg("-f")
        .arg("polkit.*[Aa]gent")
        .output();
    match out {
        Ok(o) => o.status.success() && !o.stdout.is_empty(),
        Err(_) => false,
    }
}

fn cell(ui: &mut egui::Ui, label: &str, value: Option<String>) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).weak());
        ui.label(
            RichText::new(value.unwrap_or_else(|| "—".into())).monospace(),
        );
    });
}

fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn uw_to_w(uw: u64) -> u32 {
    ((uw + 500_000) / 1_000_000) as u32
}

fn fmt_w(uw: Option<u64>) -> String {
    uw.map(|u| format!("{}W", uw_to_w(u))).unwrap_or_else(|| "—".into())
}

impl TdpState {
    fn draw_energy_section(&mut self, ui: &mut egui::Ui, e: &EnergyInfo) {
        // ─── Status card ─────────────────────────────────────────────────
        // Inline `label: value` chunks separated by add_space(8.0) so the
        // whole row wraps naturally on a narrow window. The previous
        // vertical-cell layout had ~580 px of fixed gaps and clipped off
        // the right edge of the smaller GUI viewport.
        egui::Frame::default()
            .fill(Color32::from_rgb(28, 32, 40))
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                let pct = e
                    .capacity_pct
                    .map(|p| format!("{p}%"))
                    .unwrap_or_else(|| "—".into());

                // Whichever timestamp matches the current state is the
                // "live" counter — show only that one so the row stays
                // single-line. Both timestamps persist across daemon
                // restarts so the count survives restarts mid-session.
                let on_label = if e.on_ac {
                    format_duration(e.on_ac_active_s)
                } else {
                    format_duration(e.on_battery_active_s)
                };
                let on_caption = if e.on_ac { "On AC" } else { "On bat" };

                let time_label = format_duration(e.time_remaining_s);
                let time_caption = match e.charge_state.as_str() {
                    "charging" => "Full in",
                    _ => "Empty in",
                };

                // Net Wh over this session: positive = net charged,
                // negative = net discharged. Replaces the old pair of
                // `Session out` + `Session in`, one of which was always 0.
                let net_wh = e.session_charge_wh - e.session_discharge_wh;
                let net_str = if net_wh.abs() < 0.005 {
                    "0.00 Wh".to_string()
                } else {
                    format!("{net_wh:+.2} Wh")
                };

                ui.horizontal_wrapped(|ui| {
                    pair(ui, "Charge",     &pct);
                    pair(ui, on_caption,   &on_label);
                    pair(ui, time_caption, &time_label);
                    pair(ui, "Net",        &net_str);
                });

                ui.add_space(4.0);

                // Live wattage breakdown — disjoint slices that sum to
                // Battery: CPU is RAPL package, GpuSoc is psys − package
                // (iGPU + IMC + DMI + …), Other is battery − psys
                // (display, NVMe, WiFi, EC). When psys is missing we
                // degrade to two slices (CPU + everything-else).
                let cpu = e.pkg_w;
                let gpu_soc = match (e.psys_w, e.pkg_w) {
                    (Some(p), Some(c)) => Some((p - c).max(0.0)),
                    _ => None,
                };
                let other = match (e.battery_w, e.psys_w, e.pkg_w) {
                    (Some(b), Some(p), _) => Some((b - p).max(0.0)),
                    (Some(b), None, Some(c)) => Some((b - c).max(0.0)),
                    _ => None,
                };
                ui.horizontal_wrapped(|ui| {
                    pair(ui, "Battery draw", &fmt_w_opt(e.battery_w));
                    pair(ui, "CPU pkg",      &fmt_w_opt(cpu));
                    pair(ui, "GPU+SoC",      &fmt_w_opt(gpu_soc));
                    pair(ui, "Display+I/O",  &fmt_w_opt(other));
                });
            });

        // ─── Battery level over 24 h ─────────────────────────────────────
        // 24 bars, one per hour. Each bar = average % over the samples
        // that fell into that hour. All green — same colour both on
        // discharge and on recharge — keeps the read clean ("how full was
        // the battery this hour") without juggling segment colours.
        ui.add_space(8.0);
        ui.label(RichText::new("Battery level — last 24 h").strong());
        ui.add_space(4.0);

        if e.samples.is_empty() {
            ui.label(
                RichText::new(
                    "Collecting samples… first bar lands ~1 h after daemon start.",
                )
                .small()
                .weak()
                .italics(),
            );
            return;
        }

        // Bucket samples by hours-ago. Bucket k covers ages
        // [k h, k+1 h), so bar k sits at x = -(k + 0.5).
        const HOURS: usize = 24;
        const BPH: usize = 4;
        const BUCKETS: usize = HOURS * BPH;
        const BUCKET_S: f32 = 3600.0 / BPH as f32;

        let mut sum_pct = [0.0_f64; BUCKETS];
        let mut counts = [0_u32; BUCKETS];
        for s in &e.samples {
            let Some(p) = s.capacity_pct else { continue };
            let idx = (s.age_s / BUCKET_S) as i64;
            if idx < 0 || idx >= BUCKETS as i64 {
                continue;
            }
            let idx = idx as usize;
            sum_pct[idx] += p as f64;
            counts[idx] += 1;
        }

        let mut avg_pct: [Option<f64>; BUCKETS] = [None; BUCKETS];
        for k in 0..BUCKETS {
            if counts[k] > 0 {
                avg_pct[k] = Some(sum_pct[k] / counts[k] as f64);
            }
        }
        let known: Vec<usize> = (0..BUCKETS).filter(|&k| avg_pct[k].is_some()).collect();
        for w in known.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b - a > 1 {
                let va = avg_pct[a].unwrap();
                let vb = avg_pct[b].unwrap();
                let span = (b - a) as f64;
                for j in (a + 1)..b {
                    let t = (j - a) as f64 / span;
                    avg_pct[j] = Some(va + t * (vb - va));
                }
            }
        }

        let green = Color32::from_rgb(140, 220, 140);
        let stroke_green = Color32::from_rgb(170, 240, 170);
        let empty = Color32::from_rgb(60, 60, 60);
        let stroke_empty = Color32::from_rgb(80, 80, 80);

        let prev_bg = ui.visuals().extreme_bg_color;
        ui.visuals_mut().extreme_bg_color = Color32::from_rgb(30, 30, 35);

        let now_h = {
            let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
            let t = unsafe { libc::time(std::ptr::null_mut()) };
            unsafe { libc::localtime_r(&t, &mut tm) };
            tm.tm_hour as f64 + tm.tm_min as f64 / 60.0
        };

        let plot_response = Plot::new("battery_level_bars")
            .height(180.0)
            .show_x(false)
            .show_y(false)
            .x_axis_formatter(move |m, _range| {
                let clock_h = ((now_h + m.value).rem_euclid(24.0)).round() as u32;
                format!("{clock_h}")
            })
            .x_grid_spacer(move |input| {
                let mut marks = Vec::new();
                for t in (0u32..24).step_by(3) {
                    let mut x = t as f64 - now_h;
                    if x > 0.0 { x -= 24.0; }
                    if x >= input.bounds.0 && x <= input.bounds.1 {
                        marks.push(GridMark { value: x, step_size: 3.0 });
                    }
                }
                marks
            })
            .y_axis_formatter(|m, _range| {
                let v = m.value.round() as i64;
                if v == 0 || v == 50 || v == 100 {
                    format!("{v}%")
                } else {
                    String::new()
                }
            })
            .y_axis_position(HPlacement::Right)
            .y_grid_spacer(|_| {
                vec![
                    GridMark { value: 0.0, step_size: 50.0 },
                    GridMark { value: 25.0, step_size: 25.0 },
                    GridMark { value: 50.0, step_size: 50.0 },
                    GridMark { value: 75.0, step_size: 25.0 },
                    GridMark { value: 100.0, step_size: 50.0 },
                ]
            })
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .show_axes([true, true])
            .include_x(-(HOURS as f64))
            .include_x(0.0)
            .include_y(0.0)
            .include_y(100.0)
            .show(ui, |_| {});

        ui.visuals_mut().extreme_bg_color = prev_bg;

        let transform = &plot_response.transform;
        let plot_rect = *transform.frame();
        let painter = ui.painter().with_clip_rect(plot_rect);
        let bar_rounding = Rounding { nw: 1.0, ne: 1.0, sw: 0.0, se: 0.0 };
        let half_w = 0.35 / BPH as f64;

        struct BarInfo {
            rect: egui::Rect,
            idx: usize,
        }
        let mut bar_infos: Vec<BarInfo> = Vec::with_capacity(BUCKETS);

        for k in 0..BUCKETS {
            let center_x = -((k as f64 + 0.5) / BPH as f64);
            let (val, fill, stroke_c) = match avg_pct[k] {
                Some(avg) => (avg, green, stroke_green),
                None => (0.5, empty, stroke_empty),
            };
            let lo = PlotPoint::new(center_x - half_w, 0.0);
            let hi = PlotPoint::new(center_x + half_w, val);
            let screen_rect = transform.rect_from_values(&lo, &hi);
            painter.add(egui::Shape::Rect(egui::epaint::RectShape::new(
                screen_rect,
                bar_rounding,
                fill,
                egui::Stroke::new(0.5, stroke_c),
            )));
            bar_infos.push(BarInfo { rect: screen_rect, idx: k });
        }

        if let Some(pointer) = plot_response.response.hover_pos() {
            if let Some(info) = bar_infos.iter().find(|b| b.rect.contains(pointer)) {
                let k = info.idx;
                let center_x = -((k as f64 + 0.5) / BPH as f64);
                let (val, fill, stroke_c) = match avg_pct[k] {
                    Some(avg) => (avg, green, stroke_green),
                    None => (0.5, empty, stroke_empty),
                };
                let hi_fill = Color32::from_rgb(
                    fill.r().saturating_add(30),
                    fill.g().saturating_add(20),
                    fill.b().saturating_add(30),
                );
                let lo = PlotPoint::new(center_x - half_w, 0.0);
                let hi = PlotPoint::new(center_x + half_w, val);
                let screen_rect = transform.rect_from_values(&lo, &hi);
                painter.add(egui::Shape::Rect(egui::epaint::RectShape::new(
                    screen_rect,
                    bar_rounding,
                    hi_fill,
                    egui::Stroke::new(1.0, stroke_c),
                )));

                let label = match avg_pct[k] {
                    Some(avg) => format!("{avg:.1}%"),
                    None => "no data".into(),
                };
                egui::show_tooltip_at_pointer(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new("battery_bar_tip"),
                    |ui| { ui.label(&label); },
                );
            }
        }

        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Each bar is the average battery % over 15 minutes, last 24 h. \
                 History persists across daemon restarts.",
            )
            .weak()
            .small(),
        );
    }
}

fn format_duration(secs: Option<u32>) -> String {
    let Some(s) = secs else { return "—".into() };
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{h}h{m:02}")
    } else {
        format!("{m}m")
    }
}


/// Compact `label: value` chunk. Both pieces use the default text size
/// (no `.small()`) so the row reads as a single typographic line;
/// `.weak()` on the label just dims it for hierarchy. The trailing
/// space lets `horizontal_wrapped` reflow chunks at chunk boundaries
/// rather than mid-pair.
fn pair(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(format!("{label}:")).weak().monospace());
    ui.label(RichText::new(value).monospace());
    ui.add_space(6.0);
}

fn fmt_w_opt(w: Option<f32>) -> String {
    match w {
        Some(v) => format!("{v:.1}W"),
        None => "—".into(),
    }
}

fn discover_coretemp() -> Option<PathBuf> {
    // hwmon ordering isn't stable; walk and match by name. We want
    // temp1_input from the "coretemp" device (Package id 0 on Intel).
    for entry in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let name_path = entry.path().join("name");
        let Ok(name) = fs::read_to_string(&name_path) else { continue };
        if name.trim() == "coretemp" {
            let temp_path = entry.path().join("temp1_input");
            if temp_path.exists() {
                return Some(temp_path);
            }
        }
    }
    None
}
