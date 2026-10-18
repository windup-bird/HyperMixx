//! Where separated stems live between sessions.
//!
//! Separation costs ~80 s and ~2 GB for a three-minute track, so *not* redoing it is the difference
//! between a feature and a curiosity. The key is the model id plus the content hash of the mix, so
//! the same audio through different separators (or a retuned model) can never collide.
//!
//! Format: one directory per key, one raw interleaved-f32 file per stem, plus a tiny `meta` header.
//! Raw f32 rather than WAV because the engine wants exactly that and the files are private to this
//! cache — going through an encoder would only risk a format mismatch for no benefit. It is also the
//! format a future memory-mapped source can hand to the engine without a decode step.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use hypermixx_core::{Source, Stem, StemSet, CHANNELS, SAMPLE_RATE};
use hypermixx_media::{DecodedAudio, PcmPool};
use sha2::{Digest, Sha256};

use crate::StemError;

/// Stems the cache holds, and what it reports having read.
#[derive(Clone)]
pub struct CachedStems {
    pub stems: StemSet,
    /// Frames per stem.
    pub frames: u64,
}

/// Where separated audio is kept: `$HYPERMIXX_CACHE`, or **`.cache` in the working directory**.
///
/// Under the working directory rather than under `$HOME` on purpose: a separation is a few hundred
/// megabytes *per track*, it is only useful next to the tracks it belongs to, and deleting a project
/// directory should take its cached audio with it instead of leaving it in the home directory
/// forever. (An explicit `HYPERMIXX_CACHE` still overrides everything, which is what the tests use.)
pub fn cache_root() -> PathBuf {
    cache_root_from(std::env::var("HYPERMIXX_CACHE").ok().as_deref())
}

/// The rule itself, as a pure function: an override, else the working directory's `.cache`.
pub(crate) fn cache_root_from(override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(DEFAULT_CACHE_DIR),
    }
}

/// Where separated audio goes when nothing overrides it: the working directory.
pub const DEFAULT_CACHE_DIR: &str = ".cache";

/// The stem cache's directory.
pub fn stems_root() -> PathBuf {
    stems_root_from(std::env::var("HYPERMIXX_CACHE").ok().as_deref())
}

fn stems_root_from(override_dir: Option<&str>) -> PathBuf {
    cache_root_from(override_dir).join("stems")
}

