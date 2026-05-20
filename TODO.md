# Roadmap / TODO

Ideas captured for future iterations. Nothing here is committed to a specific
release.

## Self-bootstrap from a single binary

Goal: the user downloads one binary, runs it, and the systemd user service is
installed and started — no manual `install -Dm755 …` or `systemctl --user
enable` step. Conversely, removing the binary should leave no zombie service.

Done:

- ~~**Embed the unit in the binary.**~~ `include_str!` in `src/bootstrap.rs`;
  the binary now carries its own unit file. Release tarball still ships
  `systemd/` for the manual-install path — drop it once the README leads with
  `install`.
- ~~**`niri-battery-keeper install`.**~~ Copies `/proc/self/exe` →
  `~/.local/bin/niri-battery-keeper` (idempotent — skipped when already there),
  writes the embedded unit to
  `~/.config/systemd/user/niri-battery-keeper.service`, then `daemon-reload`
  and `enable --now`. Fails fast with a helpful message if `systemctl --user`
  isn't reachable.
- ~~**`niri-battery-keeper uninstall`.**~~ Best-effort `disable --now`,
  removes the unit and the binary copy. Leaves the config dir by default;
  `uninstall --purge` (or `-p`) wipes it too.

Still open:

- **GUI auto-bootstrap.** If the GUI is launched and the service isn't
  installed, prompt the user with a one-click "Install service" button (vs.
  doing it silently — open question, decide when we implement).
- **Service ↔ binary lifecycle.** The user's stated wish: "the service
  shouldn't run without the app." Concrete behaviour is undecided. Options:
  - Daemon checks on startup that `~/.local/bin/niri-battery-keeper` exists
    and exits if not (so a moved/removed binary doesn't leave a stale unit
    silently failing).
  - On `uninstall`, fully clean up so this never happens.
  - Pick one when we get there.
- **README rewrite.** Lead the "Install" section with
  `curl -L … -o niri-battery-keeper && chmod +x ./niri-battery-keeper &&
  ./niri-battery-keeper install`; demote the manual `install -Dm755 …` flow
  to a "from source" appendix.

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

The release binary is ~5 MB stripped and the running process is around 80 MB
RSS. Neither is alarming but both feel high for what this does (a focus
listener + cgroup writer + small egui UI). Worth a profiling pass before 1.0.

Open questions:

- **Daemon vs. GUI RSS.** Measure each separately — `ps -o pid,rss,comm -C
  niri-battery-keeper` while both run. 80 MB on the GUI is normal for an
  egui+glow app (GL context, font atlas, textures). 80 MB on the *daemon*
  would be surprising — it has no window — and worth digging into.
- **Binary breakdown.** `cargo bloat --release --crates` will show which
  dependencies dominate. Likely suspects: `eframe`/`egui` (~2–3 MB), `glow`
  (OpenGL bindings), `wayland-client` + `wayland-protocols-wlr`. The release
  profile is already aggressive (`opt-level = "z"`, `lto = "fat"`,
  `codegen-units = 1`, `panic = "abort"`, `strip = true`) so further wins
  come from cutting features, not flags.
- **Daemon heap profile.** If daemon RSS is suspicious, `heaptrack
  niri-battery-keeper daemon` (or `valgrind --tool=massif`) to see whether
  it's `UnitCache` growing unbounded, serde_json buffering the entire niri
  event stream, or process-tree scans not freeing.

Potential trims (only if measurement justifies):

- Split the GUI behind a feature flag and ship a daemon-only binary for users
  who never want the GUI. eframe/egui is most of the binary size.
- Replace `serde_json` with hand-rolled parsing for the small niri event
  schema (only a few message types).
- Drop `default_fonts` in egui and ship a smaller font.

## Other ideas

- **ARM64 / aarch64 builds.** Add a matrix to `.github/workflows/release.yml`.
  Cross-compile or run on a real arm64 runner.
- **Richer screenshots.** The current pair (Apps + Presets/Simple) is enough
  for v0.1.x; once the UI stabilises, capture Advanced editor and an Apps
  card with expanded Details so the README better conveys what the GUI does.
