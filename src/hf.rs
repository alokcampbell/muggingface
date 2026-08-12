use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tracing::info;

const USER_AGENT: &str = "muggingface/1.0 (+https://muggingface.co)";

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub id: String,
    pub sha: String,
    pub siblings: Vec<String>,
    pub used_storage: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelHit {
    pub id: String,
}

#[derive(Clone)]
pub struct HfClient {
    http: Client,
    base: String,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelApiResponse {
    id: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    sha: Option<String>,
    siblings: Option<Vec<Sibling>>,
    #[serde(rename = "usedStorage")]
    used_storage: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Sibling {
    rfilename: String,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    id: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

impl HfClient {
    pub fn new(token: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: "https://huggingface.co".to_string(),
            token,
        })
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_string();
        self
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.header("Authorization", format!("Bearer {token}")),
            None => req,
        }
    }

    pub async fn repo_info(&self, repo: &str) -> Result<Option<RepoInfo>> {
        let repo = repo.trim().trim_matches('/');
        if repo.is_empty() {
            return Ok(None);
        }
        let url = format!("{}/api/models/{}", self.base, repo);
        let response = self.apply_auth(self.http.get(&url)).send().await?;
        match response.status() {
            StatusCode::OK => {
                let body: ModelApiResponse = response.json().await?;
                let id = body
                    .id
                    .or(body.model_id)
                    .ok_or_else(|| anyhow!("HF model response missing id"))?;
                let sha = body
                    .sha
                    .ok_or_else(|| anyhow!("HF model response missing sha"))?;
                let siblings: Vec<String> = body
                    .siblings
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| s.rfilename)
                    .collect();
                info!(
                    "HF repo {} sha={} files={} used_storage={:?}",
                    id,
                    &sha[..sha.len().min(12)],
                    siblings.len(),
                    body.used_storage
                );
                Ok(Some(RepoInfo {
                    id,
                    sha,
                    siblings,
                    used_storage: body.used_storage,
                }))
            }
            StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Ok(None),
            status => {
                let text = response.text().await.unwrap_or_default();
                Err(anyhow!("HF API {status} for {repo}: {text}"))
            }
        }
    }

    pub async fn search_models(&self, query: &str, limit: usize) -> Result<Vec<ModelHit>> {
        let limit = limit.clamp(1, 20);
        let mut req = self
            .http
            .get(format!("{}/api/models", self.base))
            .query(&[("limit", limit.to_string())]);
        let q = query.trim();
        if q.is_empty() {
            req = req.query(&[("sort", "downloads"), ("direction", "-1")]);
        } else {
            req = req.query(&[("search", q)]);
        }
        let response = self.apply_auth(req).send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("HF search {status}: {text}"));
        }
        let items: Vec<SearchItem> = response.json().await?;
        Ok(items
            .into_iter()
            .filter_map(|item| item.id.or(item.model_id))
            .filter(|id| id.contains('/'))
            .map(|id| ModelHit { id })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gpt2_body() -> serde_json::Value {
        json!({
            "id": "openai-community/gpt2",
            "modelId": "openai-community/gpt2",
            "sha": "607a30d783dfa663caf39e06633721c8d4cfcd7e",
            "usedStorage": 11977009063u64,
            "siblings": [
                {"rfilename": "config.json"},
                {"rfilename": "model.safetensors"}
            ]
        })
    }

    async fn client(server: &MockServer, token: Option<&str>) -> HfClient {
        HfClient::new(token.map(|s| s.to_string()))
            .unwrap()
            .with_base(server.uri())
    }

    #[tokio::test]
    async fn repo_info_ok_and_canonical_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/gpt2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gpt2_body()))
            .mount(&server)
            .await;
        let info = client(&server, None)
            .await
            .repo_info("gpt2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.id, "openai-community/gpt2");
        assert_eq!(info.sha, "607a30d783dfa663caf39e06633721c8d4cfcd7e");
        assert_eq!(info.siblings.len(), 2);
        assert_eq!(info.used_storage, Some(11977009063));
    }

    #[tokio::test]
    async fn repo_info_sends_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/openai-community/gpt2"))
            .and(header("Authorization", "Bearer hf_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gpt2_body()))
            .mount(&server)
            .await;
        let info = client(&server, Some("hf_test_token"))
            .await
            .repo_info("openai-community/gpt2")
            .await
            .unwrap();
        assert!(info.is_some());
    }

    #[tokio::test]
    async fn repo_info_404_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/stabilityai/stable-diffusion-2-1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error":"Repository not found"})))
            .mount(&server)
            .await;
        let info = client(&server, Some("hf_test_token"))
            .await
            .repo_info("stabilityai/stable-diffusion-2-1")
            .await
            .unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn search_uses_search_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models"))
            .and(query_param("search", "stable-diffusion-2-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "sd2-community/stable-diffusion-2-1", "modelId": "sd2-community/stable-diffusion-2-1"},
                {"id": "orphan-model"}
            ])))
            .mount(&server)
            .await;
        let hits = client(&server, None)
            .await
            .search_models("stable-diffusion-2-1", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sd2-community/stable-diffusion-2-1");
    }

    #[tokio::test]
    async fn empty_search_sorts_by_downloads() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models"))
            .and(query_param("sort", "downloads"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "openai-community/gpt2"}
            ])))
            .mount(&server)
            .await;
        let hits = client(&server, None).await.search_models("", 8).await.unwrap();
        assert_eq!(hits[0].id, "openai-community/gpt2");
    }
}
