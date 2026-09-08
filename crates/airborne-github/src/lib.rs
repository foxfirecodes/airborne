//! GitHub's narrow HTTP boundary for Airborne.
//!
//! This crate accepts only canonical public GitHub pull request identities and
//! always sends requests to `api.github.com` in production.

use std::{sync::Arc, time::Duration};

use airborne_core::{
    GitHubOwner, GitHubPullRequestKey, GitHubPullRequestNumber, GitHubRepository,
    GitHubRepositoryKey, Revision,
};
use async_trait::async_trait;
use reqwest::{header, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::time::sleep;
use url::Url;

const API_ROOT: &str = "https://api.github.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: usize = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 20;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestSnapshot {
    pub title: String,
    pub head_revision: Revision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckRun {
    pub id: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
    pub started_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitStatus {
    pub id: String,
    pub context: String,
    pub state: String,
    pub target_url: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GitHubError {
    #[error("GitHub request timed out")]
    Timeout,
    #[error("GitHub network request failed")]
    Network,
    #[error("GitHub authentication was rejected")]
    AuthenticationRejected,
    #[error("GitHub resource was not found")]
    NotFound,
    #[error("GitHub rate limit reached; retry after {retry_after_seconds} seconds")]
    RateLimited { retry_after_seconds: u64 },
    #[error("GitHub returned HTTP status {status}")]
    HttpStatus { status: u16 },
    #[error("GitHub returned an invalid response: {message}")]
    ProviderResponse { message: String },
    #[error("invalid GitHub pull request URL")]
    InvalidPullRequestUrl,
}

impl GitHubError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Network
                | Self::RateLimited { .. }
                | Self::HttpStatus { status: 502..=504 }
        )
    }
}

#[async_trait]
pub trait GitHubApi: Send + Sync {
    async fn pull_request(
        &self,
        key: &GitHubPullRequestKey,
    ) -> Result<PullRequestSnapshot, GitHubError>;
    async fn check_runs(
        &self,
        repository: &GitHubRepositoryKey,
        revision: &Revision,
    ) -> Result<Vec<CheckRun>, GitHubError>;
    async fn commit_statuses(
        &self,
        repository: &GitHubRepositoryKey,
        revision: &Revision,
    ) -> Result<Vec<CommitStatus>, GitHubError>;
}

#[derive(Clone)]
pub struct ReqwestGitHubApi {
    client: reqwest::Client,
    token: Arc<str>,
    api_root: Url,
}

impl ReqwestGitHubApi {
    /// Creates a production client that talks only to GitHub's public API.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be configured.
    pub fn new(token: impl Into<String>) -> Result<Self, GitHubError> {
        Self::with_api_root(token, API_ROOT)
    }

    /// Checks that the configured token can make a read-only GitHub request.
    ///
    /// This uses `GET /user`, GitHub's least-cost authenticated endpoint.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, authentication, rate-limit, or response error.
    pub async fn check_access(&self) -> Result<(), GitHubError> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let (_user, _) = self.get_json::<UserPayload>("/user").await?;
            Ok(())
        })
        .await
        .map_err(|_| GitHubError::Timeout)?
    }

    fn with_api_root(token: impl Into<String>, api_root: &str) -> Result<Self, GitHubError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("airborne/0.1")
            .build()
            .map_err(|_| GitHubError::Network)?;
        let api_root = Url::parse(api_root).map_err(|_| GitHubError::ProviderResponse {
            message: "invalid configured API endpoint".into(),
        })?;
        Ok(Self {
            client,
            token: Arc::from(token.into()),
            api_root,
        })
    }

    #[cfg(test)]
    fn for_test(token: impl Into<String>, api_root: &str) -> Result<Self, GitHubError> {
        Self::with_api_root(token, api_root)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<(T, Option<String>), GitHubError> {
        let operation = async {
            let mut last_error = GitHubError::Network;
            for attempt in 0..MAX_ATTEMPTS {
                match self.get_json_once(path).await {
                    Ok(value) => return Ok(value),
                    Err(error) if error.retryable() && attempt + 1 < MAX_ATTEMPTS => {
                        let delay = retry_delay(&error, attempt);
                        last_error = error;
                        sleep(delay).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error)
        };
        tokio::time::timeout(OPERATION_TIMEOUT, operation)
            .await
            .map_err(|_| GitHubError::Timeout)?
    }

    async fn get_json_once<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<(T, Option<String>), GitHubError> {
        let url = self
            .api_root
            .join(path)
            .map_err(|_| GitHubError::ProviderResponse {
                message: "could not construct API request".into(),
            })?;
        let response = self
            .client
            .get(url)
            .bearer_auth(self.token.as_ref())
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| map_transport_error(&error))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let rate_limit_remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok());
        let link = response
            .headers()
            .get(header::LINK)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            return Err(map_status(
                status,
                retry_after.as_deref(),
                rate_limit_remaining,
            ));
        }
        let bytes = read_limited_body(response).await?;
        let value =
            serde_json::from_slice(&bytes).map_err(|error| GitHubError::ProviderResponse {
                message: json_error(&error),
            })?;
        Ok((value, link))
    }
}

