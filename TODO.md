# Roadmap / TODO

Ideas captured for future iterations. Nothing here is committed to a specific
release.

## Self-bootstrap from a single binary

Goal: the user downloads one binary, runs it, and the systemd user service is
installed and started — no manual `install -Dm755 …` or `systemctl --user
enable` step. Conversely, removing the binary should leave no zombie service.

Pieces:

- **Embed the unit in the binary.**
  `include_str!("../systemd/niri-battery-keeper.service")` so the binary
  carries its own unit file. Drop the `systemd/` directory from the release
  tarball once this lands.
- **`niri-battery-keeper install`** — new CLI subcommand. Copy
  `/proc/self/exe` → `~/.local/bin/niri-battery-keeper` (idempotent), write
  the embedded unit to `~/.config/systemd/user/niri-battery-keeper.service`,
  shell out to `systemctl --user daemon-reload && enable --now
  niri-battery-keeper.service`. Refuse with a helpful message if `systemctl
  --user` isn't reachable (e.g. on a non-systemd distro).
- **`niri-battery-keeper uninstall`** — reverse: `stop`, `disable`, remove the
  unit, remove the binary copy from `~/.local/bin/`. Ask about the config
  before deleting it (`~/.config/niri-battery-keeper/`).
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

## Other ideas

- **ARM64 / aarch64 builds.** Add a matrix to `.github/workflows/release.yml`.
  Cross-compile or run on a real arm64 runner.
- **`.deb` / `.rpm` packages** via `cargo-deb` / `cargo-generate-rpm`. Useful
  for users who want their package manager to track the install.
- **Richer screenshots.** The current pair (Apps + Presets/Simple) is enough
  for v0.1.x; once the UI stabilises, capture Advanced editor and an Apps
  card with expanded Details so the README better conveys what the GUI does.
