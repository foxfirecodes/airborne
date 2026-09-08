//! Buildkite's narrow provider boundary.
//!
//! The client only calls Buildkite's fixed API origin. Public build links are
//! parsed as data; they are never used as request targets.

use async_trait::async_trait;
use rand::Rng;
use reqwest::{header, StatusCode};
use serde::Deserialize;
use std::{fmt, time::Duration};
use thiserror::Error;

pub const API_ORIGIN: &str = "https://api.buildkite.com/v2";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: usize = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY: usize = 512;
const MAX_RESPONSE_BODY: usize = 1_048_576;

/// A checked public Buildkite build link. This value is safe to use only as an
/// identity; callers must use [`BuildkiteApi`] to retrieve the build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicBuildUrl {
    pub organization: String,
    pub pipeline: String,
    pub number: u64,
}

impl PublicBuildUrl {
    /// Converts the parsed identity into the checked core key.
    ///
    /// # Errors
    ///
    /// Returns an error when the parsed segments violate core value rules.
    pub fn build_key(&self) -> Result<airborne_core::BuildkiteBuildKey, BuildkiteError> {
        let organization = airborne_core::BuildkiteOrganization::new(self.organization.clone())
            .map_err(|error| BuildkiteError::InvalidInput(error.to_string()))?;
        let pipeline = airborne_core::BuildkitePipeline::new(self.pipeline.clone())
            .map_err(|error| BuildkiteError::InvalidInput(error.to_string()))?;
        let number = airborne_core::BuildkiteBuildNumber::new(self.number.to_string())
            .map_err(|error| BuildkiteError::InvalidInput(error.to_string()))?;
        Ok(airborne_core::BuildkiteBuildKey::new(
            organization,
            pipeline,
            number,
        ))
    }
}

/// Rejects non-canonical public Buildkite links and links for another pipeline.
///
/// # Errors
///
/// Returns an error when the URL is malformed, non-canonical, or names a
/// different organization or pipeline.
pub fn parse_public_build_url(
    value: &str,
    expected_organization: &str,
    expected_pipeline: &str,
) -> Result<PublicBuildUrl, BuildkiteUrlError> {
    if expected_organization.is_empty() || expected_pipeline.is_empty() {
        return Err(BuildkiteUrlError::InvalidExpectedIdentity);
    }
    if !value.starts_with("https://buildkite.com/")
        || value.contains('?')
        || value.contains('#')
        || value.ends_with('/')
        || value.contains('@')
    {
        return Err(BuildkiteUrlError::NotCanonical);
    }
    let path = value
        .strip_prefix("https://buildkite.com/")
        .ok_or(BuildkiteUrlError::NotCanonical)?;
    let mut parts = path.split('/');
    let organization = parts.next().ok_or(BuildkiteUrlError::NotCanonical)?;
    let pipeline = parts.next().ok_or(BuildkiteUrlError::NotCanonical)?;
    let marker = parts.next().ok_or(BuildkiteUrlError::NotCanonical)?;
    let number = parts.next().ok_or(BuildkiteUrlError::NotCanonical)?;
    if parts.next().is_some()
        || organization.is_empty()
        || pipeline.is_empty()
        || marker != "builds"
    {
        return Err(BuildkiteUrlError::NotCanonical);
    }
    if organization != expected_organization || pipeline != expected_pipeline {
        return Err(BuildkiteUrlError::UnexpectedPipeline);
    }
    if !number.bytes().all(|byte| byte.is_ascii_digit()) || number.starts_with('0') {
        return Err(BuildkiteUrlError::NotCanonical);
    }
    let number = number
        .parse()
        .map_err(|_| BuildkiteUrlError::NotCanonical)?;
    Ok(PublicBuildUrl {
        organization: organization.to_owned(),
        pipeline: pipeline.to_owned(),
        number,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildState {
    Scheduled,
    Running,
    Passing,
    Failing,
    Passed,
    Failed,
    Blocked,
    Canceling,
    Canceled,
    Skipped,
    NotRun,
}

impl BuildState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Failed | Self::Canceled | Self::Skipped | Self::NotRun
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Scheduled,
    Waiting,
    Assigned,
    Accepted,
    Running,
    Finished,
    Passed,
    Failed,
    TimedOut,
    Canceled,
    Skipped,
    Broken,
    Expired,
    Blocked,
    Canceling,
}

impl JobState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Passed
                | Self::Failed
                | Self::TimedOut
                | Self::Canceled
                | Self::Skipped
                | Self::Broken
                | Self::Expired
        )
    }

    #[must_use]
    pub fn is_passed(self) -> bool {
        self == Self::Passed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildSnapshot {
    pub state: BuildState,
    pub finished_at: Option<String>,
    pub jobs: Vec<JobSnapshot>,
    pub web_url: Option<String>,
}

impl BuildSnapshot {
    /// Buildkite considers a build complete only when its known terminal state
    /// has a completion timestamp. No `finished` response field is used.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.is_terminal() && self.finished_at.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSnapshot {
    pub id: String,
    pub name: String,
    pub state: JobState,
    pub finished_at: Option<String>,
    pub web_url: Option<String>,
}

#[async_trait]
pub trait BuildkiteApi: Send + Sync {
    /// Fetches one build and its complete current `jobs` array.
    ///
    /// The Buildkite get-build endpoint embeds jobs in this response and has no
    /// job-page cursor, so this operation performs one fixed-host request.
    async fn build(
        &self,
        key: &airborne_core::BuildkiteBuildKey,
    ) -> Result<BuildSnapshot, BuildkiteError>;
}

#[derive(Debug, Error)]
pub enum BuildkiteUrlError {
    #[error("expected Buildkite organization and pipeline must not be empty")]
    InvalidExpectedIdentity,
    #[error("Buildkite URL is not canonical")]
    NotCanonical,
    #[error("Buildkite URL does not match the configured organization and pipeline")]
    UnexpectedPipeline,
}

#[derive(Debug, Error)]
pub enum BuildkiteError {
    #[error("Buildkite request input is invalid: {0}")]
    InvalidInput(String),
    #[error("Buildkite credentials were rejected")]
    CredentialRejected,
    #[error("Buildkite rate limited the request")]
    RateLimited(Option<Duration>),
    #[error("Buildkite did not find the requested build")]
    NotFound,
    #[error("Buildkite returned an unexpected status: {0}")]
    UnexpectedStatus(u16),
    #[error("Buildkite response is invalid: {0}")]
    ProviderResponse(String),
    #[error("Buildkite network request failed")]
    Network,
}

impl BuildkiteError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited(_) | Self::Network | Self::UnexpectedStatus(502..=504)
        )
    }
}

