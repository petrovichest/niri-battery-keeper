# Roadmap / TODO

Ideas captured for future iterations. Nothing here is committed to a specific
release.

## Self-bootstrap from a single binary

Goal: the user downloads one binary, runs it, and the systemd user service is
installed and started — no manual `install -Dm755 …` or `systemctl --user
enable` step. Conversely, removing the binary should leave no zombie service.

Done:

- ~~**Embed the unit in the binary.**~~ `include_str!` in `src/bootstrap.rs`;
  the binary now carries its own unit file.
- ~~**`niri-battery-keeper install`.**~~ Copies `/proc/self/exe` →
  `~/.local/bin/niri-battery-keeper` (idempotent — skipped when already there),
  writes the embedded unit to
  `~/.config/systemd/user/niri-battery-keeper.service`, then `daemon-reload`
  and `enable --now`. Fails fast with a helpful message if `systemctl --user`
  isn't reachable.
- ~~**`niri-battery-keeper uninstall`.**~~ Best-effort `disable --now`,
  removes the unit and the binary copy. Leaves the config dir by default;
  `uninstall --purge` (or `-p`) wipes it too.
- ~~**GUI auto-bootstrap.**~~ When the systemd user unit is missing, the GUI
  shows a top banner with an "Install service" button that calls the same
  `bootstrap::install()` path. Explicit-consent flavour rather than silent
  install — surface what's about to mutate before mutating it.
- ~~**README rewrite.**~~ "Install" section now leads with the self-bootstrap
  flow (`curl … && chmod +x && ./niri-battery-keeper install`); the manual
  `install -Dm755 …` recipe demoted to a "Manual install" appendix under
  "Build from source".

Dropped:

- ~~**Daemon-side lifecycle check** (binary-at-canonical-path probe).~~
  Redundant: `ExecStart=%h/.local/bin/niri-battery-keeper daemon` already
  means systemd can't start the unit if the binary is gone, and a check
  inside the daemon would only fire from `target/release/` development runs
  where we don't want to fail. Cleanup is the `uninstall` subcommand's job.

Still open:

- (none — section ready to retire once the next release ships the
  slimmer tarball and the GUI-only entry point)

Done in [Unreleased]:

- ~~**Drop `systemd/` from the release tarball.**~~ The unit is embedded
  in the binary and written by the GUI's "Install service" button, so
  the tarball's `systemd/` copy was dead weight. README's "Manual
  install" path lives under "Build from source" and uses the in-repo
  `systemd/` — unchanged.
- ~~**Collapse all lifecycle into the GUI.**~~ Removed the `install` /
  `uninstall` / `mode` / `disable` / `enable` / `status` CLI
  subcommands. Everything user-facing now lives in the GUI; the binary
  itself only knows `niri-battery-keeper` (open GUI) and
  `niri-battery-keeper daemon` (systemd entry point).

## Packaging / distribution

Goal: a user on any common distro should be able to install via the tool they
already use — not by reading a README and running four shell commands.

Done:

- ~~**AUR `-bin` package.**~~ `yay -S niri-battery-keeper-bin`. PKGBUILD
  in `packaging/aur/niri-battery-keeper-bin/`, CI auto-pushes on each
  release tag using `KSXGitHub/github-actions-deploy-aur`. Requires
  one-time setup of `AUR_SSH_PRIVATE_KEY` GitHub secret.
- ~~**`.deb` artifact.**~~ `cargo-deb` builds in CI;
  `apt install ./niri-battery-keeper_*.deb` works on Debian 12+ /
  Ubuntu 22.04+ / Mint.
- ~~**`.rpm` artifact.**~~ `cargo-generate-rpm` builds in CI;
  `dnf install ./niri-battery-keeper-*.rpm` works on Fedora / openSUSE.
- ~~**Desktop integration.**~~ `.desktop` + SVG icon shipped under
  `/usr/share/` (packages) or `~/.local/share/` (bare-binary install).
  All install paths now produce an app-menu entry.
- ~~**Broaden glibc compat.**~~ Release builds run on `ubuntu-22.04`
  (glibc 2.35) instead of `ubuntu-latest`.

Still open:

