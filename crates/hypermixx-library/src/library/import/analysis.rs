//! Shared ANLZ sidecar import. Rekordbox and VirtualDJ both write the same `PQTZ`/`PCO2` tags;
//! only the provenance and clock calibration differ.

use std::path::Path;

use rusqlite::Connection;

use super::super::db::{insert_beatgrid, insert_cue, Result};
use super::super::model::ScanStats;
use super::anlz::{Anlz, AnlzCue};

pub(crate) fn import_sidecars(
    conn: &Connection,
    source_id: i64,
    location_id: i64,
    dat: &Path,
    ext: &Path,
    offset_ms: f64,
    origin: &str,
    clock: &str,
    stats: &mut ScanStats,
) -> Result<()> {
    let dat_anlz = match Anlz::parse(dat) {
        Ok(anlz) => Some(anlz),
        Err(err) => {
            stats.errors.push(err.to_string());
            None
        }
    };
    if let Some(dat) = &dat_anlz {
        if !dat.beats.is_empty() {
            let beats: Vec<(u16, f32, f64)> = dat
                .beats
                .iter()
                .map(|beat| (beat.beat, beat.bpm, beat.time_ms))
                .collect();
            let bpm = dat.beats.first().map(|beat| beat.bpm);
            let anchor = dat.beats.first().map(|beat| beat.time_ms);
            insert_beatgrid(
                conn,
                location_id,
                source_id,
                origin,
                bpm,
                anchor,
                clock,
                offset_ms,
                Some(1.0),
                "anlz-v1",
                &beats,
            )?;
        }
    }

    let cues = match Anlz::parse(ext) {
        Ok(anlz) if !anlz.cues.is_empty() => anlz.cues,
        Ok(_) => dat_anlz.map(|anlz| anlz.cues).unwrap_or_default(),
        Err(err) => {
            stats.errors.push(err.to_string());
            dat_anlz.map(|anlz| anlz.cues).unwrap_or_default()
        }
    };
    for (seq, cue) in cues.iter().enumerate() {
        let (kind, idx) = cue_kind(cue, seq);
        let color = cue.color.map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"));
        insert_cue(
            conn,
            location_id,
            source_id,
            origin,
            kind,
            Some(idx),
            cue.time_ms,
            clock,
            offset_ms,
            color.as_deref(),
            None,
        )?;
    }
    Ok(())
}

fn cue_kind(cue: &AnlzCue, sequence: usize) -> (&'static str, i64) {
    if cue.is_loop {
        (
            if cue.hot_cue > 0 { "hot_loop" } else { "loop" },
            if cue.hot_cue > 0 {
                i64::from(cue.hot_cue.saturating_sub(1))
            } else {
                sequence as i64
            },
        )
    } else if cue.hot_cue > 0 {
        ("hot", i64::from(cue.hot_cue.saturating_sub(1)))
    } else {
        ("memory", sequence as i64)
    }
}
