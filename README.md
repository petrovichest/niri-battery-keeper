# niri-battery-keeper

> ⚠️ **Work in progress — not production-ready.** This is an early personal
> project. Behaviour may be unstable, modes can leave background apps in
> unexpected states (frozen language servers, stuck IPC, etc.), and breaking
> config changes are likely between versions. Bug reports welcome, but don't
> trust your daily-driver workflow to it yet.

Focus-driven CPU/IO governor for unfocused apps on the [Niri](https://github.com/YaLTeR/niri)
Wayland compositor. Throttle or freeze background apps when you're not using
them; let them run free when you focus them back.

Built to extend battery life on Niri laptops where idle Electron apps,
language servers and background tabs would otherwise quietly drain your
charge.

One static binary (≈ 5 MB stripped). Rust + egui + `systemctl --user`.
Works on any modern Linux with systemd ≥ 246 and cgroup v2.

## What it does

- Listens to `niri msg --json event-stream` for window focus events.
- For each app, walks its process tree and discovers **every systemd scope**
  the app's processes live in — including detached helpers like language
  servers (so freezing VSCode actually freezes its `node`-based helpers too).
- Applies cgroup-v2 limits (`CPUQuota` / `CPUWeight` / `IOWeight`) or the
  cgroup freezer to all scopes of an app when it's unfocused. Reverses
  everything when you refocus.
- Two-level protection: the compositor itself (niri), the shell
  (DankMaterialShell / quickshell), portals, audio daemons, etc. are never
  managed, even when an app accidentally lives in their cgroup
  (e.g. Firefox opened via xdg-open from a notification).
- A small GUI lets you switch the global mode, exclude apps, and pin per-app
  rules. A CLI subcommand mirrors the same for keybinds.
- On shutdown, every scope it touched is reset to system defaults.

## Status

Early. Pre-1.0, in active development, **expected to break in fun ways**.
Works for the author on Niri 26.04 / Arch-family. Reports and patches
welcome.

## Requirements

- Linux with **systemd ≥ 246** (for `freeze`/`thaw` and `CPUQuota=` syntax)
- **cgroup v2** as the unified hierarchy (default on Arch, Fedora 32+, Ubuntu 21.10+, Debian 12+, etc.)
- **Niri** with `niri msg --json event-stream` (verified on 26.04)
- Wayland session
- Rust toolchain to build (`rustc` 1.80+ is enough)

## Build & install

```sh
git clone https://github.com/petrovichest/niri-battery-keeper.git
cd niri-battery-keeper
cargo build --release
install -Dm755 target/release/niri-battery-keeper ~/.local/bin/niri-battery-keeper
install -Dm644 systemd/niri-battery-keeper.service \
               ~/.config/systemd/user/niri-battery-keeper.service
systemctl --user daemon-reload
systemctl --user enable --now niri-battery-keeper.service
```

Default config is written to `~/.config/niri-battery-keeper/config.toml` on
first run. Mode defaults to **`off`** — the daemon does nothing until you
switch modes via the GUI or the CLI.

## Usage

```sh
niri-battery-keeper                        # open the GUI
niri-battery-keeper daemon                 # what the systemd unit runs
niri-battery-keeper status                 # print state and exit
niri-battery-keeper mode minimal           # background apps: 5% CPU
niri-battery-keeper mode pause             # background apps: frozen (0% CPU)
niri-battery-keeper mode off               # no restrictions
niri-battery-keeper disable                # KILL SWITCH on — release every scope, stop applying anything
niri-battery-keeper enable                 # KILL SWITCH off — resume normal operation
```

Useful Niri keybinds (`~/.config/niri/config.kdl`):

```
Mod+Shift+P { spawn "niri-battery-keeper" "mode" "pause"; }
Mod+Shift+M { spawn "niri-battery-keeper" "mode" "minimal"; }
Mod+Shift+O { spawn "niri-battery-keeper" "mode" "off"; }
Mod+Shift+K { spawn "niri-battery-keeper" "disable"; }   # panic button
Mod+Shift+J { spawn "niri-battery-keeper" "enable"; }
```

## Kill switch (panic button)

`disable` is a global override that beats every other setting. When engaged
it overrides `active_mode` AND every per-app `profile` / `use_mode` rule,
unfreezes/clears every scope the daemon had touched, and stops applying new
restrictions until you `enable` again. The daemon keeps running and tracking
focus events, so re-enabling is instant. State persists in `config.toml`
across daemon restarts.

Use this when you're experimenting with profiles and want a single
reliable knob that guarantees the program has zero effect on your system.
If the daemon itself is hung and unresponsive to IPC,
`systemctl --user stop niri-battery-keeper.service` runs the same cleanup
via SIGTERM.

## Default modes

| Mode      | Action              | CPU Quota | CPU Weight | IO Weight |
|-----------|---------------------|-----------|------------|-----------|
| off       | no restriction      | —         | —          | —         |
| minimal   | throttle            | 5%        | 5          | 5         |
| pause     | freeze cgroup       | —         | —          | —         |

`CPUQuota=5%` means 5% of a single core (50 ms/sec). `CPUWeight` and
`IOWeight` are systemd's relative scheduling weights (default 100). `pause`
uses the cgroup-v2 freezer — the process keeps its memory but consumes 0 %
CPU until you refocus it.

You can add custom presets, edit values, or change the action type in the
GUI's **Presets** tab.

## Per-app rules

Override the global mode for individual apps in `[apps.<id>]`. The `<id>`
is the `app_id` reported by Niri (`niri msg --json windows | jq '.[].app_id'`).

```toml
[apps."org.telegram.desktop"]
override = "exclude"               # never touch Telegram

[apps.firefox]
override = "profile"
profile  = "minimal"               # always Minimal for Firefox

[apps."code-oss"]
override = "use_mode"              # follow active_mode (default)
```

You can also edit these from the GUI's per-app card.

## How it stays out of trouble

- **Two-level protection.** A scope is skipped if either (a) the PID our
  resolver sees is a protected process, or (b) the scope itself contains any
  protected process. Protected processes: niri, quickshell/qs,
  xdg-desktop-portal\*, dbus-daemon, pipewire, wireplumber, pulseaudio,
  swaync/mako/dunst, waybar/eww, fuzzel/wofi/rofi, polkit agents, display
  managers, systemd.
- **`--runtime` set-property.** Limits live in the cgroup only, never in
  persistent unit drop-ins. A reboot wipes them.
- **`Slice=session.slice`** in the systemd unit: keeps the daemon out of
  `app.slice` so it can't accidentally throttle itself.
- **Stale-sweep on startup.** Every scope the daemon discovers is thawed and
  has properties cleared before reconciliation — so a previous crash can't
  leave you with a frozen Firefox.
- **On SIGTERM/SIGINT/SIGKILL**, the daemon clears every property and thaws
  every scope it set. (SIGKILL skips the cleanup, hence the stale-sweep.)
- **SIGHUP** reloads the config file.

## Shared scopes (Firefox-in-Telegram and friends)

When you click a link in Telegram, it spawns Firefox via `xdg-open`, and
systemd puts the new Firefox process **inside Telegram's scope**. They now
share a cgroup. A daemon trying to throttle Firefox would inevitably
throttle Telegram too.

niri-battery-keeper detects this and refuses to manage shared scopes
whenever any app in them is focused. The trade-off: a Firefox launched from
Telegram is also unmanaged when both are unfocused — the only way to fix
that is to launch Firefox separately first (so future xdg-open requests
route to its own scope via DBus).

## Limitations

- Only manages apps that systemd places in their own `app-*.scope` /
  `run-*.scope` under `app.slice`. Apps launched from a terminal share the
  terminal's scope and are skipped.
- A few apps don't reliably expose their child processes via
  `/proc/<pid>/task/<tid>/children` (children that detach and reparent to
  PID 1). Those helpers will keep running even in `pause` mode. Patches
  welcome.
- Wayland/Niri only. The throttle/freeze code is compositor-agnostic, but
  the focus source is Niri-specific.
- Two windows of the same app instance (e.g. two Firefox windows of the
  same process) cannot be controlled independently — cgroups limit
  processes, not individual windows. Use separate profiles
  (`firefox -P work --no-remote`, etc.) for that.

## Files

- `~/.config/niri-battery-keeper/config.toml` — your config
- `$XDG_RUNTIME_DIR/niri-battery-keeper.sock` — IPC socket
  (line-delimited JSON; the GUI and CLI talk to the daemon through it)
- `~/.config/systemd/user/niri-battery-keeper.service` — systemd user unit

## License

MIT.
