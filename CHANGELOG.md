All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **AUR `-bin` package** (`yay -S niri-battery-keeper-bin`). PKGBUILD
  pulls the published x86_64 binary and the unit/desktop/icon from the
  matching git tag, installs to `/usr/bin/` + `/usr/lib/systemd/user/` +
  `/usr/share/applications/` + `/usr/share/icons/hicolor/scalable/apps/`.
  CI auto-pushes a new package version on every release tag.
- **`.deb` and `.rpm` artifacts** on GitHub Releases. `cargo-deb` and
  `cargo-generate-rpm` build them in CI alongside the bare binary;
  install with `apt install ./niri-battery-keeper_*.deb` or
  `dnf install ./niri-battery-keeper-*.rpm`.
- **Application menu entry.** The bundled `.desktop` file and SVG icon
  show "Niri Battery Keeper" in app menus and launchers. AUR / deb / rpm
  ship them to `/usr/share/`; the bare-binary "Install service" flow
  writes them to `~/.local/share/`.
- **Auto-detect install state.** The GUI now inspects `/proc/self/exe`
  on startup. When running from `/usr/bin/` it skips the "Install
  service" banner entirely and offers "Enable autostart" only if the
  package's unit is present but not enabled. Prevents stale copies in
  `~/.local/bin/` when a package-managed binary gets upgraded.

### Changed
- Release builds now run on **ubuntu-22.04** (glibc 2.35) instead of
  ubuntu-latest (24.04, glibc 2.39). The bare binary now runs on
  Ubuntu 22.04+, Debian 12+, Fedora 36+ in addition to current Arch /
  Fedora / Ubuntu LTS.

## [0.2.0] — 2026-05-20

### Added
- **TDP tab** in the GUI: PL1/PL2 sliders, live CPU temperature, live
  wattage (delta of `intel-rapl` energy counters), and an Apply button.
  Writes Intel RAPL power limits via a privileged helper invoked through
  pkexec — useful for quieting the fan by capping sustained CPU power.
  PL1 can be set to 0 to disable the long-term constraint and run with
  PL2 as the sole cap.
- **One-click TDP setup** via a single pkexec prompt. The GUI's
  "Install TDP helper" button lays down all three system-level files at
  once: a root-owned copy of the main binary at
  `/usr/local/bin/nbk-set-rapl` (multi-call dispatch — same binary,
  different name routes to the privileged code path), the polkit policy
  at `/usr/share/polkit-1/actions/...set-rapl.policy` with
  `auth_admin_keep` (5-min password cache), and a udev rule at
  `/etc/udev/rules.d/60-intel-rapl-energy.rules` opening `energy_uj`
  to the wheel group so the live wattage readout works without root.
- Polkit-agent detection in the TDP tab. When no authentication agent
  is running in the Niri session, a yellow banner appears with the
  exact install command for `hyprpolkitagent`.

### Changed
- GUI is now the only user-facing entry point. All CLI subcommands —
  `install`, `uninstall`, `mode`, `disable`, `enable`, `status` — were
  removed. Mode switching, kill switch, install/uninstall, status all
  live in the GUI. The only remaining invocations are
  `niri-battery-keeper` (opens GUI) and `niri-battery-keeper daemon`
  (what the systemd unit runs).
- Release tarball no longer ships a `systemd/` directory. The unit is
  embedded in the binary and written to
  `~/.config/systemd/user/niri-battery-keeper.service` by the GUI's
  "Install service" button, so the duplicate copy in the tarball was
  dead weight. Source builds still have `systemd/niri-battery-keeper.service`
  in the repo for the manual-install path.

### Removed
- CLI subcommands `install`, `uninstall [--purge]`, `mode <name>`,
  `disable`, `enable`, `status`. Niri keybind recipes that spawned
  `niri-battery-keeper mode pause` etc. no longer work — bind the GUI
  instead (`spawn "niri-battery-keeper";`).

## [0.1.1] — 2026-05-20

### Added
- Standalone binary asset `niri-battery-keeper-x86_64-linux` published
  alongside the tarball, so `curl -LO` works without untarring.
- "Install (from release)" section in README pointing at
  `releases/latest/download/…`, so users without a Rust toolchain don't
  have to build from source.

### Changed
- Release tarball renamed to `niri-battery-keeper-x86_64-linux.tar.gz`
  (no version in filename) so the GitHub `latest/download` URL is stable.
  The folder inside still carries the version.

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

[Unreleased]: https://github.com/petrovichest/niri-battery-keeper/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/petrovichest/niri-battery-keeper/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/petrovichest/niri-battery-keeper/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/petrovichest/niri-battery-keeper/releases/tag/v0.1.0
