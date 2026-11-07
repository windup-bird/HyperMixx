//! `gridcmp` — beat-grid comparison harness: internal analysis vs the rekordbox USB reference.
//!
//! The library already imports rekordbox's `PQTZ` beat lists (origin `rekordbox_anlz`, clock
//! calibration applied in `beat_anchor.time_ms`). This tool turns them into an oracle for the
//! analysis pipeline, in two layers:
//!
//! **Layer 1 — fitter oracle (no audio).** Rekordbox's own beat list goes through
//! [`fit_rigid`]; the fit must reproduce rekordbox's declared BPM and anchor. Any failure here is
//! a bug in the fitter itself, independent of the stratum backend.
//!
//! **Layer 2 — end to end.** The cached (or freshly run, `--fresh`) internal grid is compared
//! against the reference: octave ratio (must be 2^0), BPM error, grid-line phase offset (reported,
//! not failed — a constant clock offset shifts phase but the calibration table owns that), and
//! per-beat error (median / p95).
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p hypermixx-library --example gridcmp -- [--db <path>] [--track <id>] [--fresh]
//! ```
//!
//! `--fresh` re-runs the stratum backend on the local file instead of reading the `analysis`
//! cache (slow: one full decode + analysis per track). Exit code 1 when any track fails.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};

use hypermixx_library::analyser::{analyze, fit_rigid, Backends, RawAnalysis, RefineConfig};
use hypermixx_library::{default_database_path, LibraryDb};

/// Decode/analysis sample rate for `--fresh` (grids convert to ms through their own rate).
const ANALYSIS_RATE: u32 = 44_100;
/// Layer 1: fitted-vs-declared BPM slack (rekordbox stores BPM as hundredths).
const ORACLE_BPM_SLACK: f64 = 0.005;
/// Layer 2 pass threshold: the DJ-practical BPM bar (sync stays inside what pitch faders
/// correct anyway). Beat-error and phase are *reported*, not failed — a rigid fit of a track
/// with real micro-tempo legitimately disagrees with the reference by the difference of the two
/// compromises, and the rekordbox MP3 clock shift is a calibration matter.
const BPM_SLACK: f64 = 0.005;

struct Args {
    db: Option<PathBuf>,
    track: Option<i64>,
    fresh: bool,
    /// Only analyse the first N tracks (mainly for `--fresh` sampling).
    limit: Option<usize>,
}

struct RefGrid {
    track_id: i64,
    title: String,
    path: String,
    declared_bpm: f64,
    beats_ms: Vec<f64>,
}

/// Layer 1: the fit's BPM and its distance to the declared anchor, or the fit's error string.
enum Oracle {
    Fit { bpm: f64, anchor_err_ms: f64 },
    Failed(String),
}

/// Everything one row of the report needs.
struct Report {
    grid: RefGrid,
    oracle_bpm: f64,
    oracle_anchor_ms: f64,
    internal: Option<Internal>,
}