- **AUR source package** `niri-battery-keeper` — builds from the latest
  release tarball with the user's local Rust. Less critical now that
  `-bin` is live, but useful for source-purity audiences.
- **AUR `-git` package** — tracks `main`. Trivial copy of `-bin` with
  source pointing at the git repo and a `pkgver()` function for
  `git describe` versioning.
- **Nix flake.** NixOS users overlap heavily with the Niri crowd. A
  `flake.nix` exposing `packages.x86_64-linux.default` (a Rust derivation)
  and a `nixosModules.niri-battery-keeper` that wires the systemd user
  unit makes `nix run github:petrovichest/niri-battery-keeper` and
  `services.niri-battery-keeper.enable = true;` work.
- **AppImage.** Universal Linux single-file. Build with `linuxdeploy` +
  `appimagetool` in CI; caveat: the unit writes a path that AppImages
  rename across versions, so needs either `$APPIMAGE` env or path
  rewrite on first launch.
- **`cargo install`.** Already works (`cargo install --git …`) but
  requires Rust. Documented as the source-build path.
- **Drop bare-binary on Releases?** Reconsider once Nix flake lands —
  with AUR + .deb + .rpm + Nix the bare binary is fallback for ≤5%
  of users (Gentoo, Void, Slackware). Removing it would let us delete
  most of `bootstrap.rs` and the install-banner code.

Not pursuing (sandbox is fundamentally hostile to what this app does):

- **Flatpak / Snap.** Both confine apps in ways that prevent talking to
  the user's systemd, reading other apps' cgroups, or running `niri msg`.

## Investigate footprint — RAM and binary size

