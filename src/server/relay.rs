//! The single LAN connection everything else shares.
//!
//! `bambu serve --emulate` may have Bambu Studio, a browser dashboard and a
//! Home Assistant integration all watching at once, and the printer would rather
//! not know. [`LivePrinterLink`] holds one [`LanMqttClient::relay`] connection
//! and hands its reports to everyone who asks, reconnecting underneath without
//! any of them noticing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::emulate::Upstream;
use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;

/// Wait this long before reconnecting after the link drops. Matches
/// [`LiveSource`](super::live::LiveSource)'s pacing: long enough not to hammer a
/// rebooting printer, short enough that a Wi-Fi blip is invisible.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// How many reports a subscriber may fall behind before it starts missing them.
/// Deltas are small and frequent; a subscriber this far behind has a problem
/// that a deeper queue would only hide.
const REPORT_DEPTH: usize = 256;

/// How many requests may be in flight toward the printer. Requests are things a
/// person or a slicer did — rare, and never bursty — so reaching this depth
/// means the link is down, not that we are behind.
const REQUEST_DEPTH: usize = 256;

/// One long-lived connection to the real printer, shared by every consumer.
pub struct LivePrinterLink {
    reports: broadcast::Sender<Value>,
    requests: mpsc::Sender<Value>,
    /// The receiving end, parked here until the worker is started. Its presence
    /// is what makes "built" and "connected" separate states.
    worker: Mutex<Option<mpsc::Receiver<Value>>>,
}

impl LivePrinterLink {
    /// Build the link's channels **without connecting**.
    ///
    /// Connecting is a separate step ([`connect`](Self::connect)) so every
    /// consumer can [`subscribe`](Upstream::subscribe) first. The first thing a
    /// connection does is ask for a `pushall`, and that reply seeds the whole
    /// cache; a broadcast channel discards what arrives with no receivers, so
    /// connecting before the subscribers exist can throw the seed away. With no
    /// poll interval configured nothing would ask again, and the dashboard
    /// would spend the rest of the print merging deltas onto an empty state.
    pub fn new() -> Arc<Self> {
        let (reports, _) = broadcast::channel(REPORT_DEPTH);
        let (requests, request_rx) = mpsc::channel(REQUEST_DEPTH);
        Arc::new(Self {
            reports,
            requests,
            worker: Mutex::new(Some(request_rx)),
        })
    }

    /// Connect to `target` and keep the link up for as long as the process runs.
    /// `interval` is the `pushall` poll period; `None` relies on the printer's
    /// own ~2 s push. Call once, after the subscribers are in place.
    pub fn connect(self: Arc<Self>, target: ResolvedTarget, interval: Option<Duration>) {
        self.start(move |reports, mut requests| async move {
            loop {
                let client = LanMqttClient::new(target.clone());
                match client
                    .relay(interval, &mut requests, |report| {
                        // Err only means nobody is subscribed at this instant.
                        let _ = reports.send(report.clone());
                    })
                    .await
                {
                    // The request channel closed: the server is shutting down.
                    Ok(()) => return,
                    Err(e) => {
                        eprintln!("relay: printer link lost ({e}); reconnecting");
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        });
    }

    /// Start an arbitrary worker on this link's channels. The real one is
    /// [`connect`](Self::connect); this seam lets the plumbing be tested without
    /// a printer on the other end. Calling it twice does nothing the second
    /// time — there is only one request channel to hand over.
    fn start<F, Fut>(self: Arc<Self>, run: F)
    where
        F: FnOnce(broadcast::Sender<Value>, mpsc::Receiver<Value>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let Some(requests) = self.worker.lock().expect("worker lock poisoned").take() else {
            debug_assert!(false, "LivePrinterLink started twice");
            return;
        };
        tokio::spawn(run(self.reports.clone(), requests));
    }
}

impl Upstream for LivePrinterLink {
    fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.reports.subscribe()
    }

    fn send(&self, payload: Value) {
        // Sync on purpose: the callers are inside a client's read loop, and a
        // request that can't be queued is one the printer will never run.
        // Saying so is the whole handling — there is nothing to retry against a
        // link that is down, and the client's own ACK timeout is what tells it.
        if let Err(e) = self.requests.try_send(payload) {
            eprintln!("relay: dropping a request for the printer: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_request_reaches_the_worker_and_its_reply_reaches_every_subscriber() {
        // The link is a two-way pipe with fan-out on the way back; this is that
        // shape, with the printer replaced by an echo.
        let link = LivePrinterLink::new();
        Arc::clone(&link).start(|reports, mut requests| async move {
            while let Some(req) = requests.recv().await {
                let _ = reports.send(json!({"print": {
                    "command": "push_status",
                    "echoed": req["print"]["command"].clone(),
                }}));
            }
        });

        let mut a = link.subscribe();
        let mut b = link.subscribe();
        link.send(json!({"print": {"command": "pause"}}));

        for rx in [&mut a, &mut b] {
            let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the echo should arrive")
                .unwrap();
            assert_eq!(got["print"]["echoed"], "pause");
        }
    }

    #[tokio::test]
    async fn nothing_connects_until_the_subscribers_are_in_place() {
        // The very first thing the link does on connecting is ask for a
        // `pushall`, and that reply is the seed for the whole cache. A
        // broadcast channel discards what it receives with no receivers, so a
        // worker started before the emulator and the dashboard have subscribed
        // can throw the seed away — after which, with no poll interval, nothing
        // asks again and the dashboard merges deltas onto an empty state.
        let link = LivePrinterLink::new();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&started);
        let mut rx = link.subscribe();
        Arc::clone(&link).start(move |reports, _requests| async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = reports.send(json!({"print": {"command": "push_status", "msg": 0}}));
            std::future::pending::<()>().await;
        });

        // The subscription was made before the worker ran, so the seed lands.
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the connect-time snapshot must not be discarded")
            .unwrap();
        assert_eq!(got["print"]["msg"], 0);
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_subscriber_that_arrives_late_misses_what_it_missed_and_nothing_else() {
        // Reports are a live stream, not a log: a dashboard opened mid-print
        // gets what comes next, and asks for a snapshot if it wants the rest.
        let link = LivePrinterLink::new();
        Arc::clone(&link).start(|_reports, mut requests| async move {
            while requests.recv().await.is_some() {}
        });
        let early = link.subscribe();
        drop(early);
        let mut late = link.subscribe();
        let _ = link.reports.send(json!({"print": {"mc_percent": 1}}));
        let got = tokio::time::timeout(Duration::from_secs(5), late.recv())
            .await
            .expect("a report sent after subscribing arrives")
            .unwrap();
        assert_eq!(got["print"]["mc_percent"], 1);
    }

    #[tokio::test]
    async fn requests_are_dropped_loudly_rather_than_queued_forever_when_nothing_drains_them() {
        // A wedged link must not grow a queue without bound; the depth is the
        // backstop, and overflowing it is reported, not swallowed silently.
        let link = LivePrinterLink::new();
        Arc::clone(&link).start(|_reports, requests| async move {
            // Never drains: hold the receiver open and sleep.
            let _held = requests;
            std::future::pending::<()>().await;
        });
        for _ in 0..(REQUEST_DEPTH + 10) {
            link.send(json!({"print": {"command": "pause"}}));
        }
        // The point is that it returns at all — a blocking send here would have
        // deadlocked a client's read loop.
        assert!(link.requests.capacity() == 0 || link.requests.capacity() < REQUEST_DEPTH);
    }
}
