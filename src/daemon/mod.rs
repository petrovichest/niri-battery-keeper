pub mod niri;
pub mod cgroup;
pub mod throttle;
pub mod ipc;
pub mod proctree;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::proto::{self, AppGroupInfo, DaemonState, Request, Response, ScopeInfo, WindowInfo};
use cgroup::UnitCache;
use throttle::Throttler;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_term(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}
extern "C" fn handle_hup(_sig: libc::c_int) {
    RELOAD.store(true, Ordering::SeqCst);
}

fn install_signals() {
    let term_handler = handle_term as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
    let hup_handler  = handle_hup  as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, term_handler);
        libc::signal(libc::SIGINT, term_handler);
        libc::signal(libc::SIGHUP, hup_handler);
        // Ignore SIGPIPE — we'd rather get EPIPE on write
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

enum DaemonMsg {
    Niri(niri::Event),
    Ipc(ipc::IpcMessage),
}

#[derive(Default, Debug)]
struct AppEntry {
    windows: HashSet<u64>,
    /// scope name -> count of PIDs in it (for diagnostics)
    scopes: HashMap<String, usize>,
    focused: bool,
}

#[derive(Default)]
struct State {
    windows: HashMap<u64, niri::Window>,
    focused_id: Option<u64>,
    apps: HashMap<String, AppEntry>,
    /// app_id -> deadline when this app's scopes should be throttled
    pending: HashMap<String, Instant>,
    cache: UnitCache,
    throttler: Throttler,
}

impl State {
    fn new() -> Self { Self::default() }

    fn apply_snapshot(&mut self, windows: Vec<niri::Window>) {
        let new_ids: HashSet<u64> = windows.iter().map(|w| w.id).collect();
        let gone: Vec<u64> = self.windows.keys().copied().filter(|id| !new_ids.contains(id)).collect();
        for id in gone {
            self.forget_window(id);
        }
        for w in windows {
            self.upsert_window(w);
        }
        self.recompute_focused();
        self.refresh_all_app_scopes();
    }

    fn upsert_window(&mut self, w: niri::Window) {
        let prev_app_id = self.windows.get(&w.id).map(|x| x.app_id.clone());
        if let Some(prev) = prev_app_id {
            if prev != w.app_id {
                self.detach_window_from_app(w.id, &prev);
            }
        }
        let app = self.apps.entry(w.app_id.clone()).or_default();
        app.windows.insert(w.id);
        self.windows.insert(w.id, w);
    }

    fn detach_window_from_app(&mut self, window_id: u64, app_id: &str) {
        let drop_app = if let Some(app) = self.apps.get_mut(app_id) {
            app.windows.remove(&window_id);
            app.windows.is_empty()
        } else { false };
        if drop_app {
            if let Some(app) = self.apps.remove(app_id) {
                self.pending.remove(app_id);
                for scope in app.scopes.keys() {
                    if self.throttler.is_throttled(scope) {
                        self.throttler.reset(scope);
                    }
                }
            }
        }
    }

    fn forget_window(&mut self, id: u64) {
        if let Some(w) = self.windows.remove(&id) {
            if let Some(pid) = w.pid { self.cache.invalidate(pid); }
            let app_id = w.app_id.clone();
            self.detach_window_from_app(id, &app_id);
        }
    }

    fn recompute_focused(&mut self) {
        self.focused_id = self.windows.values().find(|w| w.is_focused).map(|w| w.id);
        // Recompute per-app focused flag.
        for app in self.apps.values_mut() {
            app.focused = false;
        }
        for w in self.windows.values() {
            if w.is_focused {
                if let Some(app) = self.apps.get_mut(&w.app_id) {
                    app.focused = true;
                }
            }
        }
    }

    fn set_focus(&mut self, id: Option<u64>) {
        for (wid, w) in self.windows.iter_mut() {
            w.is_focused = Some(*wid) == id;
        }
        self.focused_id = id;
        // Sync per-app focused flags.
        for app in self.apps.values_mut() { app.focused = false; }
        for w in self.windows.values() {
            if w.is_focused {
                if let Some(app) = self.apps.get_mut(&w.app_id) {
                    app.focused = true;
                }
            }
        }
    }

    /// Re-walk descendant trees of every app and refresh its scope set.
    /// Scopes that disappear get unthrottled.
    fn refresh_all_app_scopes(&mut self) {
        let app_ids: Vec<String> = self.apps.keys().cloned().collect();
        for app_id in app_ids {
            self.refresh_app_scopes(&app_id);
        }
    }