/// The cache key for `separator`'s output over `pcm`.
///
/// Hashing the whole mix (~80 MB) costs a fraction of a second against an ~80 s separation, so it is
/// worth it: it makes the key *content*-addressed, and the cache therefore correct across renames,
/// re-encodes with identical PCM, and different files that decode the same.
pub fn cache_key(separator_id: &str, pcm: &[f32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(separator_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(SAMPLE_RATE.to_le_bytes());
    hasher.update((pcm.len() / CHANNELS).to_le_bytes());
    hasher.update(b"\0");
    for sample in pcm {
        hasher.update(sample.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn entry_dir(key: &str) -> PathBuf {
    stems_root().join(key)
}

fn stem_file(key: &str, stem: Stem) -> PathBuf {
    entry_dir(key).join(format!("{}.f32", stem.name()))
}

fn meta_file(key: &str) -> PathBuf {
    entry_dir(key).join("meta")
}

/// Stores a separated set. Best-effort: a cache that fails to write is not a failed separation, so
/// callers may ignore the error (the CLI reports it as a notice, not an error).
pub fn store_cached(key: &str, set: &StemSet) -> Result<(), StemError> {
    let dir = entry_dir(key);
    std::fs::create_dir_all(&dir)
        .map_err(|e| StemError::Io(format!("{}: {e}", dir.display())))?;
    let frames = set.total_frames();
    for stem in Stem::ALL {
        let source = set.get(stem);
        let path = stem_file(key, stem);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
        let mut written = 0u64;
        let mut buf = vec![0.0f32; (1 << 15) * CHANNELS];
        let mut bytes: Vec<u8> = Vec::with_capacity(buf.len() * 4);
        while written < frames {
            let want = (frames - written).min((buf.len() / CHANNELS) as u64) as usize;
            let read = source.read_frames(written, &mut buf[..want * CHANNELS]);
            if read == 0 {
                return Err(StemError::Io(format!(
                    "{}: source ran out at frame {written} of {frames}",
                    stem.name()
                )));
            }
            bytes.clear();
            for sample in &buf[..read * CHANNELS] {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            file.write_all(&bytes)
                .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
            written += read as u64;
        }
    }
    let mut meta = std::fs::File::create(meta_file(key))
        .map_err(|e| StemError::Io(format!("{}: {e}", meta_file(key).display())))?;
    writeln!(meta, "frames={frames}").ok();
    writeln!(meta, "sample_rate={SAMPLE_RATE}").ok();
    writeln!(meta, "stems={}", Stem::COUNT).ok();
    Ok(())
}

/// Appends one block to a cache entry, creating it if it is the first.
///
/// `blocks[i]` is interleaved stereo for stream `i`, all the same length. The entry's `meta` carries
/// the **completed** watermark, so a partial entry is a valid, if short, cache hit — which is what
/// makes a cancelled or interrupted job resumable instead of a rewrite. Best-effort: a cache that
/// fails to write is not a failed separation.
pub fn append_cached(
    key: &str,
    total_frames: u64,
    start_frame: u64,
    blocks: &[Vec<f32>],
) -> Result<(), StemError> {
    let dir = entry_dir(key);
    std::fs::create_dir_all(&dir)
        .map_err(|e| StemError::Io(format!("{}: {e}", dir.display())))?;
    let Some(first) = blocks.first() else {
        return Ok(());
    };
    let frames = (first.len() / CHANNELS) as u64;
    if blocks.iter().any(|block| block.len() != first.len()) {
        return Err(StemError::Io(
            "a cached block's streams must be frame-aligned".to_owned(),
        ));
    }
    for (index, samples) in blocks.iter().enumerate() {
        let Some(stem) = Stem::from_index(index) else {
            return Err(StemError::Io(format!("no stem for stream {index}")));
        };
        let path = stem_file(key, stem);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
        let mut bytes: Vec<u8> = Vec::with_capacity(samples.len() * 4);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        file.write_all(&bytes)
            .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
    }
    // The watermark moves only once every stream is on disk, so a crash mid-append leaves the entry
    // *shorter* than its files — which `load_cached` refuses (a half-written stem is a wrong result).
    let completed = start_frame + frames;
    let mut meta = std::fs::File::create(meta_file(key))
        .map_err(|e| StemError::Io(format!("{}: {e}", meta_file(key).display())))?;
    writeln!(meta, "frames={completed}").ok();
    writeln!(meta, "total={total_frames}").ok();
    writeln!(meta, "sample_rate={SAMPLE_RATE}").ok();
    writeln!(meta, "stems={}", Stem::COUNT).ok();
    Ok(())
}

/// How many frames an entry already holds, without reading any audio. `None` when there is no entry.
///
/// This is what makes opening a cache entry cheap: the answer decides whether a job replays, resumes
/// or starts from nothing, and none of those decisions should cost a file read the size of a track.
pub fn cached_frames(key: &str) -> Option<u64> {
    let meta = std::fs::read_to_string(meta_file(key)).ok()?;
    for line in meta.lines() {
        if let Some(value) = line.strip_prefix("frames=") {
            return value.trim().parse::<u64>().ok();
        }
    }
    None
}

/// Loads a cached set, or `None` when the key is absent or incomplete.
///
/// An incomplete entry (a crash mid-write) is treated as absent rather than repaired: the cache is
/// an optimisation, and a half-written stem would be a wrong result.
pub fn load_cached(key: &str) -> Option<CachedStems> {
    let meta = std::fs::read_to_string(meta_file(key)).ok()?;
    let mut frames = None;
    let mut sample_rate = None;
    for line in meta.lines() {
        let (name, value) = line.split_once('=')?;
        match name.trim() {
            "frames" => frames = value.trim().parse::<u64>().ok(),
            "sample_rate" => sample_rate = value.trim().parse::<u32>().ok(),
            _ => {}
        }
    }
    let frames = frames?;
    if sample_rate? != SAMPLE_RATE {
        // A rate change invalidates every cached stem; refuse rather than mis-report positions.
        return None;
    }

    let mut stems: [Arc<dyn Source>; Stem::COUNT] = std::array::from_fn(|_| empty_pool());
    for stem in Stem::ALL {
        let raw = std::fs::read(stem_file(key, stem)).ok()?;
        if raw.len() != frames as usize * CHANNELS * 4 {
            return None;
        }
        let mut pcm = vec![0.0f32; raw.len() / 4];
        for (i, chunk) in raw.as_chunks::<4>().0.iter().enumerate() {
            pcm[i] = f32::from_le_bytes(*chunk);
        }
        stems[stem.index()] = Arc::new(PcmPool::from_decoded(DecodedAudio {
            total_frames: frames,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            pcm,
        }));
    }
    Some(CachedStems {
        stems: StemSet::new(stems),
        frames,
    })
}

fn empty_pool() -> Arc<dyn Source> {
    Arc::new(PcmPool::from_decoded(DecodedAudio {
        pcm: Vec::new(),
        total_frames: 0,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    }))
}

/// Deletes all but the `keep` most recently written entries, returning `(removed, freed bytes)`.
///
/// A separated track is a few hundred megabytes, so an unbounded cache is a real problem rather than
/// a tidiness one. Recency is the entry directory's mtime, which is when its last stem was written.
pub fn prune_cache(keep: usize) -> Result<(usize, u64), StemError> {
    let Ok(dir) = std::fs::read_dir(stems_root()) else {
        return Ok((0, 0));
    };
    let mut entries: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let written = entry
            .metadata()
            .and_then(|meta| meta.modified())
            // An unreadable timestamp sorts last, so it is a candidate for removal rather than a
            // thing that is never removed.
            .unwrap_or(std::time::UNIX_EPOCH);
        let bytes = std::fs::read_dir(&path)
            .map(|inner| {
                inner
                    .flatten()
                    .filter_map(|file| file.metadata().ok())
                    .map(|meta| meta.len())
                    .sum()
            })
            .unwrap_or(0);
        entries.push((written, path, bytes));
    }
    // Newest first; `Reverse` keeps the ordering (and the `skip(keep)` below) readable.
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut removed = 0usize;
    let mut freed = 0u64;
    for (_, path, bytes) in entries.into_iter().skip(keep) {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                freed += bytes;
            }
            // A cache that cannot be pruned is not a failed separation; report what did go.
            Err(_) => continue,
        }
    }
    Ok((removed, freed))
}

/// Total bytes the cache is holding, and how many entries — for a `stem cache` report.
pub fn cache_usage() -> (u64, usize) {
    let mut bytes = 0u64;
    let mut entries = 0usize;
    let Ok(dir) = std::fs::read_dir(stems_root()) else {
        return (0, 0);
    };
    for entry in dir.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        entries += 1;
        if let Ok(inner) = std::fs::read_dir(entry.path()) {
            for file in inner.flatten() {
                if let Ok(meta) = file.metadata() {
                    bytes += meta.len();
                }
            }
        }
    }
    (bytes, entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockMode, MockSeparator, StemSeparator};
    use hypermixx_media::{DecodedAudio, PcmPool};

    /// The shared helper: one lock across every module that touches the cache.
    use crate::testing::with_isolated_cache;

    fn mix(frames: u64, tag: f32) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|i| [i as f32 + tag, -(i as f32) - tag])
                .collect(),
            total_frames: frames,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    fn read_all(source: &dyn Source) -> Vec<f32> {
        let mut out = vec![0.0f32; source.total_frames() as usize * CHANNELS];
        let n = source.read_frames(0, &mut out);
        out.truncate(n * CHANNELS);
        out
    }

    #[test]
    fn a_stored_set_round_trips_sample_for_sample() {
        with_isolated_cache("round-trip", || {
            let source = mix(1_000, 0.0);
            let set = MockSeparator::default()
                .separate(source.as_ref(), &crate::no_progress(), &crate::Cancelled::new())
                .unwrap();
            let key = cache_key("mock", &read_all(source.as_ref()));
            store_cached(&key, &set).unwrap();
            let loaded = load_cached(&key).expect("just stored");
            assert_eq!(loaded.frames, 1_000);
            for stem in Stem::ALL {
                assert_eq!(
                    read_all(loaded.stems.get(stem).as_ref()),
                    read_all(set.get(stem).as_ref()),
                    "{stem} did not survive the cache"
                );
            }
        });
    }

    /// The default location is a fact about this program, so it is pinned: a separation belongs next
    /// to the tracks it came from, not in the home directory forever.
    #[test]
    fn the_default_roots_are_in_the_working_directory() {
        assert_eq!(cache_root_from(None), PathBuf::from(".cache"));
        assert_eq!(stems_root_from(None), PathBuf::from(".cache/stems"));
        // An override wins, and an empty one is treated as absent rather than as "the root of `/`".
        assert_eq!(cache_root_from(Some("/tmp/x")), PathBuf::from("/tmp/x"));
        assert_eq!(cache_root_from(Some("  ")), PathBuf::from(".cache"));
        assert_eq!(cache_root_from(Some("")), PathBuf::from(".cache"));
    }

    #[test]
    fn a_missing_key_is_not_an_error_and_a_partial_entry_is_refused() {
        with_isolated_cache("partial", || {
            assert!(load_cached("deadbeef").is_none());
            // A directory that exists but has no meta is an interrupted write, not a hit.
            std::fs::create_dir_all(entry_dir("cafe")).unwrap();
            assert!(load_cached("cafe").is_none());
            // meta present, one stem missing.
            std::fs::write(meta_file("cafe"), "frames=10\nsample_rate=44100\nstems=4\n").unwrap();
            assert!(load_cached("cafe").is_none());
        });
    }

    #[test]
    fn the_key_is_content_addressed_not_path_or_name_addressed() {
        let a = vec![0.0f32; 100];
        let b = vec![0.0f32; 100];
        let mut c = vec![0.0f32; 100];
        c[50] = 1.0;
        assert_eq!(cache_key("m", &a), cache_key("m", &b));
        assert_ne!(cache_key("m", &a), cache_key("m", &c), "content");
        assert_ne!(cache_key("m", &a), cache_key("other", &a), "separator id");
    }

    #[test]
    fn pruning_keeps_the_newest_and_reports_what_it_freed() {
        with_isolated_cache("prune", || {
            let source = mix(100, 0.0);
            let set = MockSeparator::default()
                .separate(source.as_ref(), &crate::no_progress(), &crate::Cancelled::new())
                .unwrap();
            let mut keys = Vec::new();
            for tag in 0..3 {
                // Distinct keys, written in order; the mtime granularity on some filesystems is
                // coarse, so the *set* is asserted rather than which exact one survived.
                let key = format!("entry{tag}");
                store_cached(&key, &set).unwrap();
                keys.push(key);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(cache_usage().1, 3);
            let (removed, freed) = prune_cache(1).unwrap();
            assert_eq!(removed, 2);
            assert!(freed > 0);
            let (bytes, entries) = cache_usage();
            assert_eq!(entries, 1);
            assert!(bytes > 0);
            // The newest survives: it is the one written last.
            assert!(load_cached(keys.last().unwrap()).is_some());
            // Pruning to zero is how a user asks for "clear it".
            assert_eq!(prune_cache(0).unwrap().0, 1);
            assert_eq!(cache_usage(), (0, 0));
        });
    }

    #[test]
    fn usage_counts_entries() {
        with_isolated_cache("usage", || {
            let source = mix(100, 0.0);
            let set = MockSeparator::default()
                .separate(source.as_ref(), &crate::no_progress(), &crate::Cancelled::new())
                .unwrap();
            let key = cache_key("mock", &read_all(source.as_ref()));
            store_cached(&key, &set).unwrap();
            let (bytes, entries) = cache_usage();
            assert_eq!(entries, 1);
            // 4 stems × 100 frames × 2 ch × 4 bytes, plus the meta file.
            assert!(bytes >= 3_200, "{bytes}");
        });
    }

    #[test]
    fn the_mock_and_the_cache_agree_on_an_empty_track() {
        with_isolated_cache("empty", || {
            let source = mix(0, 0.0);
            let set = MockSeparator::new(MockMode::QuarterEach)
                .separate(source.as_ref(), &crate::no_progress(), &crate::Cancelled::new())
                .unwrap();
            let key = cache_key("mock", &[]);
            store_cached(&key, &set).unwrap();
            let loaded = load_cached(&key).unwrap();
            assert_eq!(loaded.frames, 0);
            // A spec-less entry still has four addressable (empty) stems.
            assert_eq!(loaded.stems.total_frames(), 0);
        });
    }
}
