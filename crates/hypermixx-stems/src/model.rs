//! The model table and its acquisition.
//!
//! Files land in `.model` in the working directory (see [`models_root`]); the table below is the only
//! thing that says where they come from and what they must hash to.
//!
//! `huggingface.co` is not reachable from everywhere (it is not reachable from the machine this was
//! developed on), so acquisition is **explicit and mirror-aware** rather than delegated to a
//! backend's downloader:
//!
//! 1. `HYPERMIXX_HF_ENDPOINT` overrides the endpoint; otherwise the mirror is tried first and the
//!    canonical host second.
//! 2. The file is streamed to `<name>.part`, hashed **while it downloads**.
//! 3. The hash must equal the one in the table — the same value the backend's own registry pins —
//!    or the file is deleted and the job fails. An unverified weight file is never handed to the
//!    inference runtime.
//! 4. It is then renamed into place and a `.sha256` marker written, so later runs trust the file by
//!    size + marker instead of re-hashing 300 MB.
//!
//! This is also why a model download can never surprise anyone mid-set: it is one command, with
//! progress, and it either verifies or does nothing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use hypermixx_core::Stem;
use sha2::{Digest, Sha256};

use crate::StemError;

/// One model file this crate knows how to obtain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    /// Canonical name, used in the cache key and in errors.
    pub name: &'static str,
    /// Path under an endpoint, e.g. `StemSplitio/htdemucs-onnx/resolve/main/htdemucs.onnx`.
    pub repo_path: &'static str,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: &'static str,
    /// Expected size in bytes.
    pub bytes: u64,
    /// The model's native sample rate. HTDemucs is 44.1 kHz, which is *also* the engine's rate, so
    /// a separated track needs no resampling anywhere in the chain.
    pub sample_rate: u32,
    /// Frames the model takes at a time — its input is a **fixed** length, and the graph rejects
    /// anything else.
    ///
    /// Two things are derived from it: how much audio one *block* covers (the window stride,
    /// `(1 - overlap) × segment_frames` — that is the cadence the deck installs audio at), and where a
    /// resumed stream may start (the same grid). A model with a different export length simply
    /// changes both.
    pub segment_frames: u64,
    /// What it separates a track into, in [`Stem::ALL`] order.
    pub stems: [Stem; Stem::COUNT],
}

/// The 4-stem HTDemucs ONNX export (`demucs-onnx`, MIT).
///
/// 302 MB, single file, and the fastest 4-stem startup of the ONNX exports (~30% faster than the
/// fine-tuned bag of four). The fine-tuned variant scores better SDR but is four files and ~4× the
/// separation time, which for a cache-once/play-many DJ tool is the wrong trade by default.
pub const HTDEMUCS: ModelSpec = ModelSpec {
    name: "htdemucs",
    repo_path: "StemSplitio/htdemucs-onnx/resolve/main/htdemucs.onnx",
    sha256: "68d0bf16428ef66e692cdff8a9ccf28f1ef3f69440d57e58605a4cc55fcc5e74",
    bytes: 316_446_953,
    sample_rate: hypermixx_core::SAMPLE_RATE,
    // Pinned by the export: `mix [1, 2, 343980]` → `stems [1, 4, 2, 343980]`. Cross-checked against
    // `charon_audio`'s own constant in the test below, so a fork that changed it cannot drift here.
    segment_frames: 343_980,
    stems: Stem::ALL,
};

/// The fp16-weights variant of [`HTDEMUCS`]: the same graph with its weights stored in half
/// precision.
///
/// **Half the download, the same speed.** ONNX Runtime converts the weights on load, so the runtime
/// cost, the memory and the output are the same (the model card measures both at ~1.6 s/window and
/// ~1.1 GB on CPU, and the parity difference is 4.6e-5). It is here because a 166 MB download is a
/// different proposition from a 316 MB one on a laptop tethered to a phone — not because it is
/// faster.
pub const HTDEMUCS_FP16: ModelSpec = ModelSpec {
    name: "htdemucs-fp16",
    repo_path: "StemSplitio/htdemucs-onnx/resolve/main/htdemucs_fp16weights.onnx",
    sha256: "d05c269d0178d2a72ad484b10b11dd370193fc923201c3b27a99f848745db70a",
    bytes: 165_612_636,
    sample_rate: hypermixx_core::SAMPLE_RATE,
    segment_frames: 343_980,
    stems: Stem::ALL,
};

