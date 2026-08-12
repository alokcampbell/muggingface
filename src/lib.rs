pub mod db;
pub mod git_lfs;
pub mod hf;
pub mod magnet;
pub mod paths;
pub mod search;
pub mod state;
pub mod web;

/// Repos larger than this are not cloned; we show the donate page instead.
pub const MAX_REPO_SIZE_BYTES: u64 = 60 * 1024 * 1024 * 1024;

pub fn split_repo_id(full_repo: &str) -> Option<(String, String)> {
    let full_repo = full_repo.trim().trim_matches('/');
    if full_repo.is_empty() {
        return None;
    }
    match full_repo.split_once('/') {
        Some((user, repo)) if !user.is_empty() && !repo.is_empty() && !repo.contains('/') => {
            Some((user.to_string(), repo.to_string()))
        }
        Some((user, rest)) if !user.is_empty() && !rest.is_empty() => {
            let repo = rest.split('/').next().unwrap_or("").to_string();
            if repo.is_empty() {
                None
            } else {
                Some((user.to_string(), repo))
            }
        }
        None => None,
        _ => None,
    }
}

pub fn is_reserved_path(name: &str) -> bool {
    matches!(
        name,
        "search"
            | "about"
            | "healthz"
            | "static"
            | "torrent"
            | "favicon.ico"
            | "robots.txt"
    )
}

pub fn over_size_limit(used_storage: Option<u64>) -> bool {
    used_storage.is_some_and(|n| n >= MAX_REPO_SIZE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_scoped_repo() {
        assert_eq!(
            split_repo_id("openai-community/gpt2"),
            Some(("openai-community".into(), "gpt2".into()))
        );
    }

    #[test]
    fn split_trims_slashes() {
        assert_eq!(
            split_repo_id("/stabilityai/sdxl-turbo/"),
            Some(("stabilityai".into(), "sdxl-turbo".into()))
        );
    }

    #[test]
    fn split_rejects_unscoped() {
        assert_eq!(split_repo_id("gpt2"), None);
        assert_eq!(split_repo_id(""), None);
        assert_eq!(split_repo_id("   "), None);
    }

    #[test]
    fn split_uses_first_two_segments() {
        assert_eq!(
            split_repo_id("org/model/extra"),
            Some(("org".into(), "model".into()))
        );
    }

    #[test]
    fn reserved_paths() {
        assert!(is_reserved_path("search"));
        assert!(is_reserved_path("about"));
        assert!(!is_reserved_path("gpt2"));
        assert!(!is_reserved_path("openai-community"));
    }

    #[test]
    fn size_limit_uses_hf_used_storage() {
        assert!(!over_size_limit(None));
        assert!(!over_size_limit(Some(12 * 1024 * 1024 * 1024)));
        assert!(over_size_limit(Some(MAX_REPO_SIZE_BYTES)));
        assert!(over_size_limit(Some(77 * 1024 * 1024 * 1024)));
    }
}