    fn refresh_app_scopes(&mut self, app_id: &str) {
        let pids: Vec<i32> = match self.apps.get(app_id) {
            Some(app) => app.windows.iter()
                .filter_map(|wid| self.windows.get(wid).and_then(|w| w.pid))
                .collect(),
            None => return,
        };
        let mut new_scopes: HashMap<String, usize> = HashMap::new();
        for pid in pids {
            for (scope, count) in self.cache.resolve_scopes_for_tree(pid) {
                *new_scopes.entry(scope).or_insert(0) += count;
            }
        }
        let app = self.apps.get_mut(app_id).unwrap();
        let removed: Vec<String> = app.scopes.keys()
            .filter(|s| !new_scopes.contains_key(*s))
            .cloned()
            .collect();
        app.scopes = new_scopes;
        for scope in removed {
            if self.throttler.is_throttled(&scope) {
                self.throttler.reset(&scope);
            }
        }
    }

    fn reconcile(&mut self, config: &Config) {
        let now = Instant::now();
        let grace = Duration::from_millis(config.policy.unfocus_grace_ms);
        let app_ids: Vec<String> = self.apps.keys().cloned().collect();

        for app_id in app_ids {
            let (focused, scopes): (bool, Vec<String>) = {
                let app = &self.apps[&app_id];
                (app.focused, app.scopes.keys().cloned().collect())
            };
            let profile = config.resolve_profile(&app_id);

            if focused || profile.is_none() {
                self.pending.remove(&app_id);
                for scope in &scopes {
                    if self.throttler.is_throttled(scope) {
                        self.throttler.reset(scope);
                    }
                }
            } else {
                let prof = profile.unwrap();
                let any_throttled = scopes.iter().any(|s| self.throttler.is_throttled(s));
                if any_throttled {
                    for scope in &scopes {
                        if let Err(e) = self.throttler.apply(scope, &prof) {
                            log::warn!("reapply {scope}: {e}");
                        }
                    }
                } else if !self.pending.contains_key(&app_id) {
                    self.pending.insert(app_id, now + grace);
                }
            }
        }
    }

    fn process_pending(&mut self, config: &Config) {
        let now = Instant::now();
        let due: Vec<String> = self.pending
            .iter()
            .filter_map(|(a, d)| if *d <= now { Some(a.clone()) } else { None })
            .collect();
        for app_id in due {
            self.pending.remove(&app_id);
            let (focused, scopes) = match self.apps.get(&app_id) {
                Some(a) => (a.focused, a.scopes.keys().cloned().collect::<Vec<_>>()),
                None => continue,
            };
            if focused { continue; }
            let profile = match config.resolve_profile(&app_id) {
                Some(p) => p, None => continue,
            };
            for scope in scopes {
                if let Err(e) = self.throttler.apply(&scope, &profile) {
                    log::warn!("apply {scope}: {e}");
                }
            }
        }
    }

    fn next_timeout(&self) -> Duration {
        let now = Instant::now();
        self.pending.values().min()
            .map(|d| d.saturating_duration_since(now).max(Duration::from_millis(10)))
            .unwrap_or(Duration::from_secs(2))
    }

    fn snapshot_for_ipc(&self, config: &Config) -> DaemonState {
        let throttled = self.throttler.throttled_units();
        let throttled_set: HashSet<&String> = throttled.iter().collect();

        let mut windows: Vec<WindowInfo> = self.windows.values().map(|w| {
            let excluded = config.resolve_profile(&w.app_id).is_none();
            WindowInfo {
                window_id: w.id,
                app_id: w.app_id.clone(),
                title: w.title.clone(),
                pid: w.pid,
                focused: w.is_focused,
                unit: self.apps.get(&w.app_id)
                    .and_then(|a| a.scopes.keys().next().cloned()),
                throttled: self.apps.get(&w.app_id)
                    .map(|a| a.scopes.keys().any(|s| throttled_set.contains(s)))
                    .unwrap_or(false),
                excluded,
            }
        }).collect();
        windows.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(a.window_id.cmp(&b.window_id)));

        let mut apps: Vec<AppGroupInfo> = self.apps.iter().map(|(app_id, app)| {
            let profile = config.resolve_profile(app_id);
            let mut scopes: Vec<ScopeInfo> = app.scopes.iter().map(|(name, count)| {
                ScopeInfo {
                    unit: name.clone(),
                    pid_count: *count,
                    throttled: throttled_set.contains(name),
                }
            }).collect();
            scopes.sort_by(|a, b| a.unit.cmp(&b.unit));
            AppGroupInfo {
                app_id: app_id.clone(),
                window_count: app.windows.len(),
                focused: app.focused,
                excluded: profile.is_none(),
                any_throttled: scopes.iter().any(|s| s.throttled),
                scopes,
            }
        }).collect();
        apps.sort_by(|a, b| a.app_id.cmp(&b.app_id));

