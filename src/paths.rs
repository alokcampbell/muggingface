use anyhow::Result;
use directories::BaseDirs;
use std::path::{Path, PathBuf};
use tracing::info;

pub fn seeding_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SEEDING_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    let base_dirs =
        BaseDirs::new().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(base_dirs.home_dir().join("seeding"))
}

pub fn ensure_server_directories() -> Result<bool> {
    let dir = seeding_dir()?;
    if dir.exists() {
        info!("Server directories already existed");
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).map_err(|e| {
        anyhow::anyhow!("Failed to create directory {}: {e}", dir.display())
    })?;
    info!("Server directories initialized successfully");
    Ok(true)
}

pub fn torrent_path(seeding: &Path, sha: &str) -> PathBuf {
    seeding.join(format!("{sha}.torrent"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_path_uses_sha() {
        assert_eq!(
            torrent_path(Path::new("/data"), "abc"),
            PathBuf::from("/data/abc.torrent")
        );
    }
}