#[derive(Clone)]
pub struct ReqwestBuildkiteClient {
    client: reqwest::Client,
    token: String,
    origin: String,
    operation_timeout: Duration,
}

impl fmt::Debug for ReqwestBuildkiteClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestBuildkiteClient")
            .finish_non_exhaustive()
    }
}

impl ReqwestBuildkiteClient {
    /// Creates a client for Buildkite's fixed API origin.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty or the HTTP client cannot be
    /// constructed.
    pub fn new(token: impl Into<String>) -> Result<Self, BuildkiteError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(BuildkiteError::InvalidInput("missing token".to_owned()));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| BuildkiteError::Network)?;
        Ok(Self {
            client,
            token,
            origin: API_ORIGIN.to_owned(),
            operation_timeout: OPERATION_TIMEOUT,
        })
    }

    /// Checks whether the configured token can make a read-only Buildkite API
    /// request.
    ///
    /// This uses `GET /v2/user`, the least-cost authenticated endpoint, and
    /// never follows a provider-supplied URL.
    ///
    /// # Errors
    ///
    /// Returns a typed, redacted transport or credential error.
    pub async fn check_access(&self) -> Result<(), BuildkiteError> {
        let url = format!("{}/user", self.origin);
        tokio::time::timeout(self.operation_timeout, self.check_access_with_retries(&url))
            .await
            .map_err(|_| BuildkiteError::Network)?
    }

    fn build_url(&self, key: &airborne_core::BuildkiteBuildKey) -> String {
        format!(
            "{}/organizations/{}/pipelines/{}/builds/{}",
            self.origin,
            key.organization.as_str(),
            key.pipeline.as_str(),
            key.number.as_str()
        )
    }

    #[cfg(test)]
    fn with_test_origin(token: impl Into<String>, origin: String) -> Self {
        let mut client = Self::new(token).expect("test token must be accepted");
        client.origin = origin;
        client
    }

    #[cfg(test)]
    fn with_test_timeouts(
        token: impl Into<String>,
        origin: String,
        request_timeout: Duration,
        operation_timeout: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(request_timeout)
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test HTTP client must be constructed");
        Self {
            client,
            token: token.into(),
            origin,
            operation_timeout,
        }
    }

    async fn fetch(&self, url: &str) -> Result<BuildSnapshot, BuildkiteError> {
        tokio::time::timeout(self.operation_timeout, self.fetch_with_retries(url))
            .await
            .map_err(|_| BuildkiteError::Network)?
    }

    async fn fetch_with_retries(&self, url: &str) -> Result<BuildSnapshot, BuildkiteError> {
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let response = self.client.get(url).bearer_auth(&self.token).send().await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let body = bounded_body(response).await?;
                    return decode_build(&body);
                }
                Ok(response) => {
                    let error = response_error(response.status(), response.headers());
                    if !error.retryable() || attempt + 1 == MAX_ATTEMPTS {
                        return Err(error);
                    }
                    let wait = match error {
                        BuildkiteError::RateLimited(value) => {
                            value.unwrap_or_else(|| retry_delay(attempt))
                        }
                        _ => retry_delay(attempt),
                    };
                    last_error = Some(error);
                    tokio::time::sleep(wait).await;
                }
                Err(error)
                    if (error.is_connect() || error.is_timeout()) && attempt + 1 < MAX_ATTEMPTS =>
                {
                    last_error = Some(BuildkiteError::Network);
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Err(_) => return Err(BuildkiteError::Network),
            }
        }
        Err(last_error.unwrap_or(BuildkiteError::Network))
    }

    async fn check_access_with_retries(&self, url: &str) -> Result<(), BuildkiteError> {
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let response = self.client.get(url).bearer_auth(&self.token).send().await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let error = response_error(response.status(), response.headers());
                    if !error.retryable() || attempt + 1 == MAX_ATTEMPTS {
                        return Err(error);
                    }
                    let wait = match error {
                        BuildkiteError::RateLimited(value) => {
                            value.unwrap_or_else(|| retry_delay(attempt))
                        }
                        _ => retry_delay(attempt),
                    };
                    last_error = Some(error);
                    tokio::time::sleep(wait).await;
                }
                Err(error)
                    if (error.is_connect() || error.is_timeout()) && attempt + 1 < MAX_ATTEMPTS =>
                {
                    last_error = Some(BuildkiteError::Network);
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Err(_) => return Err(BuildkiteError::Network),
            }
        }
        Err(last_error.unwrap_or(BuildkiteError::Network))
    }
}