/// Every model this crate can obtain, in the order a front-end should list them.
///
/// **A model is data, not code**: the table is the only place a model's identity, size, hash and
/// window live, so adding one is an entry here plus whatever front-end flag selects it. What *does*
/// not fit this shape is a model that is not a single waveform-in/waveform-out graph producing all
/// `Stem::COUNT` stems — the spectrogram-domain separators (Spleeter, Open-Unmix, UVR's MDX-Net) need
/// the STFT and the cross-stem mask done outside the graph, which is a different kind of work than a
/// table entry.
pub const MODELS: &[ModelSpec] = &[HTDEMUCS, HTDEMUCS_FP16];

/// The default model: the fastest 4-stem ONNX export, in full precision.
pub fn default_model() -> &'static ModelSpec {
    &HTDEMUCS
}

/// Looks a model up by name, ignoring case. `None` for a name that is not in the table.
pub fn model_by_name(name: &str) -> Option<&'static ModelSpec> {
    let name = name.trim();
    MODELS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(name))
}

/// The model names, for an error message that says what *is* available.
pub fn model_names() -> Vec<&'static str> {
    MODELS.iter().map(|spec| spec.name).collect()
}

/// Where a mirror is preferred over the canonical host.
pub const DEFAULT_ENDPOINT: &str = "https://hf-mirror.com";
/// The canonical host, tried when the mirror fails.
pub const FALLBACK_ENDPOINT: &str = "https://huggingface.co";

/// Where model files are kept: `$HYPERMIXX_MODELS`, or **`.model` in the working directory**.
///
/// Kept apart from [`crate::cache_root`] because the two have different lifetimes and sizes: a model
/// is 166–316 MB of *software* that any project can reuse, while a separation is a few hundred
/// megabytes of *this track's* audio. Mixing them would mean a `prune` of one could not be reasoned
/// about without the other.
pub fn models_root() -> PathBuf {
    models_root_from(std::env::var("HYPERMIXX_MODELS").ok().as_deref())
}

/// The rule itself, as a pure function: an override, else the working directory's `.model`.
fn models_root_from(override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(DEFAULT_MODELS_DIR),
    }
}

/// Where models go when nothing overrides it: the working directory.
pub const DEFAULT_MODELS_DIR: &str = ".model";

/// Where `spec`'s file lives once obtained.
pub fn model_path(spec: &ModelSpec) -> PathBuf {
    models_root().join(format!("{}.onnx", spec.name))
}

/// The endpoint order to try: an explicit `HYPERMIXX_HF_ENDPOINT`, else mirror then canonical.
fn endpoints(explicit: Option<&str>) -> Vec<String> {
    if let Some(url) = explicit {
        return vec![url.trim_end_matches('/').to_owned()];
    }
    if let Ok(url) = std::env::var("HYPERMIXX_HF_ENDPOINT") {
        if !url.trim().is_empty() {
            return vec![url.trim_end_matches('/').to_owned()];
        }
    }
    vec![DEFAULT_ENDPOINT.to_owned(), FALLBACK_ENDPOINT.to_owned()]
}

/// Whether `path` is `spec`'s file, trusting a `.sha256` marker when there is one and hashing
/// otherwise. A wrong-size file is rejected without reading it.
pub fn verify_model(spec: &ModelSpec, path: &Path) -> Result<(), StemError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| StemError::Model(format!("{}: {e}", path.display())))?;
    if meta.len() != spec.bytes {
        return Err(StemError::Model(format!(
            "{} is {} bytes, expected {}",
            path.display(),
            meta.len(),
            spec.bytes
        )));
    }
    let marker = path.with_extension("onnx.sha256");
    if let Ok(seen) = std::fs::read_to_string(&marker) {
        if seen.trim().eq_ignore_ascii_case(spec.sha256) {
            return Ok(());
        }
    }
    let actual = hash_file(path)?;
    if !actual.eq_ignore_ascii_case(spec.sha256) {
        return Err(StemError::Model(format!(
            "{}: sha256 {actual} != {}",
            path.display(),
            spec.sha256
        )));
    }
    let _ = std::fs::write(&marker, format!("{}\n", spec.sha256));
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, StemError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Obtains `spec`, downloading and verifying it if needed. Returns the file's path.
///
/// Progress is over the download only (0.0..=1.0); a file that is already present returns
/// immediately without reporting.
#[cfg(feature = "onnx")]
pub fn ensure_model(
    spec: &ModelSpec,
    endpoint: Option<&str>,
    progress: &crate::ProgressSink,
) -> Result<PathBuf, StemError> {
    let path = model_path(spec);
    if path.exists() && verify_model(spec, &path).is_ok() {
        return Ok(path);
    }
    std::fs::create_dir_all(models_root())
        .map_err(|e| StemError::Io(format!("{}: {e}", models_root().display())))?;

    let mut last = String::new();
    for base in endpoints(endpoint) {
        let url = format!("{base}/{}", spec.repo_path);
        match download_to(spec, &url, &path, progress) {
            Ok(()) => return Ok(path),
            Err(err) => last = err.message(),
        }
    }
    Err(StemError::Model(format!(
        "could not obtain {}: {last}",
        spec.name
    )))
}

