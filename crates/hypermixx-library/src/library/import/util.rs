//! Shared file-side helpers for the source adapters: identity, hashing, and the extension set.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Extensions the library indexes. A source may point at any of these; the decoder decides later
/// whether a particular file is actually playable.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp1", "mp2", "wav", "aiff", "aif", "flac", "m4a", "mp4", "aac", "ogg", "oga",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension.as_str()))
}

pub fn relative_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// `(file_size, mtime_ns, sha256)` for one file. Hashing is streaming, so a large track does not
/// need to be resident in memory.
pub fn file_identity(path: &Path) -> std::result::Result<(i64, i64, String), String> {
    let metadata = std::fs::metadata(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let file_size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);
    let hash = hash_file(path)?;
    Ok((file_size, mtime_ns, hash))
}

pub fn hash_file(path: &Path) -> std::result::Result<String, String> {
    let mut file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Resolves a path written by another tool against a USB root. Source files often carry a Windows
/// drive or a `cdjexport=` prefix; the exported tree is the authority.
pub fn resolve_under_root(root: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let raw = raw
        .split_once('=')
        .map(|(_, path)| path)
        .unwrap_or(raw)
        .replace('\\', "/");
    let raw = raw.trim_start_matches('/');
    let raw = if raw.len() > 2 && raw.as_bytes()[1] == b':' {
        &raw[3..]
    } else {
        raw
    };
    let direct = root.join(raw);
    if direct.is_file() {
        return Some(direct);
    }
    // A Windows library path (`C:\Users\...`) is not on the USB; the exported copy usually keeps
    // only the file name.
    let name = Path::new(raw).file_name()?.to_string_lossy();
    for folder in ["", "Contents", "serato-export", "vdj-export"] {
        let candidate = root.join(folder).join(name.as_ref());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_extensions_are_case_insensitive() {
        assert!(is_audio(Path::new("a.MP3")));
        assert!(is_audio(Path::new("a.flac")));
        assert!(!is_audio(Path::new("cover.jpg")));
        assert!(!is_audio(Path::new("notes")));
    }

    #[test]
    fn a_hash_is_stable_for_the_same_bytes() {
        let path = std::env::temp_dir().join(format!(
            "hypermixx-hash-{}-{}",
            std::process::id(),
            crate::library::db::unix_now()
        ));
        std::fs::write(&path, b"hypermixx").unwrap();
        let a = hash_file(&path).unwrap();
        let b = hash_file(&path).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let _ = std::fs::remove_file(path);
    }
}