        DaemonState {
            active_mode: config.active_mode.clone(),
            config: config.clone(),
            windows,
            apps,
            throttled_units: throttled,
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    install_signals();
    let mut config = Config::load_or_default();
    log::info!("loaded config, active_mode={}", config.active_mode);

    let (msg_tx, msg_rx) = channel::<DaemonMsg>();

    // Niri event-stream → DaemonMsg::Niri
    {
        let tx = msg_tx.clone();
        let (niri_tx, niri_rx) = channel::<niri::Event>();
        niri::spawn_event_stream(niri_tx);
        thread::spawn(move || {
            while let Ok(ev) = niri_rx.recv() {
                if tx.send(DaemonMsg::Niri(ev)).is_err() { break; }
            }
        });
    }

    // IPC server → DaemonMsg::Ipc
    {
        let tx = msg_tx.clone();
        let ipc_rx = ipc::start()?;
        thread::spawn(move || {
            while let Ok(m) = ipc_rx.recv() {
                if tx.send(DaemonMsg::Ipc(m)).is_err() { break; }
            }
        });
    }

    let mut state = State::new();

    // Bootstrap snapshot
    match niri::fetch_windows() {
        Ok(ws) => {
            log::info!("bootstrap: {} window(s)", ws.len());
            state.apply_snapshot(ws);

            // Stale-sweep: any scope we now know about may carry leftovers
            // from a prior daemon run that didn't shut down cleanly (frozen,
            // residual CPUQuota, etc.). Take ownership by clearing them.
            let scopes: HashSet<String> = state.apps.values()
                .flat_map(|a| a.scopes.keys().cloned())
                .collect();
            if !scopes.is_empty() {
                log::info!("stale-sweep: clearing state on {} scope(s)", scopes.len());
                for scope in scopes {
                    state.throttler.force_clear(&scope);
                }
            }

            state.reconcile(&config);
        }
        Err(e) => log::warn!("bootstrap snapshot failed: {e}"),
    }

    log::info!("daemon ready");

    while !SHUTDOWN.load(Ordering::SeqCst) {
        if RELOAD.swap(false, Ordering::SeqCst) {
            log::info!("SIGHUP: reloading config");
            config = Config::load_or_default();
            state.reconcile(&config);
        }

        let timeout = state.next_timeout();
        match msg_rx.recv_timeout(timeout) {
            Ok(DaemonMsg::Niri(ev)) => {
                handle_niri_event(&mut state, ev, &config);
            }
            Ok(DaemonMsg::Ipc(m)) => {
                handle_ipc(&mut state, &mut config, m);
            }
            Err(RecvTimeoutError::Timeout) => {
                state.process_pending(&config);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    log::info!("shutting down — resetting throttled units");
    state.throttler.reset_all();
    let _ = std::fs::remove_file(proto::socket_path());
    Ok(())
}

fn handle_niri_event(state: &mut State, ev: niri::Event, config: &Config) {
    match ev {
        niri::Event::Snapshot(ws) => {
            log::debug!("snapshot: {} window(s)", ws.len());
            state.apply_snapshot(ws);
            state.reconcile(config);
        }
        niri::Event::Upsert(w) => {
            log::trace!("upsert: id={} app_id={} focused={}", w.id, w.app_id, w.is_focused);
            let app_id = w.app_id.clone();
            state.upsert_window(w);
            state.recompute_focused();
            state.refresh_app_scopes(&app_id);
            state.reconcile(config);
        }
        niri::Event::Closed(id) => {
            log::trace!("closed: id={id}");
            state.forget_window(id);
            state.reconcile(config);
        }
        niri::Event::Focus(id) => {
            log::debug!("focus → {:?}", id);
            state.set_focus(id);
            state.reconcile(config);
        }
        niri::Event::StreamLost => {
            log::warn!("niri stream lost, clearing pending");
            state.pending.clear();
        }
    }
}

fn handle_ipc(state: &mut State, config: &mut Config, m: ipc::IpcMessage) {
    let resp = match m.req {
        Request::GetState => Response::State(state.snapshot_for_ipc(config)),
        Request::SetMode { mode } => {
            if !config.modes.contains_key(&mode) {
                Response::Error { message: format!("unknown mode '{mode}'") }
            } else {
                config.active_mode = mode;
                if let Err(e) = config.save_to(&Config::path()) {
                    log::warn!("could not save config: {e}");
                }
                state.reconcile(config);
                Response::Ok
            }
        }
        Request::SetConfig { config: new_cfg } => {
            *config = new_cfg;
            if let Err(e) = config.save_to(&Config::path()) {
                log::warn!("could not save config: {e}");
            }
            state.reconcile(config);
            Response::Ok
        }
        Request::Reload => {
            *config = Config::load_or_default();
            state.reconcile(config);
            Response::Ok
        }
    };
    let _ = m.reply.send(resp);
}
