//! Background library scanning, owned by the library crate.
//!
//! One worker owns the scan connection and one watcher owns removable-media discovery, so every
//! front-end (TUI, line REPL, a future FFI shell) shares the same behaviour. The front-end never
//! reads an audio byte, opens a scan transaction, or polls `/sys/block` itself: it enqueues sources
//! and polls [`ScanService::poll_next`] for plain-data progress events.
//!
//! [`ScanProgress`] is deliberately serde-ready and the API exposes no channel type, so it can be
//! bridged over FFI unchanged (poll it into a stream on the other side).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};

use super::db::{detect_source_kinds, ImportCalibration, LibraryDb};
use super::model::{ScanStats, Source, SourceKind, SourceOrigin};
use crate::usb::{self, UsbEvent};

/// Progress from the scan worker. Plain data, no handles: safe to hand across an FFI boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ScanProgress {
    /// A removable device was registered. The front-end should allow this id in the browser.
    SourceRegistered {
        source_id: i64,
        name: String,
    },
    /// A device went away; these ids are no longer visible (the rows stay in the cache).
    SourcesRemoved {
        source_ids: Vec<i64>,
    },
    /// The source's list tree is committed. Playlists are visible now, with their cached tracks,
    /// before the track pass has run. Emitted by the lists phase of a layered scan.
    ListsReady {
        source_id: i64,
        name: String,
    },
    /// A source finished its full scan.
    SourceDone {
        source_id: i64,
        name: String,
        kind: String,
        stats: ScanStats,
    },
    /// A source could not be scanned (missing device, malformed export, poisoned lock, ...).
    Failed {
        source_id: i64,
        name: String,
        error: String,
    },
}

/// Work handed to the single worker. USB events travel on the same request channel as explicit
/// enqueues, so a mount and a `browser refresh` can never race on the db write transaction.
enum ScanRequest {
    Scan(Vec<Source>),
    Configure {
        calibration: ImportCalibration,
        usb_formats: Vec<SourceKind>,
    },
}

/// The front-end's handle on the scan subsystem.
pub struct ScanService {
    request_tx: Option<Sender<ScanRequest>>,
    progress_tx: Sender<ScanProgress>,
    progress_rx: Receiver<ScanProgress>,
    /// Terminal events (`SourceDone`/`Failed`) emitted but not yet polled. Every emission is
    /// counted once — including worker-level failures that have no source of their own — and
    /// [`ScanService::poll_next`] decrements once per event, so a front-end can tell "no result
    /// outstanding / no scan in flight" without tracking sources itself.
    pending: Arc<AtomicUsize>,
    db_path: Option<PathBuf>,
    calibration: ImportCalibration,
    usb_formats: Vec<SourceKind>,
}

impl Default for ScanService {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanService {
    pub fn new() -> Self {
        let (progress_tx, progress_rx) = unbounded();
        Self {
            request_tx: None,
            progress_tx,
            progress_rx,
            pending: Arc::new(AtomicUsize::new(0)),
            db_path: None,
            calibration: ImportCalibration::default(),
            usb_formats: Vec::new(),
        }
    }

    /// Supplies the db connection details and which export kinds to auto-discover on removable
    /// media. Safe to call more than once (e.g. after F9 changes the preference): the worker picks
    /// the new settings up on its next request.
    pub fn configure(&mut self, db: &LibraryDb, usb_formats: Vec<SourceKind>) {
        let calibration = db.import_calibration();
        if self.db_path.is_none() {
            self.db_path = Some(db.path().to_path_buf());
        }
        self.calibration = calibration;
        self.usb_formats = usb_formats.clone();
        if self.request_tx.is_none() {
            self.start_worker();
        }
        if let Some(tx) = &self.request_tx {
            let _ = tx.send(ScanRequest::Configure {
                calibration,
                usb_formats,
            });
        }
    }

    /// Queues sources (configured folders/libraries, or an explicit `browser refresh`). Returns
    /// whether they were accepted. Nothing is scanned on the calling thread.
    pub fn enqueue(&mut self, sources: Vec<Source>) -> bool {
        if sources.is_empty() {
            return false;
        }
        if self.db_path.is_none() {
            return false;
        }
        if self.request_tx.is_none() {
            self.start_worker();
        }
        let Some(tx) = &self.request_tx else {
            return false;
        };
        tx.send(ScanRequest::Scan(sources)).is_ok()
    }

