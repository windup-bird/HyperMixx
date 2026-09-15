//! FlowShift: offloads flow warm-up to a background thread so jumps never block the caller.
//!
//! Renamed from `TimeShift` — "time" collided with time-stretch; this shifts *flows*, not time.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::flow::Flow;

/// Owns a warm-up worker thread and the channels between it and the deck.
///
/// `submit_prepare` moves a `Flow` to the worker, which warms it up and parks it in `warm` before
/// announcing its id on the ready channel. Announcing after parking means `poll_ready() -> Some(id)`
/// always guarantees `take_ready_flow(id)` succeeds.
pub struct FlowShift {
    prepare_tx: Sender<Flow>,
    ready_rx: Receiver<u64>,
    ready_tx: Sender<u64>,
    warm: Arc<Mutex<HashMap<u64, Flow>>>,
    /// Only the newest submitted flow is announced; a newer jump supersedes an in-flight warm-up.
    latest: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl FlowShift {
    pub fn new() -> Self {
        let (prepare_tx, prepare_rx) = unbounded::<Flow>();
        let (ready_tx, ready_rx) = unbounded::<u64>();
        let warm: Arc<Mutex<HashMap<u64, Flow>>> = Arc::new(Mutex::new(HashMap::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_warm = Arc::clone(&warm);
        let thread_latest = Arc::clone(&latest);
        let thread_shutdown = Arc::clone(&shutdown);
        let worker = std::thread::Builder::new()
            .name("hypermixx-prepare".into())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match prepare_rx.recv_timeout(Duration::from_millis(2)) {
                        Ok(mut flow) => {
                            // A real time-stretch engine fills its overlap window here; that work
                            // must never happen on the audio thread.
                            flow.prepare();
                            if flow.id != thread_latest.load(Ordering::Relaxed) {
                                // A newer jump made this warm-up irrelevant; drop it silently.
                                continue;
                            }
                            // Move (not clone) the prepared flow into the warm map, then announce.
                            // The timestretch engine handles are not Clone, so the deck receives
                            // the exact engine instance that was warmed up.
                            if let Ok(mut guard) = thread_warm.lock() {
                                flow.mark_ready();
                                guard.insert(flow.id, flow);
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("failed to spawn warm-up thread");

        Self {
            prepare_tx,
            ready_rx,
            ready_tx,
            warm,
            latest,
            worker: Some(worker),
            shutdown,
        }
    }

    /// The channel a new `Flow` must announce its warm-up completion on.
    pub fn ready_sender(&self) -> Sender<u64> {
        self.ready_tx.clone()
    }

    /// Queues a flow for warm-up. Never blocks (unbounded channel).
    pub fn submit_prepare(&self, flow: Flow) {
        self.latest.store(flow.id, Ordering::Relaxed);
        let _ = self.prepare_tx.send(flow);
    }

    /// Non-blocking peek at the id of a flow that finished warming up.
    pub fn poll_ready(&self) -> Option<u64> {
        self.ready_rx.try_recv().ok()
    }

    /// Takes the warmed-up flow previously announced for `id`.
    pub fn take_ready_flow(&self, id: u64) -> Option<Flow> {
        self.warm.lock().ok()?.remove(&id)
    }

    /// Number of flows still waiting to be warmed up.
    pub fn pending(&self) -> usize {
        self.prepare_tx.len()
    }
}

impl Default for FlowShift {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FlowShift {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::FlowState;
    use hypermixx_core::CHANNELS;
    use hypermixx_media::{DecodedAudio, PcmPool};
    use std::sync::Arc;

    fn source(n_frames: u64) -> Arc<dyn hypermixx_core::Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: 48_000,
            channels: CHANNELS,
        }))
    }

    /// A flow wired to its own ready channel, so tests can watch the announcement directly.
    fn dummy_flow(id: u64, start: u64) -> (Flow, Receiver<u64>) {
        let (tx, rx) = unbounded();
        (Flow::new(id, source(10_000), start, None, tx), rx)
    }

    /// Polls `probe` until it yields a value; the warm-up thread ticks every 2ms.
    fn wait_for<T>(label: &str, probe: impl Fn() -> Option<T>) -> T {
        for _ in 0..200 {
            if let Some(value) = probe() {
                return value;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {label}");
    }

    #[test]
    fn warms_flow_up_and_parks_it() {
        let ts = FlowShift::new();
        let (flow, ready_rx) = dummy_flow(1, 123);
        ts.submit_prepare(flow);

        assert_eq!(wait_for("ready id", || ready_rx.try_recv().ok()), 1);
        let warmed = ts.take_ready_flow(1).expect("warmed flow missing");
        assert_eq!(warmed.state, FlowState::Ready);
        assert_eq!(warmed.current_frame(), 123);
        assert!(
            ts.take_ready_flow(1).is_none(),
            "a parked flow may be taken only once"
        );
    }

    #[test]
    fn announces_on_the_channel_it_was_given() {
        let ts = FlowShift::new();
        let flow = Flow::new(3, source(1000), 200, None, ts.ready_sender());
        ts.submit_prepare(flow);
        assert_eq!(wait_for("deck announcement", || ts.poll_ready()), 3);
        assert_eq!(
            ts.take_ready_flow(3).map(|flow| flow.current_frame()),
            Some(200)
        );
    }

    #[test]
    fn the_newest_jump_always_ends_up_warm() {
        let ts = FlowShift::new();
        for id in 10..14 {
            let (flow, _) = dummy_flow(id, id * 100);
            ts.submit_prepare(flow);
        }
        let warmed = wait_for("newest flow", || ts.take_ready_flow(13));
        assert_eq!(warmed.current_frame(), 1300);
    }

    #[test]
    fn poll_is_non_blocking_when_idle() {
        let ts = FlowShift::new();
        assert_eq!(ts.poll_ready(), None);
        assert!(ts.take_ready_flow(1).is_none());
        assert_eq!(ts.pending(), 0);
    }
}
