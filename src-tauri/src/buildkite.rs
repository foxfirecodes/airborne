use crate::rules::BuildkiteJob;
use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;
use url::Url;
#[derive(Clone)]
pub struct Buildkite {
    client: Client,
    token: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRef {
    pub organization: String,
    pub pipeline: String,
    pub number: i64,
}
#[derive(Debug, Clone)]
pub struct Build {
    pub jobs: Vec<BuildkiteJob>,
    pub finished: bool,
}
impl Buildkite {
    pub fn new(token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("PR Watcher v0")
                .build()
                .expect("HTTP client"),
            token,
        }
    }
    pub async fn build(&self, r: &BuildRef) -> Result<Build> {
        #[derive(Deserialize)]
        struct R {
            jobs: Vec<J>,
            finished: bool,
        }
        #[derive(Deserialize)]
        struct J {
            id: String,
            name: Option<String>,
            state: String,
            web_url: Option<String>,
        }
        let r: R = self
            .client
            .get(format!(
                "https://api.buildkite.com/v2/organizations/{}/pipelines/{}/builds/{}",
                r.organization, r.pipeline, r.number
            ))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Build {
            finished: r.finished,
            jobs: r
                .jobs
                .into_iter()
                .filter_map(|j| {
                    Some(BuildkiteJob {
                        id: j.id,
                        name: j.name?,
                        state: j.state,
                        web_url: j.web_url,
                    })
                })
                .collect(),
        })
    }
}
/// Strictly accepts only the public Buildkite build URL, never a redirect or arbitrary API URL.
pub fn parse_build_url(value: &str, organization: &str, pipeline: &str) -> Result<BuildRef> {
    let u = Url::parse(value)?;
    if u.scheme() != "https"
        || u.host_str() != Some("buildkite.com")
        || u.port().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("Buildkite target URL is not a supported build URL")
    }
    let parts: Vec<_> = u
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("bad URL"))?
        .collect();
    if parts.len() != 4 || parts[0] != organization || parts[1] != pipeline || parts[2] != "builds"
    {
        bail!("Buildkite target URL points to a different pipeline")
    }
    let number = parts[3].parse::<i64>()?;
    if value != format!("https://buildkite.com/{organization}/{pipeline}/builds/{number}") {
        bail!("Buildkite target URL is not an exact build URL")
    }
    Ok(BuildRef {
        organization: organization.into(),
        pipeline: pipeline.into(),
        number,
    })
}
