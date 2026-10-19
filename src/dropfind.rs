//! Resolve browser-dropped files back to local paths.
//!
//! Chromium never exposes a dropped file's absolute path to page JavaScript.
//! It does expose name, byte size and modified time. The UI sends those three
//! facts and this module finds the matching local file, so a drop from Explorer
//! can be added by reference without uploading gigabytes of media.
//!
//! A match requires the same file name (case-insensitive), exact size and an
//! mtime within two seconds. Likely media folders are searched first, then the
//! rest of the profile and fixed drives with a hard time budget. System/cache
//! folders and symbolic links/junctions are skipped.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Deserialize, Clone, Debug)]
pub struct DroppedFile {
    pub name: String,
    pub size: u64,
    /// File.lastModified: milliseconds since the Unix epoch.
    pub modified: i64,
}

extern "system" {
    fn GetDriveTypeW(root: *const u16) -> u32;
}

const DRIVE_FIXED: u32 = 3;
const MTIME_TOLERANCE_MS: i64 = 2000;
// Matches AppData, caches and system trees at any depth.
const SKIP_ANYWHERE: &[&str] = &["$recycle.bin", "system volume information", "node_modules", ".git", "appdata", "$windows.~bt", "$windows.~ws"];
// Only skipped directly below a drive root, so user folders with these names still work.
const SKIP_AT_DRIVE_ROOT: &[&str] = &["windows", "program files", "program files (x86)", "programdata", "recovery", "perflogs"];

fn fixed_drives() -> Vec<PathBuf> {
    let mut drives = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string for this call.
        if unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_FIXED {
            drives.push(PathBuf::from(root));
        }
    }
    drives
}

fn search_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        for name in ["Downloads", "Desktop", "Videos", "Documents", "Music", "Pictures"] {
            roots.push(profile.join(name));
        }
        if let Some(one_drive) = std::env::var_os("OneDrive").map(PathBuf::from) {
            roots.push(one_drive);
        }
        roots.push(profile);
    }
    roots.extend(fixed_drives());
    let mut seen: Vec<String> = Vec::new();
    roots.retain(|root| {
        let key = root.to_string_lossy().to_lowercase();
        if !root.is_dir() || seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
    roots
}

#[derive(PartialEq)]
enum ScanEnd {
    Finished,
    AllFound,
    TimedOut,
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

fn is_drive_root_child(dir: &Path) -> bool {
    dir.parent().map(|p| p.parent().is_none()).unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn scan(
    root: &Path,
    files: &[DroppedFile],
    wanted: &HashMap<String, Vec<usize>>,
    found: &mut [Option<PathBuf>],
    remaining: &mut usize,
    deadline: Instant,
    already_scanned: &[PathBuf],
) -> ScanEnd {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if Instant::now() >= deadline {
            return ScanEnd::TimedOut;
        }
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };
        for entry in read_dir.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if kind.is_dir() {
                let path = entry.path();
                if SKIP_ANYWHERE.contains(&name.as_str())
                    || (is_drive_root_child(&path) && SKIP_AT_DRIVE_ROOT.contains(&name.as_str()))
                    || already_scanned.iter().any(|done| done.as_os_str().eq_ignore_ascii_case(path.as_os_str()))
                {
                    continue;
                }
                stack.push(path);
            } else if kind.is_file() {
                let Some(indexes) = wanted.get(&name) else { continue };
                let Ok(meta) = entry.metadata() else { continue };
                let Some(modified) = modified_ms(&meta) else { continue };
                for &index in indexes {
                    let wanted_file = &files[index];
                    if found[index].is_none()
                        && meta.len() == wanted_file.size
                        && (modified - wanted_file.modified).abs() <= MTIME_TOLERANCE_MS
                    {
                        found[index] = Some(entry.path());
                        *remaining -= 1;
                    }
                }
                if *remaining == 0 {
                    return ScanEnd::AllFound;
                }
            }
        }
    }
    ScanEnd::Finished
}

/// Returns one optional path per input file, in the same order, plus whether
/// the time budget ended before every unresolved file could be searched for.
pub fn resolve(files: &[DroppedFile], budget: Duration) -> (Vec<Option<PathBuf>>, bool) {
    let mut wanted: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        wanted.entry(file.name.to_lowercase()).or_default().push(index);
    }
    let mut found: Vec<Option<PathBuf>> = vec![None; files.len()];
    let mut remaining = files.len();
    let deadline = Instant::now() + budget;
    let mut scanned: Vec<PathBuf> = Vec::new();
    let mut timed_out = false;
    for root in search_roots() {
        if remaining == 0 {
            break;
        }
        match scan(&root, files, &wanted, &mut found, &mut remaining, deadline, &scanned) {
            ScanEnd::AllFound => break,
            ScanEnd::TimedOut => {
                timed_out = true;
                break;
            }
            ScanEnd::Finished => scanned.push(root),
        }
    }
    (found, timed_out && remaining > 0)
}
