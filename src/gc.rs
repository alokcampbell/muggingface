use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{info, warn};

pub const DISK_USAGE_THRESHOLD_PERCENT: u64 = 50;
const KEEP_NEWEST: usize = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntry {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

/// Oldest first, never the newest `keep_newest` dirs, until `bytes_to_free` is covered.
pub fn plan_deletions(mut dirs: Vec<SeedEntry>, bytes_to_free: u64, keep_newest: usize) -> Vec<PathBuf> {
    if bytes_to_free == 0 || dirs.len() <= keep_newest {
        return Vec::new();
    }
    dirs.sort_by_key(|d| d.mtime);
    let cutoff = dirs.len().saturating_sub(keep_newest);
    let mut freed = 0u64;
    let mut out = Vec::new();
    for entry in dirs.into_iter().take(cutoff) {
        if freed >= bytes_to_free {
            break;
        }
        freed = freed.saturating_add(entry.size);
        out.push(entry.path);
    }
    out
}

pub fn bytes_to_free(total: u64, used: u64, threshold_percent: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let limit = total.saturating_mul(threshold_percent) / 100;
    used.saturating_sub(limit)
}

#[cfg(unix)]
pub fn filesystem_usage(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut s) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let frsize = s.f_frsize as u64;
    let total = s.f_blocks as u64 * frsize;
    let avail = s.f_bavail as u64 * frsize;
    let used = total.saturating_sub(avail);
    Ok((total, used))
}

fn dir_size(path: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let mut n = 0u64;
        let Ok(rd) = fs::read_dir(p) else {
            return 0;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let Ok(meta) = ent.metadata() else {
                continue;
            };
            if meta.is_dir() {
                n += walk(&path);
            } else {
                n += meta.len();
            }
        }
        n
    }
    walk(path)
}

pub fn list_model_dirs(seeding: &Path) -> Vec<SeedEntry> {
    let Ok(rd) = fs::read_dir(seeding) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|ent| {
            let path = ent.path();
            let meta = ent.metadata().ok()?;
            if !meta.is_dir() {
                return None;
            }
            Some(SeedEntry {
                path,
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: dir_size(&ent.path()),
            })
        })
        .collect()
}

/// Delete oldest cloned model directories when the seeding filesystem is above the threshold.
/// Keeps `.torrent` files and postgres; magnet pages still work from the DB.
pub fn prune_if_needed(seeding: &Path) -> std::io::Result<Vec<PathBuf>> {
    let (total, used) = filesystem_usage(seeding)?;
    let used_pct = if total == 0 { 0 } else { used * 100 / total };
    if used_pct <= DISK_USAGE_THRESHOLD_PERCENT {
        info!("disk usage {used_pct}% <= {DISK_USAGE_THRESHOLD_PERCENT}%, skipping model GC");
        return Ok(Vec::new());
    }
    let need = bytes_to_free(total, used, DISK_USAGE_THRESHOLD_PERCENT);
    let dirs = list_model_dirs(seeding);
    let victims = plan_deletions(dirs, need, KEEP_NEWEST);
    let mut deleted = Vec::new();
    for path in victims {
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                info!("GC removed old model dir {}", path.display());
                deleted.push(path);
            }
            Err(e) => warn!("GC failed to remove {}: {e}", path.display()),
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(name: &str, age_secs: u64, size: u64) -> SeedEntry {
        SeedEntry {
            path: PathBuf::from(name),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(age_secs),
            size,
        }
    }

    #[test]
    fn no_delete_when_under_quota() {
        assert!(plan_deletions(vec![entry("a", 1, 100)], 0, 1).is_empty());
    }

    #[test]
    fn keeps_newest() {
        let dirs = vec![
            entry("old", 1, 50),
            entry("mid", 2, 50),
            entry("new", 3, 50),
        ];
        let gone = plan_deletions(dirs, 1000, 1);
        assert_eq!(gone, vec![PathBuf::from("old"), PathBuf::from("mid")]);
    }

    #[test]
    fn stops_once_enough_bytes_freed() {
        let dirs = vec![
            entry("old", 1, 80),
            entry("mid", 2, 80),
            entry("new", 3, 80),
        ];
        let gone = plan_deletions(dirs, 50, 1);
        assert_eq!(gone, vec![PathBuf::from("old")]);
    }

    #[test]
    fn bytes_to_free_from_percent() {
        assert_eq!(bytes_to_free(1000, 400, 50), 0);
        assert_eq!(bytes_to_free(1000, 850, 50), 350);
    }
}
