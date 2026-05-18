use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub id: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub title: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub app_id: String,
    pub pid: Option<i32>,
    #[serde(default)]
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
}

/// Niri occasionally reports `"app_id": null` / `"title": null` (e.g. for
/// windows that haven't set them yet, like xdg-shell popups in flight).
/// Default `#[serde(default)]` only handles missing fields, not explicit
/// nulls, so the whole bootstrap parse used to fail on a single such
/// window and skip the stale-sweep. Treat null as the default value.
fn null_to_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(|o| o.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawEvent {
    Known(KnownEvent),
    /// Anything we don't care about (WorkspacesChanged, KeyboardLayoutsChanged, etc.)
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize)]
enum KnownEvent {
    WindowsChanged { windows: Vec<Window> },
    WindowOpenedOrChanged { window: Window },
    WindowClosed { id: u64 },
    WindowFocusChanged { id: Option<u64> },
}

/// Typed events emitted into the daemon's main loop.
#[derive(Debug, Clone)]
pub enum Event {
    /// Full snapshot, replace any in-memory state.
    Snapshot(Vec<Window>),
    /// Window added or updated.
    Upsert(Window),
    /// Window removed.
    Closed(u64),
    /// Focus changed to this window id, or to nothing if `None`.
    Focus(Option<u64>),
    /// Niri event-stream died and is being respawned.
    StreamLost,
}

/// Fetch the current windows snapshot via a one-shot `niri msg --json windows`.
pub fn fetch_windows() -> std::io::Result<Vec<Window>> {
    let out = Command::new("niri")
        .args(["msg", "--json", "windows"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("niri msg windows failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        ));
    }
    let windows: Vec<Window> = serde_json::from_slice(&out.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(windows)
}

/// Spawn a background thread that runs `niri msg --json event-stream` and
/// pushes typed events to `tx`. On EOF/spawn-error, respawns with exponential
/// backoff. Returns immediately.
pub fn spawn_event_stream(tx: Sender<Event>) {
    thread::spawn(move || {
        let mut delay = Duration::from_secs(1);
        loop {
            match run_one(&tx) {
                Ok(()) => {
                    log::warn!("niri event-stream ended cleanly, respawning");
                }
                Err(e) => {
                    log::warn!("niri event-stream error: {e}");
                }
            }
            let _ = tx.send(Event::StreamLost);
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    });
}

fn run_one(tx: &Sender<Event>) -> std::io::Result<()> {
    let mut child: Child = Command::new("niri")
        .args(["msg", "--json", "event-stream"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout: ChildStdout = child.stdout.take()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout"))?;

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Some(Event::StreamLost) | None => continue,
            Some(ev) => {
                if tx.send(ev).is_err() {
                    let _ = child.kill();
                    return Ok(());
                }
            }
        }
    }
    let _ = child.wait();
    Ok(())
}

fn parse_line(line: &str) -> Option<Event> {
    let raw: RawEvent = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            log::trace!("skip event (parse error: {e}): {line}");
            return None;
        }
    };
    match raw {
        RawEvent::Known(KnownEvent::WindowsChanged { windows }) => Some(Event::Snapshot(windows)),
        RawEvent::Known(KnownEvent::WindowOpenedOrChanged { window }) => Some(Event::Upsert(window)),
        RawEvent::Known(KnownEvent::WindowClosed { id }) => Some(Event::Closed(id)),
        RawEvent::Known(KnownEvent::WindowFocusChanged { id }) => Some(Event::Focus(id)),
        RawEvent::Unknown(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_changed() {
        let line = r#"{"WindowsChanged":{"windows":[{"id":1,"title":"t","app_id":"firefox","pid":42,"workspace_id":1,"is_focused":true,"is_floating":false,"is_urgent":false}]}}"#;
        let ev = parse_line(line).expect("event");
        match ev {
            Event::Snapshot(ws) => {
                assert_eq!(ws.len(), 1);
                assert_eq!(ws[0].app_id, "firefox");
                assert_eq!(ws[0].pid, Some(42));
                assert!(ws[0].is_focused);
            }
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn parses_focus_change() {
        let line = r#"{"WindowFocusChanged":{"id":42}}"#;
        match parse_line(line).expect("event") {
            Event::Focus(Some(42)) => {}
            _ => panic!("wrong event"),
        }
        let line = r#"{"WindowFocusChanged":{"id":null}}"#;
        match parse_line(line).expect("event") {
            Event::Focus(None) => {}
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn parses_window_closed() {
        let line = r#"{"WindowClosed":{"id":99}}"#;
        match parse_line(line).expect("event") {
            Event::Closed(99) => {}
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn ignores_unknown_event() {
        let line = r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["x"],"current_idx":0}}}"#;
        assert!(parse_line(line).is_none());
    }
}
