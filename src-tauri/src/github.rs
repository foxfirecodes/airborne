use crate::rules::CheckRun;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

#[derive(Clone)]
pub struct Github {
    client: Client,
    token: String,
}
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub title: String,
    pub head_sha: String,
}
#[derive(Debug, Clone)]
pub struct CommitStatus {
    pub context: String,
    pub target_url: Option<String>,
    pub created_at: Option<String>,
}
impl Github {
    pub fn new(token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("PR Watcher v0")
                .build()
                .expect("HTTP client"),
            token,
        }
    }
    fn request(&self, url: String) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
    }
    pub async fn pull_request(&self, owner: &str, repo: &str, number: i64) -> Result<PullRequest> {
        #[derive(Deserialize)]
        struct R {
            title: String,
            head: H,
        }
        #[derive(Deserialize)]
        struct H {
            sha: String,
        }
        let r: R = self
            .request(format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{number}"
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(PullRequest {
            title: r.title,
            head_sha: r.head.sha,
        })
    }
    pub async fn checks(&self, owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
        #[derive(Deserialize)]
        struct R {
            check_runs: Vec<C>,
        }
        #[derive(Deserialize)]
        struct C {
            id: u64,
            name: String,
            status: String,
            conclusion: Option<String>,
            details_url: Option<String>,
        }
        let r: R = self
            .request(format!(
                "https://api.github.com/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page=100"
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(r.check_runs
            .into_iter()
            .map(|c| CheckRun {
                id: c.id.to_string(),
                name: c.name,
                status: c.status,
                conclusion: c.conclusion,
                details_url: c.details_url,
            })
            .collect())
    }
    pub async fn statuses(&self, owner: &str, repo: &str, sha: &str) -> Result<Vec<CommitStatus>> {
        #[derive(Deserialize)]
        struct R {
            statuses: Vec<S>,
        }
        #[derive(Deserialize)]
        struct S {
            context: String,
            target_url: Option<String>,
            created_at: Option<String>,
        }
        let r: R = self
            .request(format!(
                "https://api.github.com/repos/{owner}/{repo}/commits/{sha}/status"
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(r.statuses
            .into_iter()
            .map(|s| CommitStatus {
                context: s.context,
                target_url: s.target_url,
                created_at: s.created_at,
            })
            .collect())
    }
}
pub fn parse_pr_url(value: &str) -> Result<(String, String, i64)> {
    let url = Url::parse(value).context("invalid GitHub PR URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        bail!("PR URL must be https://github.com/owner/repo/pull/number")
    }
    let p: Vec<_> = url
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("invalid PR URL"))?
        .collect();
    if p.len() != 4 || p[2] != "pull" {
        bail!("PR URL must be https://github.com/owner/repo/pull/number")
    }
    Ok((
        p[0].to_owned(),
        p[1].to_owned(),
        p[3].parse().context("PR number must be numeric")?,
    ))
}
