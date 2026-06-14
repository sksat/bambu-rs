//! The live printer source: bridges the blocking LAN MQTT [`monitor`] loop into
//! a [`watch`] channel so the dashboard can stream the *real* device status the
//! same way it streams the fake one.
//!
//! [`monitor`]: crate::client::LanMqttClient::monitor

use std::thread;
use std::time::Duration;

use tokio::sync::watch;

use super::api::PrinterSource;
use crate::client::{LanMqttClient, WatchStep};
use crate::config::ResolvedTarget;
use crate::core::status::PrinterStatus;

/// Wait before reconnecting after `monitor` returns (a stall or connect error).
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// Stall window handed to `monitor`: it returns if no report arrives within this,
/// then our outer loop reconnects. Generous so a momentarily-quiet printer isn't
/// dropped mid-print.
const STALL: Duration = Duration::from_secs(120);

/// A [`PrinterSource`] backed by a live LAN MQTT connection.
///
/// A dedicated OS thread runs the blocking [`LanMqttClient::monitor`] loop (which
/// owns its own current-thread runtime and auto-reconnects internally), turning
/// each merged report into a [`PrinterStatus`] pushed onto a [`watch`] channel.
/// The thread reconnects forever until the source and all its subscribers drop.
pub struct LiveSource {
    tx: watch::Sender<PrinterStatus>,
    // Held so the channel keeps a receiver while the source is alive; when the
    // source (and any WS subscribers) drop, the worker's reconnect loop stops.
    _keepalive: watch::Receiver<PrinterStatus>,
}

impl LiveSource {
    /// Connect to `target` and start streaming live status. `interval` is the
    /// `pushall` poll period; `None` relies on the printer's autonomous ~2 s push
    /// (the gentlest option, the default for an always-on dashboard).
    pub fn connect(target: ResolvedTarget, interval: Option<Duration>) -> Self {
        Self::spawn(move |tx| {
            loop {
                let client = LanMqttClient::new(target.clone()).with_timeout(STALL);
                let _ = client.monitor(interval, |rs| {
                    // Stop promptly when the source + all subscribers are gone,
                    // instead of running until the next stall/transport break.
                    if tx.send(PrinterStatus::from_state(rs.get())).is_err() {
                        WatchStep::Stop
                    } else {
                        WatchStep::Continue
                    }
                });
                if tx.receiver_count() == 0 {
                    break; // the source and all subscribers are gone
                }
                thread::sleep(RECONNECT_DELAY);
            }
        })
    }

    /// Spawn the bridge thread around `run`, handing it the channel sender. The
    /// real MQTT loop lives in [`LiveSource::connect`]; this seam lets tests drive
    /// the channel without a printer.
    fn spawn<R>(run: R) -> Self
    where
        R: FnOnce(watch::Sender<PrinterStatus>) + Send + 'static,
    {
        let (tx, rx) = watch::channel(PrinterStatus::default());
        let worker = tx.clone();
        thread::spawn(move || run(worker));
        Self { tx, _keepalive: rx }
    }
}

impl PrinterSource for LiveSource {
    fn current(&self) -> PrinterStatus {
        self.tx.borrow().clone()
    }
    fn subscribe(&self) -> watch::Receiver<PrinterStatus> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridges_published_status_to_current_and_subscribers() {
        let src = LiveSource::spawn(|tx| {
            let _ = tx.send(PrinterStatus {
                gcode_state: Some("RUNNING".to_string()),
                mc_percent: Some(42),
                ..Default::default()
            });
        });
        // The worker sends near-instantly; poll up to a generous deadline so the
        // test is reliable without timing assumptions.
        let mut got = None;
        for _ in 0..200 {
            let s = src.current();
            if s.gcode_state.is_some() {
                got = Some(s);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let s = got.expect("worker thread should publish a status");
        assert_eq!(s.gcode_state.as_deref(), Some("RUNNING"));
        assert_eq!(s.mc_percent, Some(42));
    }
}
