use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tracing::{error, info};

pub struct GitLfsCloner {
    seeding_dir: PathBuf,
    hf_token: Option<String>,
    progress_callback: Option<Box<dyn Fn(u64) + Send + Sync>>,
}

impl GitLfsCloner {
    pub fn new(seeding_dir: PathBuf, hf_token: Option<String>) -> Self {
        Self {
            seeding_dir,
            hf_token,
            progress_callback: None,
        }
    }

    pub fn with_progress_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        self.progress_callback = Some(Box::new(callback));
        self
    }

    pub fn target_dir(&self, full_repo: &str, sha: &str) -> PathBuf {
        self.seeding_dir
            .join(format!("{}-{}", full_repo.replace('/', "-"), sha))
    }

    pub async fn clone_repository(&self, full_repo: &str, sha: &str) -> Result<PathBuf> {
        let target_dir = self.target_dir(full_repo, sha);
        if self.is_repository_complete(&target_dir).await? {
            info!(
                "Repository {} already complete at {}",
                full_repo,
                target_dir.display()
            );
            return Ok(target_dir);
        }
        self.git_clone_with_lfs_skip(&target_dir, full_repo).await?;
        self.download_lfs_files(&target_dir, full_repo).await?;
        Ok(target_dir)
    }

    async fn is_repository_complete(&self, target_dir: &Path) -> Result<bool> {
        if !target_dir.exists() || !target_dir.join(".git").exists() {
            return Ok(false);
        }
        let lfs_files = self.get_lfs_files(target_dir).await?;
        for lfs_file in lfs_files {
            let file_path = target_dir.join(&lfs_file);
            if !file_path.exists() {
                return Ok(false);
            }
            if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                if content.starts_with("version https://git-lfs.github.com/spec/v1") {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    async fn git_clone_with_lfs_skip(&self, target_dir: &Path, full_repo: &str) -> Result<()> {
        if target_dir.exists() {
            info!("Repository exists, pulling latest changes");
            self.git_pull_with_lfs_skip(target_dir).await?;
            return Ok(());
        }
        let repo_url = self.build_repo_url(full_repo);
        info!("Cloning repository {} to {}", repo_url, target_dir.display());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            TokioCommand::new("git")
                .env("GIT_LFS_SKIP_SMUDGE", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["clone", &repo_url, &target_dir.to_string_lossy()])
                .output(),
        )
        .await??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Git clone failed: {stderr}"));
        }
        info!("Successfully cloned repository structure");
        Ok(())
    }

    async fn git_pull_with_lfs_skip(&self, target_dir: &Path) -> Result<()> {
        let output = TokioCommand::new("git")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .current_dir(target_dir)
            .args(["pull"])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            info!("Git pull failed (this might be normal): {stderr}");
        }
        Ok(())
    }

    pub async fn get_lfs_files(&self, target_dir: &Path) -> Result<Vec<String>> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            TokioCommand::new("git")
                .current_dir(target_dir)
                .args(["lfs", "ls-files"])
                .output(),
        )
        .await??;
        if !output.status.success() {
            info!("git lfs ls-files failed or repository doesn't use LFS");
            return Ok(Vec::new());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<String> = stdout
            .lines()
            .filter_map(|line| line.split_whitespace().nth(2).map(|s| s.to_string()))
            .collect();
        info!("Found {} LFS files", files.len());
        Ok(files)
    }

    async fn download_lfs_files(&self, target_dir: &Path, full_repo: &str) -> Result<()> {
        let lfs_files = self.get_lfs_files(target_dir).await?;
        if lfs_files.is_empty() {
            info!("No LFS files found in repository");
            return Ok(());
        }
        let client = reqwest::Client::builder()
            .user_agent("muggingface/1.0")
            .redirect(reqwest::redirect::Policy::limited(20))
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        for (i, lfs_file) in lfs_files.iter().enumerate() {
            let file_path = target_dir.join(lfs_file);
            info!(
                "Processing LFS file {}/{}: {}",
                i + 1,
                lfs_files.len(),
                lfs_file
            );
            if file_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                    if !content.starts_with("version https://git-lfs.github.com/spec/v1") {
                        info!("File {lfs_file} already downloaded, skipping");
                        continue;
                    }
                }
            }
            let mut attempts = 0;
            const MAX_ATTEMPTS: u32 = 3;
            while attempts < MAX_ATTEMPTS {
                attempts += 1;
                match tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    self.download_single_lfs_file(&client, target_dir, full_repo, lfs_file),
                )
                .await
                {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) => {
                        error!("Failed to download {lfs_file} (attempt {attempts}): {e}");
                        if attempts == MAX_ATTEMPTS {
                            return Err(anyhow::anyhow!(
                                "Failed to download {lfs_file} after {MAX_ATTEMPTS} attempts: {e}"
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    Err(_) => {
                        error!("Download timed out for {lfs_file} (attempt {attempts})");
                        if attempts == MAX_ATTEMPTS {
                            return Err(anyhow::anyhow!(
                                "Download timed out for {lfs_file} after {MAX_ATTEMPTS} attempts"
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn download_single_lfs_file(
        &self,
        client: &reqwest::Client,
        target_dir: &Path,
        full_repo: &str,
        lfs_file: &str,
    ) -> Result<()> {
        let download_url = format!("https://huggingface.co/{full_repo}/resolve/main/{lfs_file}");
        let file_path = target_dir.join(lfs_file);
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut request_builder = client.get(&download_url);
        if let Some(token) = &self.hf_token {
            request_builder = request_builder.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = request_builder.send().await?;
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to download {download_url}: HTTP {}",
                response.status()
            ));
        }
        let mut dest_file = File::create(&file_path).await?;
        let mut downloaded_bytes = 0u64;
        while let Some(chunk) = response.chunk().await? {
            dest_file.write_all(&chunk).await?;
            downloaded_bytes += chunk.len() as u64;
            if let Some(callback) = &self.progress_callback {
                callback(chunk.len() as u64);
            }
        }
        dest_file.flush().await?;
        info!("Successfully downloaded: {lfs_file} ({downloaded_bytes} bytes)");
        Ok(())
    }

    fn build_repo_url(&self, full_repo: &str) -> String {
        if let Some(token) = &self.hf_token {
            format!("https://user:{token}@huggingface.co/{full_repo}")
        } else {
            format!("https://huggingface.co/{full_repo}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dir_sanitizes_slash() {
        let cloner = GitLfsCloner::new(PathBuf::from("/data/seeding"), None);
        assert_eq!(
            cloner.target_dir("openai-community/gpt2", "abc123"),
            PathBuf::from("/data/seeding/openai-community-gpt2-abc123")
        );
    }
}
