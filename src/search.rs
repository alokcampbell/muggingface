use crate::hf::ModelHit;

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SearchSuggestion {
    pub author: String,
    pub repo_name: String,
    pub full_repo: String,
    pub source: &'static str,
}

impl SearchSuggestion {
    pub fn from_full_repo(full_repo: impl Into<String>, source: &'static str) -> Option<Self> {
        let full_repo = full_repo.into();
        let (author, repo_name) = crate::split_repo_id(&full_repo)?;
        Some(Self {
            full_repo: format!("{author}/{repo_name}"),
            author,
            repo_name,
            source,
        })
    }
}

/// Local torrents first, then Hugging Face hits. Dedupe by `author/repo`.
pub fn merge_suggestions(
    local: impl IntoIterator<Item = SearchSuggestion>,
    hf: impl IntoIterator<Item = ModelHit>,
    limit: usize,
) -> Vec<SearchSuggestion> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in local {
        if seen.insert(item.full_repo.clone()) {
            out.push(item);
        }
        if out.len() >= limit {
            return out;
        }
    }
    for hit in hf {
        if let Some(item) = SearchSuggestion::from_full_repo(hit.id, "hf") {
            if seen.insert(item.full_repo.clone()) {
                out.push(item);
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(id: &str) -> SearchSuggestion {
        SearchSuggestion::from_full_repo(id, "local").unwrap()
    }

    #[test]
    fn local_wins_over_duplicate_hf() {
        let merged = merge_suggestions(
            [local("openai-community/gpt2")],
            [
                ModelHit {
                    id: "openai-community/gpt2".into(),
                },
                ModelHit {
                    id: "sd2-community/stable-diffusion-2-1".into(),
                },
            ],
            10,
        );
        assert_eq!(
            merged
                .iter()
                .map(|s| (s.full_repo.as_str(), s.source))
                .collect::<Vec<_>>(),
            vec![
                ("openai-community/gpt2", "local"),
                ("sd2-community/stable-diffusion-2-1", "hf"),
            ]
        );
    }

    #[test]
    fn drops_unscoped_hf_ids() {
        let merged = merge_suggestions(
            [],
            [ModelHit {
                id: "gpt2".into(),
            }],
            10,
        );
        assert!(merged.is_empty());
    }

    #[test]
    fn respects_limit() {
        let local_items = [
            local("a/one"),
            local("a/two"),
            local("a/three"),
        ];
        let merged = merge_suggestions(
            local_items,
            [ModelHit {
                id: "b/four".into(),
            }],
            2,
        );
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|s| s.source == "local"));
    }
}