    /// Pulls one progress event, if any. Pull-based and channel-free on the surface: an FFI shell
    /// can wrap this in a stream without exposing crossbeam types.
    pub fn poll_next(&self) -> Option<ScanProgress> {
        let event = self.progress_rx.try_recv().ok()?;
        if matches!(
            event,
            ScanProgress::SourceDone { .. } | ScanProgress::Failed { .. }
        ) {
            // Every terminal event is counted when it is emitted, so this always has a matching
            // increment. Saturating anyway: a defensive guard against a double decrement.
            let _ = self
                .pending
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
        Some(event)
    }

    /// Sources whose result has not been polled yet. Zero means "no scan in flight".
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    fn start_worker(&mut self) {
        let Some(path) = self.db_path.clone() else {
            return;
        };
        let (request_tx, request_rx) = unbounded::<ScanRequest>();
        let (usb_tx, usb_rx) = unbounded::<UsbEvent>();
        let progress_tx = self.progress_tx.clone();
        let pending = Arc::clone(&self.pending);
        let calibration = self.calibration;
        let usb_formats = self.usb_formats.clone();

        // The worker keeps a sender alive so `usb_rx` can never disconnect: a disconnected
        // receiver returns immediately from `select!` and would spin the worker at 100% CPU.
        let usb_keepalive = usb_tx.clone();
        let mut watcher_started = !usb_formats.is_empty();
        if watcher_started {
            let tx = usb_tx.clone();
            let _ = thread::Builder::new()
                .name("hypermixx-usb".to_owned())
                .spawn(move || usb::watch(tx));
        }

        let spawned = thread::Builder::new()
            .name("hypermixx-scan".to_owned())
            .spawn(move || {
                let _usb_keepalive = usb_keepalive;
                let scan_db = match LibraryDb::open(&path) {
                    Ok(db) => db,
                    Err(err) => {
                        // No source id to attach, but this is still a terminal event the front-end
                        // will poll: count it so `pending` stays balanced.
                        pending.fetch_add(1, Ordering::Relaxed);
                        let _ = progress_tx.send(ScanProgress::Failed {
                            source_id: 0,
                            name: "library".to_owned(),
                            error: format!("cannot open scan connection: {err}"),
                        });
                        return;
                    }
                };
                scan_db.set_import_calibration(calibration);
                let mut formats = usb_formats;
                loop {
                    select! {
                        recv(request_rx) -> message => match message {
                            Ok(ScanRequest::Scan(sources)) => {
                                for source in sources {
                                    scan_passes(&scan_db, source, &progress_tx, &pending);
                                }
                            }
                            Ok(ScanRequest::Configure { calibration, usb_formats }) => {
                                scan_db.set_import_calibration(calibration);
                                // Enabling removable media later (e.g. after a settings change)
                                // starts the watcher once, on the worker's own account.
                                if !usb_formats.is_empty() && !watcher_started {
                                    let tx = usb_tx.clone();
                                    let _ = thread::Builder::new()
                                        .name("hypermixx-usb".to_owned())
                                        .spawn(move || usb::watch(tx));
                                    watcher_started = true;
                                }
                                formats = usb_formats;
                            }
                            Err(_) => break,
                        },
                        recv(usb_rx) -> message => match message {
                            Ok(event) => {
                                handle_usb(&scan_db, event, &formats, &progress_tx, &pending)
                            }
                            // A watcher that ended must not spin the loop; the keepalive sender
                            // above means this branch can only fire if every sender is gone.
                            Err(_) => break,
                        },
                    }
                }
            });

        match spawned {
            Ok(_) => self.request_tx = Some(request_tx),
            Err(err) => {
                // Counted like any other terminal event: the front-end polls this failure too.
                self.pending.fetch_add(1, Ordering::Relaxed);
                let _ = self.progress_tx.send(ScanProgress::Failed {
                    source_id: 0,
                    name: "library".to_owned(),
                    error: format!("could not start scan worker: {err}"),
                });
            }
        }
    }
}

/// Registers a freshly mounted device and scans it, or reports an unmount. All of this runs on the
/// worker, on the worker's own connection.
fn handle_usb(
    scan_db: &LibraryDb,
    event: UsbEvent,
    formats: &[SourceKind],
    progress_tx: &Sender<ScanProgress>,
    pending: &Arc<AtomicUsize>,
) {
    match event {
        UsbEvent::Mounted(root) => {
            let present = detect_source_kinds(&root);
            // Register every matching kind before scanning anything: the sources become visible to
            // the front-end together, instead of each one waiting for the previous kind's full
            // pass (a mount with rekordbox + Serato + VirtualDJ would otherwise reveal them one
            // scan apart).
            let mut registered: Vec<Source> = Vec::new();
            for kind in formats.iter().copied().filter(|kind| present.contains(kind)) {
                match scan_db.add_source_with_origin(kind, SourceOrigin::Usb, &root, None) {
                    Ok(source) => {
                        let _ = progress_tx.send(ScanProgress::SourceRegistered {
                            source_id: source.id,
                            name: source.name.clone(),
                        });
                        registered.push(source);
                    }
                    Err(err) => {
                        // Registration never started a scan, but the failure is a terminal event
                        // the front-end will poll: count it so it cannot consume another scan's
                        // slot in `pending`.
                        pending.fetch_add(1, Ordering::Relaxed);
                        let _ = progress_tx.send(ScanProgress::Failed {
                            source_id: 0,
                            name: root.display().to_string(),
                            error: err.to_string(),
                        });
                    }
                }
            }
            for source in registered {
                scan_passes(scan_db, source, progress_tx, pending);
            }
        }
        UsbEvent::Unmounted(root) => {
            let ids: Vec<i64> = scan_db
                .sources()
                .unwrap_or_default()
                .into_iter()
                .filter(|source| source.root == root)
                .map(|source| source.id)
                .collect();
            let _ = progress_tx.send(ScanProgress::SourcesRemoved { source_ids: ids });
        }
    }
}

/// Scans one source in its phases: the list tree first (committed on its own so the front-end can
/// show it, with its cached tracks, before any track metadata is read), then the full track pass.
/// A panic inside a decoder (a truncated export file) must not kill the shared worker, so it is
/// contained and turned into a `Failed` event.
fn scan_passes(
    scan_db: &LibraryDb,
    source: Source,
    progress_tx: &Sender<ScanProgress>,
    pending: &Arc<AtomicUsize>,
) {
    let lists = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !source.root.is_dir() {
            return Err(format!("{} is gone", source.root.display()));
        }
        scan_db.scan_lists(source.id).map_err(|err| err.to_string())
    }))
    .unwrap_or_else(|_| Err("scan panicked on a malformed library file".to_owned()));
    match lists {
        Err(error) => {
            // One terminal event per source keeps `pending` balanced.
            pending.fetch_add(1, Ordering::Relaxed);
            let _ = progress_tx.send(ScanProgress::Failed {
                source_id: source.id,
                name: source.name.clone(),
                error,
            });
            return;
        }
        Ok(()) => {
            let _ = progress_tx.send(ScanProgress::ListsReady {
                source_id: source.id,
                name: source.name.clone(),
            });
        }
    }
    run_full_pass(scan_db, source, progress_tx, pending);
}