#[async_trait]
impl GitHubApi for ReqwestGitHubApi {
    async fn pull_request(
        &self,
        key: &GitHubPullRequestKey,
    ) -> Result<PullRequestSnapshot, GitHubError> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            validate_repository(&key.repository)?;
            let (payload, _) = self
                .get_json::<PullRequestPayload>(&format!(
                    "/repos/{}/{}/pulls/{}",
                    key.repository.owner.as_str(),
                    key.repository.repository.as_str(),
                    key.number.get()
                ))
                .await?;
            decode_pull_request(payload)
        })
        .await
        .map_err(|_| GitHubError::Timeout)?
    }

    async fn check_runs(
        &self,
        repository: &GitHubRepositoryKey,
        revision: &Revision,
    ) -> Result<Vec<CheckRun>, GitHubError> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            validate_repository(repository)?;
            let mut path = format!(
                "/repos/{}/{}/commits/{}/check-runs?per_page=100",
                repository.owner.as_str(),
                repository.repository.as_str(),
                revision
            );
            let mut output = Vec::new();
            for _ in 0..MAX_PAGES {
                let (payload, link) = self.get_json::<CheckRunsPayload>(&path).await?;
                output.extend(decode_check_runs(payload)?);
                let Some(next) = next_link(link.as_deref()) else {
                    return Ok(output);
                };
                path = self.next_page_path(&next)?;
            }
            Err(GitHubError::ProviderResponse {
                message: "pagination exceeded the page limit".into(),
            })
        })
        .await
        .map_err(|_| GitHubError::Timeout)?
    }

    async fn commit_statuses(
        &self,
        repository: &GitHubRepositoryKey,
        revision: &Revision,
    ) -> Result<Vec<CommitStatus>, GitHubError> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            validate_repository(repository)?;
            let mut path = format!(
                "/repos/{}/{}/commits/{}/status?per_page=100",
                repository.owner.as_str(),
                repository.repository.as_str(),
                revision
            );
            let mut output = Vec::new();
            for _ in 0..MAX_PAGES {
                let (payload, link) = self.get_json::<StatusesPayload>(&path).await?;
                output.extend(decode_statuses(payload)?);
                let Some(next) = next_link(link.as_deref()) else {
                    return Ok(output);
                };
                path = self.next_page_path(&next)?;
            }
            Err(GitHubError::ProviderResponse {
                message: "pagination exceeded the page limit".into(),
            })
        })
        .await
        .map_err(|_| GitHubError::Timeout)?
    }
}

impl ReqwestGitHubApi {
    fn next_page_path(&self, next: &str) -> Result<String, GitHubError> {
        let url = Url::parse(next).map_err(|_| GitHubError::ProviderResponse {
            message: "invalid pagination link".into(),
        })?;
        if url.scheme() != self.api_root.scheme()
            || url.host_str() != self.api_root.host_str()
            || url.port_or_known_default() != self.api_root.port_or_known_default()
        {
            return Err(GitHubError::ProviderResponse {
                message: "pagination link uses an unexpected host".into(),
            });
        }
        let mut path = url.path().to_owned();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        Ok(path)
    }
}

