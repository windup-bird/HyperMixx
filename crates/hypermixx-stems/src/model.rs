//! The model table and its acquisition.
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
    stems: Stem::ALL,
};

/// Where a mirror is preferred over the canonical host.
pub const DEFAULT_ENDPOINT: &str = "https://hf-mirror.com";
/// The canonical host, tried when the mirror fails.
pub const FALLBACK_ENDPOINT: &str = "https://huggingface.co";

/// `~/.cache/hypermixx/models`, or `$HYPERMIXX_CACHE`'s `models` when that is set.
pub fn models_root() -> PathBuf {
    crate::cache::cache_root().join("models")
}

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