/// The full track pass: every file stat'ed, metadata read for new or changed files, locations and
/// items written, stale rows pruned.
fn run_full_pass(
    scan_db: &LibraryDb,
    source: Source,
    progress_tx: &Sender<ScanProgress>,
    pending: &Arc<AtomicUsize>,
) {
    pending.fetch_add(1, Ordering::Relaxed);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scan_db
            .scan_source(source.id, &mut |_| {})
            .map_err(|err| err.to_string())
    }))
    .unwrap_or_else(|_| Err("scan panicked on a malformed library file".to_owned()));

    let event = match outcome {
        Ok(stats) => ScanProgress::SourceDone {
            source_id: source.id,
            name: source.name,
            kind: source.kind.label().to_owned(),
            stats,
        },
        Err(error) => ScanProgress::Failed {
            source_id: source.id,
            name: source.name,
            error,
        },
    };
    let _ = progress_tx.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hypermixx-scan-{}-{}-{}",
            label,
            std::process::id(),
            super::super::db::unix_now()
        ))
    }

    fn wait_for<F: Fn(&ScanProgress) -> bool>(service: &ScanService, wanted: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = service.poll_next() {
                if wanted(&event) {
                    return true;
                }
                continue;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The whole point of the service: enqueue returns immediately and the metadata reads happen
    /// on the worker, never on the caller (which would freeze a UI thread).
    #[test]
    fn an_enqueued_source_reports_back_without_blocking_the_caller() {
        let dir = temp_dir("enqueue");
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("music");
        std::fs::create_dir_all(&root).unwrap();
        let db = LibraryDb::open(dir.join("library.sqlite3")).unwrap();
        let source = db.add_folder_source(&root, None).unwrap();

        let mut service = ScanService::new();
        service.configure(&db, Vec::new());
        let started = Instant::now();
        assert!(service.enqueue(vec![source]));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "enqueue must not scan inline"
        );
        assert!(
            wait_for(&service, |event| matches!(
                event,
                ScanProgress::SourceDone { .. }
            )),
            "the worker reports the finished source"
        );
        assert_eq!(service.pending(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One worker serves every source, so a panic inside a decoder must be contained. A truncated
    /// rekordbox PDB panics in its parser; the next source still has to be scanned.
    #[test]
    fn a_malformed_export_does_not_kill_the_worker() {
        let dir = temp_dir("malformed");
        let _ = std::fs::remove_dir_all(&dir);
        let bad_root = dir.join("stick");
        std::fs::create_dir_all(bad_root.join("PIONEER/rekordbox")).unwrap();
        std::fs::write(bad_root.join("PIONEER/rekordbox/export.pdb"), b"").unwrap();
        let good_root = dir.join("music");
        std::fs::create_dir_all(&good_root).unwrap();

        let db = LibraryDb::open(dir.join("library.sqlite3")).unwrap();
        let bad = db
            .add_source(SourceKind::Rekordbox, &bad_root, None)
            .unwrap();
        let good = db.add_folder_source(&good_root, None).unwrap();

        let mut service = ScanService::new();
        service.configure(&db, Vec::new());
        assert!(service.enqueue(vec![bad, good]));

        let mut failed = false;
        let mut done = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(failed && done) {
            if let Some(event) = service.poll_next() {
                match event {
                    ScanProgress::Failed { .. } => failed = true,
                    ScanProgress::SourceDone { .. } => done = true,
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(failed, "the malformed export is reported as a failure");
        assert!(done, "the worker still scans the next source after a panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worker-level failure has no source of its own, but the front-end still polls it. It must
    /// be counted when emitted, or polling it decrements `pending` and falsely marks an unrelated
    /// in-flight source as finished.
    #[test]
    fn a_startup_failure_does_not_consume_an_in_flight_scan() {
        let dir = temp_dir("startup-failure");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut service = ScanService::new();
        // A database path that is itself a directory cannot be opened by sqlite, so the worker
        // fails at startup and reports through the generic failure slot.
        service.db_path = Some(dir.clone());
        // One source's result is still outstanding when that failure arrives.
        service.pending.fetch_add(1, Ordering::Relaxed);

        service.start_worker();
        assert!(
            wait_for(&service, |event| matches!(
                event,
                ScanProgress::Failed { source_id: 0, .. }
            )),
            "the worker reports that it could not open its connection"
        );
        assert_eq!(
            service.pending(),
            1,
            "polling the startup failure must not clear the in-flight source"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removable-media handling (register + scan) is library-owned, not a front-end concern.
    #[test]
    fn a_mount_registers_and_scans_without_a_front_end() {
        let dir = temp_dir("mount");
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("stick");
        std::fs::create_dir_all(root.join("PIONEER/rekordbox")).unwrap();
        std::fs::write(root.join("PIONEER/rekordbox/export.pdb"), b"").unwrap();
        let db = LibraryDb::open(dir.join("library.sqlite3")).unwrap();
        let (tx, rx) = unbounded();
        let pending = Arc::new(AtomicUsize::new(0));

        handle_usb(
            &db,
            UsbEvent::Mounted(root),
            &[SourceKind::Rekordbox],
            &tx,
            &pending,
        );
        let events: Vec<ScanProgress> = rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ScanProgress::SourceRegistered { .. })),
            "the mount registers a source and the front-end is told its id"
        );
        assert_eq!(pending.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