/// Parses exactly `https://github.com/<owner>/<repository>/pull/<positive-number>`.
///
/// # Errors
///
/// Returns [`GitHubError::InvalidPullRequestUrl`] unless the URL is canonical.
pub fn parse_pull_request_url(value: &str) -> Result<GitHubPullRequestKey, GitHubError> {
    let url = Url::parse(value).map_err(|_| GitHubError::InvalidPullRequestUrl)?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().contains('%')
    {
        return Err(GitHubError::InvalidPullRequestUrl);
    }
    let parts = url
        .path_segments()
        .ok_or(GitHubError::InvalidPullRequestUrl)?
        .collect::<Vec<_>>();
    if parts.len() != 4 || parts.iter().any(|part| part.is_empty()) || parts[2] != "pull" {
        return Err(GitHubError::InvalidPullRequestUrl);
    }
    let owner = GitHubOwner::new(parts[0]).map_err(|_| GitHubError::InvalidPullRequestUrl)?;
    let repository =
        GitHubRepository::new(parts[1]).map_err(|_| GitHubError::InvalidPullRequestUrl)?;
    if parts[3].starts_with('0') || !parts[3].bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GitHubError::InvalidPullRequestUrl);
    }
    let number = parts[3]
        .parse::<u64>()
        .map_err(|_| GitHubError::InvalidPullRequestUrl)?;
    let repository = GitHubRepositoryKey::new("github.com", owner, repository)
        .map_err(|_| GitHubError::InvalidPullRequestUrl)?;
    GitHubPullRequestNumber::new(number)
        .map(|number| GitHubPullRequestKey::new(repository, number))
        .map_err(|_| GitHubError::InvalidPullRequestUrl)
}