struct Internal {
    bpm: f64,
    beats_ms: Vec<f64>,
    origin: &'static str,
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}\nusage: gridcmp [--db <path>] [--track <id>] [--fresh]");
            return std::process::ExitCode::FAILURE;
        }
    };
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(default_database_path);
    if !db_path.exists() {
        eprintln!("library database {} does not exist", db_path.display());
        return std::process::ExitCode::FAILURE;
    }
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("open {}: {err}", db_path.display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let refs = match reference_grids(&conn, args.track) {
        Ok(mut refs) if !refs.is_empty() => {
            if let Some(limit) = args.limit {
                refs.truncate(limit);
            }
            refs
        }
        Ok(_) => {
            eprintln!(
                "no rekordbox_anlz beat grids in {} (scan a rekordbox USB first)",
                db_path.display()
            );
            return std::process::ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("query: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let db = match LibraryDb::open(&db_path) {
        Ok(db) => Arc::new(db),
        Err(err) => {
            eprintln!("open library handle: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    println!(
        "\n{:>5} {:24} {:>8} {:>8} {:>9} {:>8} {:>3} {:>8} {:>8} {:>9}  {}",
        "track", "title", "ref", "oracle", "oΔms", "int", "oct", "ratio", "phase", "med/p95", "verdict"
    );
    let (mut pass, mut fail, mut skip) = (0usize, 0usize, 0usize);
    for grid in &refs {
        let (verdict, line) = compare_track(&conn, &db, grid, args.fresh);
        match verdict {
            "ok" => pass += 1,
            "FAIL" => fail += 1,
            _ => skip += 1,
        }
        println!("{line}");
    }
    println!("\n{pass} ok, {fail} FAIL, {skip} skip (skips passed the oracle but have no cached analysis; oracle failures count as FAIL)");
    println!("oracle = fit_rigid on rekordbox's own beat list vs its declared BPM/anchor; phase is reported, not failed (constant clock offsets are a calibration matter).");
    if fail > 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        db: None,
        track: None,
        fresh: false,
        limit: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--db" => args.db = Some(PathBuf::from(it.next().ok_or("--db needs a path")?)),
            "--track" => {
                args.track = Some(
                    it.next()
                        .ok_or("--track needs an id")?
                        .parse()
                        .map_err(|_| "--track needs a numeric track id")?,
                )
            }
            "--fresh" => args.fresh = true,
            "--limit" => {
                args.limit = Some(
                    it.next()
                        .ok_or("--limit needs a number")?
                        .parse()
                        .map_err(|_| "--limit needs a number")?,
                )
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(args)
}

/// Every track with an imported rekordbox grid, plus its calibrated beat anchors.
fn reference_grids(conn: &Connection, only: Option<i64>) -> rusqlite::Result<Vec<RefGrid>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, t.id, COALESCE(t.title, ''), tl.path, COALESCE(g.bpm, 0.0)
         FROM beatgrid g
         JOIN track_location tl ON tl.id = g.location_id
         JOIN track t ON t.id = tl.track_id
         WHERE g.origin = 'rekordbox_anlz' AND (?1 IS NULL OR t.id = ?1)
         ORDER BY t.id, g.id",
    )?;
    let rows: Vec<(i64, i64, String, String, f64)> = stmt
        .query_map(rusqlite::params![only], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut anchors = conn.prepare(
        "SELECT time_ms FROM beat_anchor WHERE grid_id = ?1 ORDER BY seq",
    )?;
    let mut out = Vec::new();
    for (grid_id, track_id, title, path, declared) in rows {
        let beats_ms: Vec<f64> = anchors
            .query_map(rusqlite::params![grid_id], |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();
        // Below the fitter's minimum the oracle cannot run; skip rather than misreport.
        if beats_ms.len() >= 16 {
            out.push(RefGrid {
                track_id,
                title,
                path,
                declared_bpm: declared,
                beats_ms,
            });
        }
    }
    Ok(out)
}

/// The track's local file: the grid's own path, or any other location that exists on disk.
fn best_local_path(conn: &Connection, track_id: i64, primary: &str) -> Option<String> {
    if Path::new(primary).is_file() {
        return Some(primary.to_owned());
    }
    let mut stmt = conn
        .prepare("SELECT path FROM track_location WHERE track_id = ?1")
        .ok()?;
    let paths: Vec<String> = stmt
        .query_map(rusqlite::params![track_id], |row| row.get(0))
        .ok()?
        .filter_map(Result::ok)
        .collect();
    paths.into_iter().find(|path| Path::new(path).is_file())
}

fn layer1(grid: &RefGrid) -> Oracle {
    let raw = RawAnalysis {
        beats_sec: grid.beats_ms.iter().map(|ms| ms / 1000.0).collect(),
        key: None,
        bpm_hint: None,
        duration_sec: 0.0,
        phase_untrusted: false,
        grid_confidence: None,
    };
    match fit_rigid(&raw, ANALYSIS_RATE, 0, &RefineConfig::default()) {
        Ok((spec, _quality)) => {
            let seg = &spec.segments[0];
            let period = 60.0 / seg.bpm;
            let phase = seg.start_frame as f64 / f64::from(ANALYSIS_RATE);
            let anchor = grid.beats_ms[0] / 1000.0;
            Oracle::Fit {
                bpm: seg.bpm,
                anchor_err_ms: residual_ms(anchor, phase, period),
            }
        }
        Err(msg) => Oracle::Failed(msg),
    }
}

fn internal_grid(
    conn: &Connection,
    db: &LibraryDb,
    grid: &RefGrid,
    fresh: bool,
) -> Result<Internal, String> {
    if fresh {
        let path = best_local_path(conn, grid.track_id, &grid.path)
            .ok_or_else(|| "no readable local file for --fresh".to_owned())?;
        eprintln!("  analysing {path} ...");
        let decoded = hypermixx_media::decode_file_at(&path, ANALYSIS_RATE)
            .map_err(|err| format!("decode: {err}"))?;
        let source = Arc::new(hypermixx_media::PcmPool::from_decoded(decoded));
        let analysis = analyze(source, ANALYSIS_RATE, Backends::default())
            .map_err(|err| err.to_string())?;
        return Ok(Internal {
            bpm: f64::from(analysis.bpm()),
            beats_ms: analysis
                .beatgrid
                .beat_frames
                .iter()
                .map(|&f| f as f64 / f64::from(analysis.beatgrid.sample_rate) * 1000.0)
                .collect(),
            origin: "fresh",
        });
    }
    match db.load_analysis(grid.track_id) {
        Ok(Some(analysis)) => Ok(Internal {
            bpm: f64::from(analysis.bpm()),
            beats_ms: analysis
                .beatgrid
                .beat_frames
                .iter()
                .map(|&f| f as f64 / f64::from(analysis.beatgrid.sample_rate) * 1000.0)
                .collect(),
            origin: "cache",
        }),
        Ok(None) => Err("no cached analysis (run `analyse` in the CLI, or use --fresh)".into()),
        Err(err) => Err(format!("cache read: {err}")),
    }
}

/// Distance from `t_sec` to the nearest grid line of `phase + k·period`, in ms.
fn residual_ms(t_sec: f64, phase_sec: f64, period_sec: f64) -> f64 {
    let k = ((t_sec - phase_sec) / period_sec).round();
    (t_sec - phase_sec - k * period_sec).abs() * 1000.0
}

/// Distance from each reference beat to the nearest internal grid beat: median and p95, ms.
fn beat_error_ms(refs: &[f64], internal: &[f64]) -> (f64, f64) {
    let mut errs: Vec<f64> = refs
        .iter()
        .map(|&t| {
            let idx = internal.partition_point(|&b| b < t);
            let before = idx.checked_sub(1).map(|i| (t - internal[i]).abs());
            let after = internal.get(idx).map(|&b| (b - t).abs());
            match (before, after) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => f64::INFINITY,
            }
        })
        .collect();
    errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f64| -> f64 {
        errs.get(((errs.len() as f64 * q) as usize).min(errs.len() - 1))
            .copied()
            .unwrap_or(f64::INFINITY)
    };
    (pick(0.5), pick(0.95))
}

fn trunc(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(width - 1).collect();
        format!("{cut}…")
    }
}

fn compare_track(
    conn: &Connection,
    db: &LibraryDb,
    grid: &RefGrid,
    fresh: bool,
) -> (&'static str, String) {
    let mut report = Report {
        grid: RefGrid {
            track_id: grid.track_id,
            title: grid.title.clone(),
            path: grid.path.clone(),
            declared_bpm: grid.declared_bpm,
            beats_ms: grid.beats_ms.clone(),
        },
        oracle_bpm: 0.0,
        oracle_anchor_ms: f64::INFINITY,
        internal: None,
    };

    // Layer 1 first: a broken fitter invalidates everything downstream. A fitted BPM exactly a
    // power of two away from the declared one is not a fitter bug — it is the DJ band fold
    // (rekordbox's tempogram may prefer the half-time display, e.g. 70 for a 174 DnB track) —
    // so the oracle judges on the octave-folded ratio.
    match layer1(grid) {
        Oracle::Fit { bpm, anchor_err_ms } => {
            let fold = 2f64.powf((bpm / grid.declared_bpm.max(1e-9)).log2().round());
            let rel = (bpm / fold - grid.declared_bpm).abs() / grid.declared_bpm.max(1e-9);
            if rel > ORACLE_BPM_SLACK {
                report.oracle_bpm = bpm;
                report.oracle_anchor_ms = anchor_err_ms;
                return (
                    "FAIL",
                    render(&report, "FAIL", &format!("oracle bpm {bpm:.2} vs declared {:.2}", grid.declared_bpm)),
                );
            }
            report.oracle_bpm = bpm;
            report.oracle_anchor_ms = anchor_err_ms;
        }
        Oracle::Failed(msg) => {
            return ("FAIL", render(&report, "FAIL", &format!("oracle fit: {msg}")));
        }
    }

    let internal = match internal_grid(conn, db, grid, fresh) {
        Ok(internal) => internal,
        Err(msg) => return ("skip", render(&report, "skip", &msg)),
    };
    report.internal = Some(internal);
    let internal = report.internal.as_ref().unwrap();

    // Octave: the internal BPM must sit on the reference's own octave (k = 0).
    let ratio = internal.bpm / report.oracle_bpm.max(1e-9);
    let octave = ratio.log2().round();
    let octave_aligned = internal.bpm / 2f64.powf(octave);
    let bpm_rel = (octave_aligned - report.oracle_bpm).abs() / report.oracle_bpm;

    // Phase of the internal grid against the reference grid (report only).
    let period_ref = 60.0 / report.oracle_bpm;
    let phase_ms = internal
        .beats_ms
        .first()
        .map(|&b0| residual_ms(b0 / 1000.0, grid.beats_ms[0] / 1000.0, period_ref))
        .unwrap_or(f64::INFINITY);

    let (med_ms, p95_ms) = beat_error_ms(&grid.beats_ms, &internal.beats_ms);

    // Pairwise drift profile: internal beat i vs reference beat i (both grids rigid, oct = 0,
    // counts comparable). A linear trend isolates a real tempo difference between the two
    // analyses; a constant offset is the known rekordbox MP3 clock shift.
    let paired = grid
        .beats_ms
        .iter()
        .zip(internal.beats_ms.iter())
        .map(|(r, i)| i - r)
        .collect::<Vec<_>>();
    let pick = |q: f64| -> f64 {
        let idx = (((paired.len() - 1) as f64) * q).round() as usize;
        paired.get(idx).copied().unwrap_or(f64::INFINITY)
    };

    let verdict = if octave.abs() > 0.5 {
        "FAIL"
    } else if bpm_rel > BPM_SLACK {
        "FAIL"
    } else {
        "ok"
    };
    let reason = match verdict {
        "ok" => format!("({})", internal.origin),
        _ if octave.abs() > 0.5 => format!("({}) octave {:+.0}", internal.origin, octave),
        _ => format!("({}) bpm off {:.2}%", internal.origin, bpm_rel * 100.0),
    };

    let line = format!(
        "{:>5} {:24} {:8.2} {:8.2} {:9.1} {:8.2} {:>3} {:8.4} {:8.1} {:4.1}/{:.1}  {}",
        grid.track_id,
        trunc(&grid.title, 24),
        grid.declared_bpm,
        report.oracle_bpm,
        report.oracle_anchor_ms,
        internal.bpm,
        octave as i64,
        ratio,
        phase_ms,
        med_ms,
        p95_ms,
        if verdict == "ok" {
            format!("ok")
        } else {
            format!("FAIL: {reason}")
        }
    );
    let line = if paired.len() >= 8 {
        format!(
            "{line}  [Δi {:+.0}/{:+.0}/{:+.0}ms]",
            pick(0.0),
            pick(0.5),
            pick(1.0)
        )
    } else {
        line
    };
    (verdict, line)
}

fn render(report: &Report, verdict: &str, msg: &str) -> String {
    let (int, oct, ratio, phase, err) = match &report.internal {
        Some(internal) => {
            let ratio = internal.bpm / report.oracle_bpm.max(1e-9);
            let oct = ratio.log2().round() as i64;
            let phase = internal
                .beats_ms
                .first()
                .map(|&b0| {
                    residual_ms(b0 / 1000.0, report.grid.beats_ms[0] / 1000.0, 60.0 / report.oracle_bpm.max(1e-9))
                })
                .unwrap_or(f64::INFINITY);
            let (med, p95) = beat_error_ms(&report.grid.beats_ms, &internal.beats_ms);
            (
                format!("{:8.2}", internal.bpm),
                format!("{oct:>3}"),
                format!("{ratio:8.4}"),
                format!("{phase:8.1}"),
                format!("{med:4.1}/{p95:.1}"),
            )
        }
        None => (
            format!("{:>8}", "-"),
            format!("{:>3}", "-"),
            format!("{:>8}", "-"),
            format!("{:>8}", "-"),
            format!("{:>9}", "-"),
        ),
    };
    format!(
        "{:>5} {:24} {:8.2} {:8.2} {:9.1} {int} {oct} {ratio} {phase} {err}  {verdict}: {msg}",
        report.grid.track_id,
        trunc(&report.grid.title, 24),
        report.grid.declared_bpm,
        report.oracle_bpm,
        report.oracle_anchor_ms,
    )
}
