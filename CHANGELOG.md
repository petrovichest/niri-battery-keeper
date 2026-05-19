All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-20

First public release. Pre-1.0 / work in progress.

### Added
- Focus-driven CPU/IO governor for unfocused apps on the Niri Wayland
  compositor, driven by `niri msg --json event-stream`.
- Three default modes: `off`, `minimal` (throttle to 5% CPU / weights 5), and
  `pause` (cgroup-v2 freezer).
- Per-app overrides (`exclude` / `profile` / `use_mode`) configurable via
  CLI, config file, and GUI.
- Global kill switch (`niri-battery-keeper disable` / `enable`) that releases
  every managed scope and stops applying restrictions regardless of mode.
- GUI (`egui`) with **Apps** and **Presets** tabs, system-wide cgroup state
  view, collapsible orphan/protected sections, Simple/Advanced preset editor,
  and a Restart-daemon button.
- Two-level protection of the compositor and shell processes (niri,
  quickshell, xdg-desktop-portal, pipewire, dbus, polkit agents, etc.) — both
  by PID and by enclosing scope.
- Shared-scope detection: refuses to manage a scope that hosts more than one
  app when any of those apps is focused (Firefox-from-Telegram case).
- Wayland clipboard-owner exemption — the app currently holding the
  clipboard is never throttled, so paste keeps working after focus loss.
- Stale-sweep on startup: every discovered scope is thawed and cleared
  before the first reconcile, so a previous crash can't leave you with a
  frozen app.
- On SIGTERM/SIGINT every property is cleared and every scope thawed.
- HiDPI scale matching the system and faster scroll in the GUI.
- systemd user unit (`systemd/niri-battery-keeper.service`) running under
  `session.slice` so the daemon never throttles itself.

[Unreleased]: https://github.com/petrovichest/niri-battery-keeper/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/petrovichest/niri-battery-keeper/releases/tag/v0.1.0
