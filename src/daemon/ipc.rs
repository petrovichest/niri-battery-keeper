use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use crate::proto::{socket_path, Request, Response};

/// Bidirectional message from IPC server to the daemon main loop.
pub struct IpcMessage {
    pub req: Request,
    pub reply: Sender<Response>,
}

/// Bind the Unix socket and spawn the accept loop. Returns a receiver the
/// main loop polls for incoming requests.
pub fn start() -> std::io::Result<Receiver<IpcMessage>> {
    let path = socket_path();
    // Remove stale socket if present
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;
    log::info!("ipc listening on {}", path.display());

    let (tx, rx) = channel::<IpcMessage>();
    thread::spawn(move || accept_loop(listener, tx));
    Ok(rx)
}

fn accept_loop(listener: UnixListener, tx: Sender<IpcMessage>) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let tx = tx.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &tx) {
                        log::debug!("ipc conn error: {e}");
                    }
                });
            }
            Err(e) => {
                log::warn!("ipc accept failed: {e}");
                break;
            }
        }
    }
}

fn handle_conn(stream: UnixStream, tx: &Sender<IpcMessage>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response::Error { message: format!("bad request: {e}") };
            write_resp(&mut writer, &resp)?;
            return Ok(());
        }
    };
    let (reply_tx, reply_rx) = channel::<Response>();
    if tx.send(IpcMessage { req, reply: reply_tx }).is_err() {
        write_resp(&mut writer, &Response::Error { message: "daemon shut down".into() })?;
        return Ok(());
    }
    let resp = reply_rx
        .recv()
        .unwrap_or(Response::Error { message: "no reply".into() });
    write_resp(&mut writer, &resp)?;
    Ok(())
}

fn write_resp(w: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let s = serde_json::to_string(resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    w.write_all(s.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}
