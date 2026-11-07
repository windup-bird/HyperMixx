//! Removable-media discovery and mounting.
//!
//! Linux already knows which block devices are removable (`/sys/block/*/removable`); the missing
//! piece for a browser is to notice a device appearing, ask udisks2 to mount it, and tell the UI
//! when it goes away. This module only observes and asks; it never writes to the medium.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crossbeam_channel::Sender;

/// One media lifecycle transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbEvent {
    Mounted(PathBuf),
    Unmounted(PathBuf),
}

/// Polls removable block devices forever. The thread is detached: process exit is the shutdown
/// signal, and the UI drops the receiver when it closes.
pub fn watch(tx: Sender<UsbEvent>) {
    let mut mounted: HashMap<String, PathBuf> = HashMap::new();
    let mut attempted: HashSet<String> = HashSet::new();
    loop {
        let current = scan_removable(&mut attempted);
        for (node, path) in &current {
            if !mounted.contains_key(node) {
                let _ = tx.send(UsbEvent::Mounted(path.clone()));
            }
        }
        for (node, path) in &mounted {
            if !current.contains_key(node) {
                attempted.remove(node);
                let _ = tx.send(UsbEvent::Unmounted(path.clone()));
            }
        }
        mounted = current;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The currently mounted removable partitions, keyed by block-device node.
fn scan_removable(attempted: &mut HashSet<String>) -> HashMap<String, PathBuf> {
    let mounts = read_mounts();
    let mut out = HashMap::new();
    let Ok(blocks) = std::fs::read_dir("/sys/block") else {
        return out;
    };
    for block in blocks.flatten() {
        let name = block.file_name().to_string_lossy().into_owned();
        if !is_removable(&block.path()) {
            continue;
        }
        for node in partitions(&name) {
            if let Some(path) = mounts.get(&node) {
                out.insert(node, path.clone());
            } else if attempted.insert(node.clone()) {
                if let Some(path) = udisks_mount(&node) {
                    out.insert(node, path);
                }
            }
        }
    }
    out
}

fn is_removable(block: &std::path::Path) -> bool {
    let removable = std::fs::read_to_string(block.join("removable")).unwrap_or_default();
    let size = std::fs::read_to_string(block.join("size")).unwrap_or_default();
    removable.trim() == "1" && size.trim().parse::<u64>().unwrap_or(0) > 0
}

fn partitions(block: &str) -> Vec<String> {
    let path = std::path::Path::new("/sys/block").join(block);
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&path) {
        for entry in entries.flatten() {
            if entry.path().join("partition").is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                parts.push(format!("/dev/{name}"));
            }
        }
    }
    if parts.is_empty() {
        parts.push(format!("/dev/{block}"));
    }
    parts
}

fn read_mounts() -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
        return out;
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
            continue;
        };
        out.insert(source.to_owned(), PathBuf::from(decode_mount(target)));
    }
    out
}

fn udisks_mount(node: &str) -> Option<PathBuf> {
    let output = Command::new("udisksctl")
        .args(["mount", "-b", node, "--no-user-interaction"])
        .output()
        .ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some((_, path)) = text.rsplit_once(" at ") {
            return Some(PathBuf::from(path.trim()));
        }
    }
    // The mount can race with the poll; read the table again before giving up.
    read_mounts().get(node).cloned()
}

/// `/proc/mounts` escapes spaces and a few control characters as `\ooo`.
fn decode_mount(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &text[index + 1..index + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(value as char);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(decode_mount("/run/media/a\\040b"), "/run/media/a b");
        assert_eq!(decode_mount("/plain"), "/plain");
    }

    #[test]
    fn scanning_removable_does_not_panic() {
        let _ = scan_removable(&mut HashSet::new());
    }

    #[test]
    #[ignore = "requires a mounted removable medium"]
    fn a_mounted_removable_is_discovered() {
        let mounted = scan_removable(&mut HashSet::new());
        assert!(!mounted.is_empty(), "no mounted removable medium");
        println!("{mounted:?}");
    }
}
