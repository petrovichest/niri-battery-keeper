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
already use — not by reading a README and running four shell commands. The
bare-binary asset published today works but lands with no `+x`, an
unwieldy name, and no systemd unit. Channels to support, ordered by audience
fit:

- **AUR (Arch).** The natural home — Niri is Arch-leaning and the author runs
  Arch. Three packages is the convention:
  - `niri-battery-keeper-bin` — uses the pre-built release binary, fastest
    install for end users (`yay -S niri-battery-keeper-bin`).
  - `niri-battery-keeper` — builds from the latest release tarball with the
    user's local Rust.
  - `niri-battery-keeper-git` — tracks the `main` branch.
    PKGBUILDs should `install -Dm755` the binary, drop the systemd unit into
    `/usr/lib/systemd/user/`, and ship `config.example.toml` under
    `/usr/share/doc/niri-battery-keeper/`. Worth extending the release workflow
    to also bump the AUR PKGBUILDs (e.g. via a separate AUR repo and an SSH
    push step in CI).
- **AppImage.** Universal Linux single-file: ship `niri-battery-keeper.AppImage`
  as a release asset. Build with `linuxdeploy` + `appimagetool` in CI; the
  AppImage runtime handles the executable bit and double-click integration in
  file managers. Caveat: the daemon writes a systemd user unit pointing at an
  AppImage path that can move — needs to either bake an absolute path on
  first launch or use `$APPIMAGE` env var.
- **Nix flake.** NixOS users overlap heavily with the Niri crowd. A `flake.nix`
  exposing `packages.x86_64-linux.default` (a Rust derivation) and a
  `nixosModules.niri-battery-keeper` that wires the systemd user unit makes
  `nix run github:petrovichest/niri-battery-keeper` and
  `services.niri-battery-keeper.enable = true;` work.
- **`.deb` / `.rpm`** via `cargo-deb` and `cargo-generate-rpm`. Easy to add
  to the release workflow alongside the existing tarball/binary, useful for
  Debian/Ubuntu and Fedora users even without uploading to a PPA/COPR.
- **`cargo install`.** Already works today (`cargo install --git …`) but
  requires Rust. Document this as an option for the source-build path; if
  we ever publish to crates.io it becomes `cargo install niri-battery-keeper`.

Not pursuing (sandbox is fundamentally hostile to what this app does):

- **Flatpak / Snap.** Both confine apps in ways that prevent talking to the
  user's systemd, reading other apps' cgroups, or running `niri msg`.

Once one of these channels is live, update README "Install" section to lead
with it and demote the manual tarball steps.

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

## Other ideas

- **ARM64 / aarch64 builds.** Add a matrix to `.github/workflows/release.yml`.
  Cross-compile or run on a real arm64 runner.
- **Richer screenshots.** The current pair (Apps + Presets/Simple) is enough
  for v0.1.x; once the UI stabilises, capture Advanced editor and an Apps
  card with expanded Details so the README better conveys what the GUI does.
