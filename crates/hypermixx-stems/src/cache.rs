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

/// `$HYPERMIXX_CACHE`, or `~/.cache/hypermixx`.
pub fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("HYPERMIXX_CACHE") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".cache").join("hypermixx")
}

/// The stem cache's directory.
pub fn stems_root() -> PathBuf {
    cache_root().join("stems")
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

    /// Every test in this module gets its own cache root, so they cannot see each other. The cache
    /// root is process-wide (`HYPERMIXX_CACHE`), so these tests are serialised — otherwise two of
    /// them would race on the same environment variable.
    fn with_isolated_cache<T>(name: &str, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hypermixx-cache-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("HYPERMIXX_CACHE", &dir);
        let out = body();
        std::env::remove_var("HYPERMIXX_CACHE");
        out
    }

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