fn retry_delay(attempt: usize) -> Duration {
    let base = 250_u64.saturating_mul(1_u64 << attempt.min(3));
    let jitter = rand::rng().random_range(0..=100);
    Duration::from_millis(base.saturating_add(jitter))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, BuildkiteError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY as u64)
    {
        return Err(BuildkiteError::ProviderResponse(
            "response body exceeds size limit".to_owned(),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BuildkiteError::Network)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY {
            return Err(BuildkiteError::ProviderResponse(
                "response body exceeds size limit".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[async_trait]
impl BuildkiteApi for ReqwestBuildkiteClient {
    async fn build(
        &self,
        key: &airborne_core::BuildkiteBuildKey,
    ) -> Result<BuildSnapshot, BuildkiteError> {
        self.fetch(&self.build_url(key)).await
    }
}

#[derive(Deserialize)]
struct RawBuild {
    state: String,
    finished_at: Option<String>,
    jobs: Vec<RawJob>,
    web_url: Option<String>,
}

#[derive(Deserialize)]
struct RawJob {
    id: String,
    name: String,
    state: String,
    finished_at: Option<String>,
    web_url: Option<String>,
}

fn decode_build(body: &[u8]) -> Result<BuildSnapshot, BuildkiteError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| BuildkiteError::ProviderResponse(bound_error(&error.to_string())))?;
    let object = value
        .as_object()
        .ok_or_else(|| BuildkiteError::ProviderResponse("response is not an object".to_owned()))?;
    if !object.contains_key("finished_at") {
        return Err(BuildkiteError::ProviderResponse(
            "missing finished_at".to_owned(),
        ));
    }
    let raw: RawBuild = serde_json::from_value(value)
        .map_err(|error| BuildkiteError::ProviderResponse(bound_error(&error.to_string())))?;
    let state = parse_build_state(&raw.state)?;
    let jobs = raw
        .jobs
        .into_iter()
        .map(|job| {
            Ok(JobSnapshot {
                id: nonempty(job.id, "job id")?,
                name: nonempty(job.name, "job name")?,
                state: parse_job_state(&job.state)?,
                finished_at: job.finished_at,
                web_url: job.web_url,
            })
        })
        .collect::<Result<_, BuildkiteError>>()?;
    Ok(BuildSnapshot {
        state,
        finished_at: raw.finished_at,
        jobs,
        web_url: raw.web_url,
    })
}

fn nonempty(value: String, field: &str) -> Result<String, BuildkiteError> {
    (!value.trim().is_empty())
        .then_some(value)
        .ok_or_else(|| BuildkiteError::ProviderResponse(format!("missing {field}")))
}

fn parse_build_state(value: &str) -> Result<BuildState, BuildkiteError> {
    match value {
        "scheduled" => Ok(BuildState::Scheduled),
        "running" => Ok(BuildState::Running),
        "passing" => Ok(BuildState::Passing),
        "failing" => Ok(BuildState::Failing),
        "passed" => Ok(BuildState::Passed),
        "failed" => Ok(BuildState::Failed),
        "blocked" => Ok(BuildState::Blocked),
        "canceling" => Ok(BuildState::Canceling),
        "canceled" => Ok(BuildState::Canceled),
        "skipped" => Ok(BuildState::Skipped),
        "not_run" => Ok(BuildState::NotRun),
        _ => Err(unknown_state("build", value)),
    }
}

fn parse_job_state(value: &str) -> Result<JobState, BuildkiteError> {
    match value {
        "scheduled" => Ok(JobState::Scheduled),
        "waiting" => Ok(JobState::Waiting),
        "assigned" => Ok(JobState::Assigned),
        "accepted" => Ok(JobState::Accepted),
        "running" => Ok(JobState::Running),
        "finished" => Ok(JobState::Finished),
        "passed" => Ok(JobState::Passed),
        "failed" => Ok(JobState::Failed),
        "timed_out" => Ok(JobState::TimedOut),
        "canceled" => Ok(JobState::Canceled),
        "skipped" => Ok(JobState::Skipped),
        "broken" => Ok(JobState::Broken),
        "expired" => Ok(JobState::Expired),
        "blocked" => Ok(JobState::Blocked),
        "canceling" => Ok(JobState::Canceling),
        _ => Err(unknown_state("job", value)),
    }
}

fn unknown_state(kind: &str, value: &str) -> BuildkiteError {
    BuildkiteError::ProviderResponse(format!("unknown {kind} state: {}", bound_error(value)))
}

fn bound_error(value: &str) -> String {
    value.chars().take(MAX_ERROR_BODY).collect()
}

fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .map(|value| value.min(MAX_RETRY_AFTER))
}

fn response_error(status: StatusCode, headers: &header::HeaderMap) -> BuildkiteError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => BuildkiteError::CredentialRejected,
        StatusCode::NOT_FOUND => BuildkiteError::NotFound,
        StatusCode::TOO_MANY_REQUESTS => BuildkiteError::RateLimited(retry_after(headers)),
        _ => BuildkiteError::UnexpectedStatus(status.as_u16()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airborne_core::{
        BuildkiteBuildKey, BuildkiteBuildNumber, BuildkiteOrganization, BuildkitePipeline,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn fixture(name: &str) -> &'static [u8] {
        match name {
            "success" => include_bytes!("../../../tests/fixtures/buildkite/build-success.json"),
            "missing-state" => {
                include_bytes!("../../../tests/fixtures/buildkite/missing-state.json")
            }
            "missing-job-id" => {
                include_bytes!("../../../tests/fixtures/buildkite/missing-job-id.json")
            }
            "unknown-build" => {
                include_bytes!("../../../tests/fixtures/buildkite/unknown-build-state.json")
            }
            "unknown-job" => {
                include_bytes!("../../../tests/fixtures/buildkite/unknown-job-state.json")
            }
            _ => panic!("unknown fixture"),
        }
    }

    fn key() -> BuildkiteBuildKey {
        BuildkiteBuildKey::new(
            BuildkiteOrganization::new("acme").unwrap(),
            BuildkitePipeline::new("widgets").unwrap(),
            BuildkiteBuildNumber::new("42").unwrap(),
        )
    }

    #[test]
    fn strict_public_url_parser_checks_canonical_identity() {
        assert_eq!(
            parse_public_build_url(
                "https://buildkite.com/acme/widgets/builds/42",
                "acme",
                "widgets"
            )
            .unwrap()
            .number,
            42
        );
        for value in [
            "http://buildkite.com/acme/widgets/builds/42",
            "https://www.buildkite.com/acme/widgets/builds/42",
            "https://buildkite.com/acme/widgets/builds/42/",
            "https://buildkite.com/acme/widgets/builds/042",
            "https://buildkite.com/acme/widgets/builds/42?x=1",
            "https://buildkite.com/acme/widgets/builds/42#job",
            "https://buildkite.com/acme/widgets/builds/42/other",
        ] {
            assert!(
                parse_public_build_url(value, "acme", "widgets").is_err(),
                "{value}"
            );
        }
        assert!(matches!(
            parse_public_build_url(
                "https://buildkite.com/other/widgets/builds/42",
                "acme",
                "widgets"
            ),
            Err(BuildkiteUrlError::UnexpectedPipeline)
        ));
    }

    #[test]
    fn captured_shapes_decode_and_reject_missing_or_unknown_fields() {
        let snapshot = decode_build(fixture("success")).unwrap();
        assert_eq!(snapshot.state, BuildState::Passed);
        assert!(snapshot.is_finished());
        assert_eq!(snapshot.jobs[0].state, JobState::Passed);
        assert!(snapshot.jobs[0].state.is_terminal());
        assert!(matches!(
            decode_build(fixture("missing-state")),
            Err(BuildkiteError::ProviderResponse(_))
        ));
        assert!(matches!(
            decode_build(fixture("missing-job-id")),
            Err(BuildkiteError::ProviderResponse(_))
        ));
        assert!(matches!(
            decode_build(fixture("unknown-build")),
            Err(BuildkiteError::ProviderResponse(_))
        ));
        assert!(matches!(
            decode_build(fixture("unknown-job")),
            Err(BuildkiteError::ProviderResponse(_))
        ));
    }

    #[test]
    fn completion_uses_state_and_finished_at_not_a_finished_field() {
        let body =
            br#"{"state":"passed","finished":true,"finished_at":null,"jobs":[],"web_url":null}"#;
        assert!(!decode_build(body).unwrap().is_finished());
    }

    #[tokio::test]
    async fn http_contract_decodes_captured_success_without_using_public_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/organizations/acme/pipelines/widgets/builds/42"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(fixture("success"), "application/json"),
            )
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("secret-canary", server.uri());
        let snapshot = client.build(&key()).await.unwrap();
        assert!(snapshot.is_finished());
        assert_eq!(snapshot.jobs.len(), 2);
    }

    #[tokio::test]
    async fn http_errors_are_typed_and_redacted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("secret-canary"))
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("secret-canary", server.uri());
        let error = client.build(&key()).await.unwrap_err();
        assert!(matches!(error, BuildkiteError::CredentialRejected));
        assert!(!error.to_string().contains("secret-canary"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn access_probe_uses_fixed_user_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("token", server.uri());
        client.check_access().await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn access_probe_reports_rejected_credentials_without_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(401).set_body_string("secret-canary"))
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("secret-canary", server.uri());
        let error = client.check_access().await.unwrap_err();
        assert!(matches!(error, BuildkiteError::CredentialRejected));
        assert!(!error.to_string().contains("secret-canary"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn hostile_redirect_is_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/organizations/acme/pipelines/widgets/builds/42"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "https://example.invalid/private-build"),
            )
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("token", server.uri());
        let error = client.build(&key()).await.unwrap_err();
        assert!(matches!(error, BuildkiteError::UnexpectedStatus(302)));
    }

    #[tokio::test]
    async fn rate_limit_retries_and_remains_typed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("token", server.uri());
        let error = client.build(&key()).await.unwrap_err();
        assert!(matches!(error, BuildkiteError::RateLimited(Some(delay)) if delay.is_zero()));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            MAX_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn transient_gateway_errors_retry_three_times() {
        for status in [502, 503, 504] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let client = ReqwestBuildkiteClient::with_test_origin("token", server.uri());
            assert!(matches!(
                client.build(&key()).await,
                Err(BuildkiteError::UnexpectedStatus(actual)) if actual == status
            ));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                MAX_ATTEMPTS
            );
        }
    }

    #[tokio::test]
    async fn timed_out_operation_is_bounded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(50)))
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_timeouts(
            "token",
            server.uri(),
            Duration::from_millis(5),
            Duration::from_millis(40),
        );
        assert!(matches!(
            client.build(&key()).await,
            Err(BuildkiteError::Network)
        ));
    }

    #[tokio::test]
    async fn oversized_success_body_is_rejected_before_decoding() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE_BODY + 1)),
            )
            .mount(&server)
            .await;
        let client = ReqwestBuildkiteClient::with_test_origin("token", server.uri());
        assert!(matches!(
            client.build(&key()).await,
            Err(BuildkiteError::ProviderResponse(message)) if message == "response body exceeds size limit"
        ));
    }

    #[test]
    fn retry_after_is_bounded() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::RETRY_AFTER,
            header::HeaderValue::from_static("999999"),
        );
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_AFTER));
    }
}