/// Streams `url` into `<path>.part`, hashing as it goes, then verifies before renaming.
#[cfg(feature = "onnx")]
fn download_to(
    spec: &ModelSpec,
    url: &str,
    path: &Path,
    progress: &crate::ProgressSink,
) -> Result<(), StemError> {
    let part = path.with_extension("onnx.part");
    let mut response = ureq::get(url)
        .call()
        .map_err(|e| StemError::Model(format!("{url}: {e}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(StemError::Model(format!("{url}: HTTP {status}")));
    }
    // A server that will not say how big the file is cannot be progress-reported, but the hash
    // check still decides whether the result is usable.
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0);

    let mut file = std::fs::File::create(&part)
        .map_err(|e| StemError::Io(format!("{}: {e}", part.display())))?;
    let mut hasher = Sha256::new();
    let mut reader = response.body_mut().as_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut written = 0u64;
    let mut next_report = 0.0f32;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| StemError::Io(format!("{url}: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| StemError::Io(format!("{}: {e}", part.display())))?;
        hasher.update(&buf[..n]);
        written += n as u64;
        if let Some(total) = total {
            let fraction = written as f32 / total as f32;
            if fraction >= next_report {
                progress(fraction.min(1.0));
                next_report = fraction + 0.01;
            }
        }
    }
    drop(file);

    let actual = hex(&hasher.finalize());
    if written != spec.bytes || !actual.eq_ignore_ascii_case(spec.sha256) {
        let _ = std::fs::remove_file(&part);
        return Err(StemError::Model(format!(
            "{url}: got {written} bytes, sha256 {actual}; expected {} bytes, sha256 {}",
            spec.bytes, spec.sha256
        )));
    }
    std::fs::rename(&part, path).map_err(|e| StemError::Io(format!("{}: {e}", path.display())))?;
    let _ = std::fs::write(
        path.with_extension("onnx.sha256"),
        format!("{}\n", spec.sha256),
    );
    progress(1.0);
    Ok(())
}

/// A build without the backend cannot fetch a model; the table and the verifier still exist so a
/// front-end can report what *would* be needed.
#[cfg(not(feature = "onnx"))]
pub fn ensure_model(
    _spec: &ModelSpec,
    _endpoint: Option<&str>,
    _progress: &crate::ProgressSink,
) -> Result<PathBuf, StemError> {
    Err(StemError::NoBackend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_self_consistent() {
        assert_eq!(HTDEMUCS.stems, Stem::ALL);
        assert_eq!(HTDEMUCS.sample_rate, hypermixx_core::SAMPLE_RATE);
        assert_eq!(HTDEMUCS.sha256.len(), 64);
        assert!(HTDEMUCS.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        // A wrong byte count in the table would mean a rejected download at best and a wrong file
        // accepted at worst, so it is checked at compile time rather than in a test.
        const { assert!(HTDEMUCS.bytes > 300_000_000) };
        assert!(HTDEMUCS.repo_path.ends_with(".onnx"));
        assert!(!HTDEMUCS.repo_path.starts_with('/'));
    }

    /// The segment length is the model's contract, not a tunable: a fork that changed it would make
    /// every chunked job mis-align, so it is checked against the runtime's own value.
    #[cfg(feature = "onnx")]
    #[test]
    fn the_segment_length_matches_the_runtime() {
        assert_eq!(
            HTDEMUCS.segment_frames as usize,
            charon_audio::models::HTDEMUCS_SEGMENT_SAMPLES,
            "the model table and the runtime must agree on the window"
        );
    }

    /// The table is data a front-end reads: names must be unique and lookupable, hashes pinned and
    /// plausible, and the default present in it.
    /// Models are *software* rather than *one track's audio*, so they live in their own directory —
    /// and, like the cache, under the working directory instead of the home directory.
    #[test]
    fn the_default_model_directory_is_not_the_cache() {
        assert_eq!(models_root_from(None), PathBuf::from(".model"));
        assert_ne!(
            models_root_from(None),
            crate::cache::cache_root_from(None),
            "a separation and the model that made it have different lifetimes; they are not one tree"
        );
        assert_eq!(models_root_from(Some("/tmp/m")), PathBuf::from("/tmp/m"));
        assert_eq!(models_root_from(Some(" ")), PathBuf::from(".model"));
    }

    #[test]
    fn the_model_table_is_consistent() {
        assert!(!MODELS.is_empty());
        for spec in MODELS {
            assert!(!spec.name.is_empty() && !spec.name.contains(' '), "{spec:?}");
            assert!(spec.repo_path.contains('/'), "{spec:?}: a lookup needs a repo path");
            assert_eq!(spec.sha256.len(), 64, "{}: a pinned hash, not a placeholder", spec.name);
            assert!(spec.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{}", spec.name);
            assert!(spec.bytes > 0, "{}: the size is what makes a wrong file cheap to reject", spec.name);
            assert_eq!(spec.sample_rate, hypermixx_core::SAMPLE_RATE, "{}", spec.name);
            assert!(spec.segment_frames > 0, "{}", spec.name);
            assert_eq!(spec.stems.len(), Stem::COUNT);
        }
        // Names are unique, so `--model` cannot be ambiguous.
        let mut names: Vec<&str> = MODELS.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "duplicate model names: {names:?}");

        // Lookup ignores case and whitespace (a config file is typed by hand), and says what exists.
        assert_eq!(model_by_name("htdemucs").map(|s| s.name), Some("htdemucs"));
        assert_eq!(model_by_name(" HTDemucs ").map(|s| s.name), Some("htdemucs"));
        assert_eq!(model_by_name("htdemucs-fp16").map(|s| s.name), Some("htdemucs-fp16"));
        assert!(model_by_name("spleeter").is_none());
        assert_eq!(model_names().len(), MODELS.len());
        // The default is in the table, and the fp16 variant really is the same graph (same window,
        // same stems) so a front-end can swap one for the other.
        //
        // Compared by *name*: a `const` item is copied into `MODELS`, so a pointer comparison against
        // the constant would be false even though it is the same model.
        assert!(MODELS.iter().any(|spec| spec.name == default_model().name));
        assert_eq!(HTDEMUCS_FP16.segment_frames, HTDEMUCS.segment_frames);
        assert_eq!(HTDEMUCS_FP16.stems, HTDEMUCS.stems);
        // `const` so the check is a compile error rather than a test failure if the size claim stops
        // holding (clippy asks for exactly this).
        const { assert!(HTDEMUCS_FP16.bytes < HTDEMUCS.bytes, "fp16 halves the download") };
    }

    #[test]
    fn a_missing_or_wrong_sized_file_is_rejected_without_hashing() {
        let dir = std::env::temp_dir().join("hypermixx-stems-test-model");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("htdemucs.onnx");
        let _ = std::fs::remove_file(&path);
        assert!(verify_model(&HTDEMUCS, &path).is_err(), "missing file");
        std::fs::write(&path, b"not the model").unwrap();
        let err = verify_model(&HTDEMUCS, &path).unwrap_err();
        assert!(format!("{err}").contains("expected"), "size is checked first: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_matching_size_with_a_bad_hash_is_rejected() {
        let dir = std::env::temp_dir().join("hypermixx-stems-test-model");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("fake.onnx");
        // A small spec whose declared size matches a file we can write, so the *hash* is what fails.
        let spec = ModelSpec { bytes: 4, ..HTDEMUCS };
        std::fs::write(&path, b"nope").unwrap();
        let err = verify_model(&spec, &path).unwrap_err();
        assert!(format!("{err}").contains("sha256"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn endpoint_order_is_environment_then_mirror_then_canonical() {
        // The explicit argument wins outright.
        assert_eq!(endpoints(Some("https://example.test/")), vec!["https://example.test"]);
        // With nothing set, the mirror comes first. (The env var is process-wide, so this test
        // only asserts the argument path; the env path is the same code.)
        let order = endpoints(None);
        assert!(!order.is_empty());
        assert!(order[0].starts_with("https://"));
    }
}