fn retry_delay(error: &GitHubError, attempt: usize) -> Duration {
    if let GitHubError::RateLimited {
        retry_after_seconds,
    } = error
    {
        if *retry_after_seconds > 0 {
            return Duration::from_secs(*retry_after_seconds).min(MAX_RETRY_AFTER);
        }
    }
    let base = 200 * (1_u64 << attempt);
    Duration::from_millis(base + rand::random_range(0..=100))
}
async fn read_limited_body(mut response: reqwest::Response) -> Result<Vec<u8>, GitHubError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(GitHubError::ProviderResponse {
            message: "response body exceeds the size limit".into(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_transport_error(&error))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(GitHubError::ProviderResponse {
                message: "response body exceeds the size limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
fn validate_repository(repository: &GitHubRepositoryKey) -> Result<(), GitHubError> {
    if repository.host == "github.com" {
        Ok(())
    } else {
        Err(GitHubError::InvalidPullRequestUrl)
    }
}
fn map_transport_error(error: &reqwest::Error) -> GitHubError {
    if error.is_timeout() {
        GitHubError::Timeout
    } else {
        GitHubError::Network
    }
}
fn map_status(
    status: StatusCode,
    retry_after: Option<&str>,
    rate_limit_remaining: Option<&str>,
) -> GitHubError {
    match status {
        StatusCode::FORBIDDEN if rate_limit_remaining == Some("0") || retry_after.is_some() => {
            GitHubError::RateLimited {
                retry_after_seconds: retry_after
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0)
                    .min(MAX_RETRY_AFTER.as_secs()),
            }
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => GitHubError::AuthenticationRejected,
        StatusCode::NOT_FOUND => GitHubError::NotFound,
        StatusCode::TOO_MANY_REQUESTS => GitHubError::RateLimited {
            retry_after_seconds: retry_after
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
                .min(MAX_RETRY_AFTER.as_secs()),
        },
        _ => GitHubError::HttpStatus {
            status: status.as_u16(),
        },
    }
}
fn json_error(error: &serde_json::Error) -> String {
    format!("JSON {:?}", error.classify())
}
fn next_link(header: Option<&str>) -> Option<String> {
    header?.split(',').find_map(|part| {
        let (target, rel) = part.trim().split_once(';')?;
        if rel.trim() != "rel=\"next\"" {
            return None;
        }
        target
            .trim()
            .strip_prefix('<')?
            .strip_suffix('>')
            .map(str::to_owned)
    })
}

#[derive(Deserialize)]
struct PullRequestPayload {
    title: String,
    head: HeadPayload,
}
#[derive(Deserialize)]
struct UserPayload {
    #[allow(dead_code)]
    login: String,
}
#[derive(Deserialize)]
struct HeadPayload {
    sha: String,
}
#[derive(Deserialize)]
struct CheckRunsPayload {
    check_runs: Vec<CheckRunPayload>,
}
#[derive(Deserialize)]
struct CheckRunPayload {
    id: serde_json::Value,
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    started_at: Option<String>,
}
#[derive(Deserialize)]
struct StatusesPayload {
    statuses: Vec<CommitStatusPayload>,
}
#[derive(Deserialize)]
struct CommitStatusPayload {
    id: serde_json::Value,
    context: String,
    state: String,
    target_url: Option<String>,
    created_at: Option<String>,
}
fn id(value: serde_json::Value) -> Result<String, GitHubError> {
    match value {
        serde_json::Value::String(value) if !value.is_empty() => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        _ => Err(GitHubError::ProviderResponse {
            message: "response item has no usable id".into(),
        }),
    }
}
fn decode_pull_request(value: PullRequestPayload) -> Result<PullRequestSnapshot, GitHubError> {
    if value.title.is_empty() {
        return Err(GitHubError::ProviderResponse {
            message: "pull request title is missing".into(),
        });
    }
    Ok(PullRequestSnapshot {
        title: value.title,
        head_revision: Revision::new(value.head.sha).map_err(|_| {
            GitHubError::ProviderResponse {
                message: "pull request head SHA is missing".into(),
            }
        })?,
    })
}
fn decode_check_runs(value: CheckRunsPayload) -> Result<Vec<CheckRun>, GitHubError> {
    value
        .check_runs
        .into_iter()
        .map(|item| {
            Ok(CheckRun {
                id: id(item.id)?,
                name: item.name,
                status: item.status,
                conclusion: item.conclusion,
                details_url: item.details_url,
                started_at: item.started_at,
            })
        })
        .collect()
}
fn decode_statuses(value: StatusesPayload) -> Result<Vec<CommitStatus>, GitHubError> {
    value
        .statuses
        .into_iter()
        .map(|item| {
            Ok(CommitStatus {
                id: id(item.id)?,
                context: item.context,
                state: item.state,
                target_url: item.target_url,
                created_at: item.created_at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const PR: &str = include_str!("../../../tests/fixtures/github/pull-request.json");
    const CHECKS: &str = include_str!("../../../tests/fixtures/github/check-runs-page-1.json");
    const STATUSES: &str = include_str!("../../../tests/fixtures/github/statuses-page-1.json");

    #[test]
    fn accepts_only_canonical_public_pr_urls() {
        for value in [
            "http://github.com/owner/repo/pull/1",
            "https://www.github.com/owner/repo/pull/1",
            "https://github.com/owner/repo/pulls/1",
            "https://github.com/owner/repo/pull/0",
            "https://github.com/owner/repo/pull/01",
            "https://github.com/owner/repo/pull/+1",
            "https://github.com/owner/repo/pull/1/",
            "https://github.com/owner/repo/pull/1?x=y",
            "https://github.com/owner/repo/pull/1#fragment",
        ] {
            assert_eq!(
                parse_pull_request_url(value),
                Err(GitHubError::InvalidPullRequestUrl)
            );
        }
        assert!(parse_pull_request_url("https://github.com/owner/repo/pull/1").is_ok());
    }

    #[test]
    fn captured_payloads_decode() {
        let pull = decode_pull_request(serde_json::from_str(PR).unwrap()).unwrap();
        assert_eq!(pull.title, "Example pull request");
        let checks = decode_check_runs(serde_json::from_str(CHECKS).unwrap()).unwrap();
        assert_eq!(checks[0].id, "101");
        let statuses = decode_statuses(serde_json::from_str(STATUSES).unwrap()).unwrap();
        assert_eq!(statuses[0].context, "buildkite/test");
    }

    #[test]
    fn missing_or_malformed_payloads_are_provider_errors() {
        assert!(serde_json::from_str::<PullRequestPayload>(include_str!(
            "../../../tests/fixtures/github/missing-head-sha.json"
        ))
        .is_err());
        assert!(serde_json::from_str::<CheckRunsPayload>(include_str!(
            "../../../tests/fixtures/github/malformed.json"
        ))
        .is_err());
    }

    #[test]
    fn status_errors_are_typed_and_redacted() {
        assert_eq!(
            map_status(StatusCode::UNAUTHORIZED, None, None),
            GitHubError::AuthenticationRejected
        );
        assert_eq!(
            map_status(StatusCode::TOO_MANY_REQUESTS, Some("120"), None),
            GitHubError::RateLimited {
                retry_after_seconds: 30
            }
        );
        let error = GitHubError::ProviderResponse {
            message: "JSON syntax".into(),
        };
        assert!(!error.to_string().contains("token"));
        for status in [502, 503, 504] {
            assert!(GitHubError::HttpStatus { status }.retryable());
        }
        assert!(!GitHubError::AuthenticationRejected.retryable());
        let fallback = retry_delay(
            &GitHubError::RateLimited {
                retry_after_seconds: 0,
            },
            0,
        );
        assert!((Duration::from_millis(200)..=Duration::from_millis(300)).contains(&fallback));
    }

    #[test]
    fn parses_next_page_links_only() {
        assert_eq!(next_link(Some("<https://api.github.com/a?page=2>; rel=\"next\", <https://api.github.com/a?page=3>; rel=\"last\"")), Some("https://api.github.com/a?page=2".into()));
        assert_eq!(
            next_link(Some("<https://api.github.com/a?page=3>; rel=\"last\"")),
            None
        );
    }

    #[test]
    fn rejects_pagination_host_confusion() {
        let client = ReqwestGitHubApi::for_test("not-a-secret", "https://api.github.com").unwrap();
        assert!(client
            .next_page_path("https://attacker.invalid/page")
            .is_err());
    }

    #[tokio::test]
    async fn concrete_client_decodes_paged_check_runs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (page, body) in [
                (
                    1,
                    include_str!("../../../tests/fixtures/github/check-runs-page-1.json"),
                ),
                (
                    2,
                    include_str!("../../../tests/fixtures/github/check-runs-page-2.json"),
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with(&format!(
                    "GET /repos/owner/repo/commits/revision/check-runs?per_page=100{} HTTP/1.1",
                    if page == 1 { "" } else { "&page=2" }
                )));
                assert!(
                    request.contains("authorization: Bearer fixture-token")
                        || request.contains("Authorization: Bearer fixture-token")
                );
                let link = if page == 1 {
                    format!("Link: <http://{address}/repos/owner/repo/commits/revision/check-runs?per_page=100&page=2>; rel=\"next\"\r\n")
                } else {
                    String::new()
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{link}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let api =
            ReqwestGitHubApi::for_test("fixture-token", &format!("http://{address}/")).unwrap();
        let repository = GitHubRepositoryKey::new(
            "github.com",
            GitHubOwner::new("owner").unwrap(),
            GitHubRepository::new("repo").unwrap(),
        )
        .unwrap();
        let result = api
            .check_runs(&repository, &Revision::new("revision").unwrap())
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["101", "102"]
        );
    }

    #[tokio::test]
    async fn concrete_client_does_not_follow_hostile_redirects() {
        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_address = api_listener.local_addr().unwrap();
        let hostile_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hostile_address = hostile_listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = api_listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{hostile_address}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let api =
            ReqwestGitHubApi::for_test("fixture-token", &format!("http://{api_address}/")).unwrap();
        let repository = GitHubRepositoryKey::new(
            "github.com",
            GitHubOwner::new("owner").unwrap(),
            GitHubRepository::new("repo").unwrap(),
        )
        .unwrap();
        let error = api
            .check_runs(&repository, &Revision::new("revision").unwrap())
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(error, GitHubError::HttpStatus { status: 302 });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), hostile_listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concrete_client_does_not_retry_authentication_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            assert!(String::from_utf8_lossy(&request).starts_with("GET /user HTTP/1.1"));
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let api =
            ReqwestGitHubApi::for_test("fixture-token", &format!("http://{address}/")).unwrap();
        assert_eq!(
            api.check_access().await.unwrap_err(),
            GitHubError::AuthenticationRejected
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn concrete_access_probe_uses_read_only_user_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("GET /user HTTP/1.1"));
            assert!(request.contains("Bearer fixture-token"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"login\":\"fixture\"}").await.unwrap();
        });
        let api =
            ReqwestGitHubApi::for_test("fixture-token", &format!("http://{address}/")).unwrap();
        api.check_access().await.unwrap();
        server.await.unwrap();
    }
}
