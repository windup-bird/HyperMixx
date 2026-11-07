//! Shared file-side helpers for the source adapters: identity, hashing, and the extension set.

use std::path::{Path, PathBuf};

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

/// `(file_size, mtime_ns)` for one file — a single `stat`, no content is read. Scans are
/// metadata-only by design: the export databases carry the tags, and an unchanged
/// `(source, path, size, mtime)` reuses the cached row wholesale. SHA-256 was dropped from the
/// scan path; if content-keyed dedup is ever needed again, it belongs in a background job.
pub fn file_stat(path: &Path) -> std::result::Result<(i64, i64), String> {
    let metadata = std::fs::metadata(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let file_size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);
    Ok((file_size, mtime_ns))
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
    fn a_stat_reports_size_and_mtime() {
        let path = std::env::temp_dir().join(format!(
            "hypermixx-stat-{}-{}",
            std::process::id(),
            crate::library::db::unix_now()
        ));
        std::fs::write(&path, b"hypermixx").unwrap();
        let (size, mtime) = file_stat(&path).unwrap();
        assert_eq!(size, 9);
        assert!(mtime > 0);
        let again = file_stat(&path).unwrap();
        assert_eq!(size, again.0);
        assert_eq!(mtime, again.1);
        let _ = std::fs::remove_file(path);
    }
}