Measured 2026-05-20 (eframe 0.29, glow, wayland; release profile already at
`opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
`strip = true`):

| Process | RSS    | PSS    | Pss_Anon | Shared_Clean |
|---------|--------|--------|----------|--------------|
| daemon  | 6.4 MB | 2.5 MB | 1.0 MB   | 5.4 MB       |
| GUI     | 100 MB | 32 MB  | 24 MB    | 76 MB        |

Binary (stripped): 5.5 MB total — `.text` 2.6 MB, `.rodata` ~2.5 MB
(of which ~1 MB is `default_fonts`: Hack + Ubuntu-Light + NotoEmoji +
emoji-icon-font).

Resolved findings:

- **The 80 MB worry was about the wrong number.** `ps`'s RSS double-counts
  pages shared with other GL apps. The GUI's PSS is 32 MB (24 MB private
  heap + ~8 MB its share of Mesa/libstdc++/etc.); the 75 MB Shared_Clean is
  the Mesa GL stack (`libgallium`, `libLLVM`, `libicudata` via `libxml2`,
  `libEGL_mesa`, `libdrm_amdgpu`+`libdrm_intel`) which is loaded once for
  the whole system anyway. Any egui/eframe/glow app pays this.
- **Daemon is fine.** 2.5 MB PSS, 1 MB anon. No leak hunt needed.
- **Top crates by `.text`:** std 667 KB, winit 314 KB, egui 232 KB,
  eframe 177 KB, niri_battery_keeper 118 KB, epaint 87 KB,
  smithay_clipboard 85 KB, x11_dl 72 KB, ttf_parser 70 KB, toml_edit 70 KB,
  x11rb_protocol 68 KB, egui_winit 60 KB, wayland_client 54 KB,
  webbrowser 35 KB.

Dropped trims:

- ~~**Drop `x11` from eframe features.**~~ Tried; saved 160 bytes. `winit`
  and `glutin-winit` have `x11` in their default features, so the X11 code
  paths (`x11_dl`, `x11rb`, `arboard`'s X11 backend, winit's X11 platform)
  stay in the binary even when eframe's `x11` feature is off. Removing them
  would require forking eframe or `[patch.crates-io]` on glutin-winit/winit
  to disable defaults — disproportionate for ~250 KB. The `x11 = false`
  setting is kept in `Cargo.toml` as semantically correct: when the
  upstream default-features situation improves, we get the win for free.
- ~~**Drop `default_fonts`.**~~ Would save ~1 MB but breaks CollapsingHeader
  arrows, checkmarks, and emoji fallback — egui ships glyph icons in
  `emoji-icon-font`. Already paired with system symbol fonts (91b7a89);
  losing the bundled set degrades UX.

Still open (only if size becomes a real problem before 1.0):

- **Replace `serde_json` with hand-rolled parsing** for the small niri
  event schema. Estimated win: ~20–30 KB; risk: regressions in event
  parsing. Not worth it at current sizes.
- **Wait for eframe/winit upstream** to expose `default-features = false`
  on `glutin-winit` (or for egui-winit to gate `arboard`/`webbrowser`/
  X11 behind toggles). Then revisit and potentially save ~300–400 KB.

## Next iteration (planned)

User's punch list captured 2026-05-21:

- **TDP UI — drop the sliders.** Replace the slider widgets in the TDP tab
  with numeric input + preset chips (15 W / 25 W / 35 W / Max). Sliders
  encourage twiddling; the real workflow is pick-a-preset.
- **Battery consumption graph + per-app energy log.** Sample power draw
  (`/sys/class/power_supply/BAT*/power_now`, or compute from `energy_now`
  deltas) on a short cadence, render a rolling timeline in the GUI, and
  attribute drain to apps by proportioning RAPL package-energy across the
  per-scope `cpu.stat` usage we already read. Persist to
  `~/.local/share/niri-battery-keeper/history.{db,jsonl}`. Adds a "what's
  draining my battery" answer the app currently hand-waves.
- **Per-app / focused-window TDP profile.** When app X is focused, switch
  PL1/PL2 to profile X; when Y is focused, switch to Y. Generalises the
  current global TDP into the same per-app rule shape used by the cgroup
  throttler, and pairs naturally with the consumption log above.
- **Redesigned app icon.** Current `assets/niri-battery-keeper.svg` is a
  12-line placeholder (battery outline + lightning). Commission/draw a
  proper one that matches Niri's geometric language.
- **Refresh README screenshots.** Recapture Apps + Presets + TDP + new
  battery graph view once the items above land.
- **Design pass for visual consistency.** Audit spacing, header style,
  accent colours, button shapes across Apps / Presets / TDP / Settings;
  pick one system and apply everywhere.
- **System tray indicator.** Battery %, current mode, and a right-click
  menu to switch modes / TDP profiles without opening the GUI. Most useful
  "quiet" surface for an app that mostly runs in the background. Native
  StatusNotifierItem (KDE / wlroots-friendly) via the `ksni` crate, or
  `system-tray` if we want a more direct D-Bus impl.
- **AMD RAPL support.** `rapl_helper.rs` is Intel-only today; `amd-rapl`
  zones exist under `/sys/class/powercap/` on Zen 4+. Detect the available
  zone in the helper, generalise the PL1/PL2 abstraction (AMD exposes
  `constraint_0_power_limit_uw` similarly but caps and semantics differ),
  and keep one code path for both. Roughly doubles addressable hardware.
- **Localization (ru-RU).** Project author works in Russian; the GUI is
  small enough (a few dozen strings) that adding `fluent-rs` (or a simple
  static-table `t!()` macro) now is cheaper than retrofitting later.
  Default to system locale; fall back to en-US.

Suggestions to consider alongside the above:

- **Battery time-remaining estimate.** Falls out of the consumption log
  almost for free; show in tray and main window.
- **Notifications on auto mode/TDP switch.** Short desktop notification
  (libnotify / `notify-rust`) when something changes under the user's feet,
  so quiet CPU never feels like a bug.
- **Power-source-aware default profile.** AC plugged in → unrestricted
  mode + max TDP; on battery → user's chosen default. Currently the user
  has to flip it manually each time they unplug.
- **CSV / JSON export of per-app energy history.** Once we log it,
  exporting is ~free and gives users data they can actually act on.

## Other ideas

- **ARM64 / aarch64 builds.** Add a matrix to `.github/workflows/release.yml`.
  Cross-compile or run on a real arm64 runner.
- **Richer screenshots.** The current pair (Apps + Presets/Simple) is enough
  for v0.1.x; once the UI stabilises, capture Advanced editor and an Apps
  card with expanded Details so the README better conveys what the GUI does.
