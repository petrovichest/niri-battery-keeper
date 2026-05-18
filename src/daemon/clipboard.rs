//! Wayland clipboard owner watcher.
//!
//! Listens to `zwlr_data_control_device_v1::selection` events and notifies
//! the daemon main loop every time the clipboard ownership changes. The
//! daemon uses this signal to remember which app currently holds the
//! clipboard and to keep its cgroup unthrottled — otherwise paste hangs
//! while the source app is on a 5%-CPU quota or frozen.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as zwlr_dev, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
};

/// Event emitted by the watcher. The daemon doesn't care about *what*
/// changed — only that ownership has just changed, so it can re-resolve
/// "currently focused window → clipboard owner".
#[derive(Debug, Clone, Copy)]
pub enum ClipboardEvent {
    /// A new selection was set (some client owns the clipboard).
    OwnerChanged,
    /// Selection was cleared (no clipboard).
    Cleared,
}

/// Spawn the watcher in a background thread. Auto-reconnects on errors
/// (e.g. compositor restart) with bounded backoff.
pub fn spawn_watcher(tx: Sender<ClipboardEvent>) {
    thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        loop {
            match run_one(&tx) {
                Ok(()) => log::warn!("clipboard watcher ended cleanly, respawning"),
                Err(e) => log::warn!("clipboard watcher error: {e}"),
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

struct WatcherState {
    tx: Sender<ClipboardEvent>,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwlrDataControlManagerV1>,
    /// Have we received the initial `selection` event after binding? The
    /// protocol guarantees the device fires `selection` once on bind to
    /// describe the current state — we want to ignore that "synthetic"
    /// notification because it doesn't represent a user action.
    got_initial: bool,
}

fn run_one(tx: &Sender<ClipboardEvent>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue::<WatcherState>();
    let qh: QueueHandle<WatcherState> = event_queue.handle();
    let _registry = display.get_registry(&qh, ());
    let mut state = WatcherState {
        tx: tx.clone(),
        seat: None,
        manager: None,
        got_initial: false,
    };
    event_queue.roundtrip(&mut state)?;
    let (seat, manager) = match (state.seat.clone(), state.manager.clone()) {
        (Some(s), Some(m)) => (s, m),
        _ => return Err("compositor missing wl_seat or zwlr_data_control_manager_v1".into()),
    };
    let _device = manager.get_data_device(&seat, &qh, ());
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WatcherState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    let v = version.min(1);
                    let seat: wl_seat::WlSeat = registry.bind(name, v, qh, ());
                    state.seat = Some(seat);
                }
                "zwlr_data_control_manager_v1" if state.manager.is_none() => {
                    let v = version.min(2);
                    let mgr: ZwlrDataControlManagerV1 = registry.bind(name, v, qh, ());
                    state.manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WatcherState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatcherState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatcherState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_dev::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_dev::Event::Selection { id } => {
                let cleared = id.is_none();
                if let Some(offer) = id {
                    offer.destroy();
                }
                if !state.got_initial {
                    state.got_initial = true;
                    return;
                }
                let ev = if cleared { ClipboardEvent::Cleared } else { ClipboardEvent::OwnerChanged };
                let _ = state.tx.send(ev);
            }
            zwlr_dev::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            zwlr_dev::Event::DataOffer { .. } => {
                // The companion offer for the upcoming Selection event. We
                // don't care about MIME types here.
            }
            zwlr_dev::Event::Finished => {
                log::warn!("clipboard device finished, watcher will respawn");
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(WatcherState, ZwlrDataControlDeviceV1, [
        zwlr_dev::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatcherState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: <ZwlrDataControlOfferV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {}
}
