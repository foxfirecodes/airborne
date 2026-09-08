//! GitHub pull request monitoring built from provider ports.
//!
//! This crate owns the meaning of Airborne's two PR rule kinds.  It never uses
//! a provider-supplied URL as a request destination.

#![allow(
    clippy::blocks_in_conditions,
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::too_many_lines
)]

use std::sync::{Arc, OnceLock};

use airborne_buildkite::{
    parse_public_build_url, BuildSnapshot, BuildkiteApi, BuildkiteError, JobSnapshot,
};
use airborne_core::{
    multi_job_source_identity, AlertIntent, BuildkiteBuildKey, Candidate, CandidateState, Provider,
    RuleConfig, SourceIdentity, SourceIssueDraft, SourceIssueScope, SubjectKind,
    SubjectMetadataUpdate, VersionedRule,
};
use airborne_github::{
    parse_pull_request_url, CheckRun, CommitStatus, GitHubApi, GitHubError, PullRequestSnapshot,
};
use airborne_runtime::{MonitorReport, MonitorRequest, RulePollResult, SubjectMonitor};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use url::Url;

/// Total provider requests allowed across every PR monitor in this process.
pub const MAX_CONCURRENT_PROVIDER_REQUESTS: usize = 8;

fn provider_request_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PROVIDER_REQUESTS))))
}

/// The monitor for `SubjectKind::GitHubPullRequest`.
#[derive(Clone)]
pub struct GitHubPullRequestMonitor {
    github: Arc<dyn GitHubApi>,
    buildkite: Arc<dyn BuildkiteApi>,
}

impl GitHubPullRequestMonitor {
    pub fn new(github: Arc<dyn GitHubApi>, buildkite: Arc<dyn BuildkiteApi>) -> Self {
        Self { github, buildkite }
    }
}

#[async_trait]
impl SubjectMonitor for GitHubPullRequestMonitor {
    fn kind(&self) -> SubjectKind {
        SubjectKind::GitHubPullRequest
    }

    async fn poll(&self, request: MonitorRequest) -> MonitorReport {
        let mut report = empty_report(&request);
        if request.subject.kind != SubjectKind::GitHubPullRequest {
            report.subject_issue = Some(subject_issue(
                "invalid_subject",
                "subject is not a GitHub pull request",
                false,
            ));
            return report;
        }
        let key = match parse_pull_request_url(&request.subject.canonical_url) {
            Ok(key) => key,
            Err(_) => {
                report.subject_issue = Some(subject_issue(
                    "invalid_subject",
                    "subject has an invalid GitHub pull request URL",
                    false,
                ));
                return report;
            }
        };
        let snapshot = match {
            let _permit = provider_request_semaphore()
                .acquire_owned()
                .await
                .expect("global provider semaphore is never closed");
            self.github.pull_request(&key).await
        } {
            Ok(snapshot) => snapshot,
            Err(error) => {
                report.subject_issue = Some(github_subject_issue(&error));
                return report;
            }
        };
        report.revision = Some(snapshot.head_revision.clone());
        report.metadata = Some(SubjectMetadataUpdate {
            display_title: snapshot.title.clone(),
            current_revision: snapshot.head_revision.clone(),
            metadata_refreshed_at: request.observed_at.clone(),
        });

        let check_indexes = rule_indexes(&request.rules, |config| {
            matches!(config, RuleConfig::GitHubCheckCompletes { .. })
        });
        let buildkite_indexes = rule_indexes(&request.rules, |config| {
            matches!(config, RuleConfig::BuildkiteJobCompletes { .. })
        });
        let mut results: Vec<Option<RulePollResult>> =
            (0..request.rules.len()).map(|_| None).collect();

        if !check_indexes.is_empty() {
            match {
                let _permit = provider_request_semaphore()
                    .acquire_owned()
                    .await
                    .expect("global provider semaphore is never closed");
                self.github
                    .check_runs(&key.repository, &snapshot.head_revision)
                    .await
            } {
                Ok(checks) => {
                    for index in check_indexes {
                        results[index] = Some(check_candidate(&request, index, &checks, &snapshot));
                    }
                }
                Err(error) => fill_issues(
                    &mut results,
                    &request.rules,
                    check_indexes,
                    github_rule_issue(&error, "check_runs"),
                ),
            }
        }

        if !buildkite_indexes.is_empty() {
            match {
                let _permit = provider_request_semaphore()
                    .acquire_owned()
                    .await
                    .expect("global provider semaphore is never closed");
                self.github
                    .commit_statuses(&key.repository, &snapshot.head_revision)
                    .await
            } {
                Ok(statuses) => {
                    self.poll_buildkite_rules(
                        &request,
                        &snapshot,
                        &statuses,
                        buildkite_indexes,
                        &mut results,
                    )
                    .await
                }
                Err(error) => fill_issues(
                    &mut results,
                    &request.rules,
                    buildkite_indexes,
                    github_rule_issue(&error, "commit_statuses"),
                ),
            }
        }

        for (index, rule) in request.rules.iter().enumerate() {
            if results[index].is_none() {
                results[index] = Some(RulePollResult::Issue {
                    rule: rule.key(),
                    issue: rule_issue(
                        rule,
                        Provider::GitHub,
                        "invalid_rule",
                        "rule configuration does not match its kind",
                        false,
                    ),
                });
            }
        }
        report.results = results.into_iter().flatten().collect();
        report
    }
}

impl GitHubPullRequestMonitor {
    async fn poll_buildkite_rules(
        &self,
        request: &MonitorRequest,
        snapshot: &PullRequestSnapshot,
        statuses: &[CommitStatus],
        indexes: Vec<usize>,
        results: &mut [Option<RulePollResult>],
    ) {
        let mut groups: Vec<(BuildkiteBuildKey, Vec<usize>)> = Vec::new();
        for index in indexes {
            let rule = &request.rules[index];
            let RuleConfig::BuildkiteJobCompletes {
                github_status_context,
                expected_organization,
                expected_pipeline,
                ..
            } = &rule.definition.config
            else {
                continue;
            };
            let Some(status) = newest_status(statuses, github_status_context.as_str()) else {
                results[index] = Some(candidate_result(waiting_candidate(request, rule, snapshot)));
                continue;
            };
            let Some(target_url) = status.target_url.as_deref() else {
                results[index] = Some(issue_result(
                    rule,
                    Provider::Buildkite,
                    "buildkite_url",
                    "matching GitHub status has no Buildkite target URL",
                    false,
                ));
                continue;
            };
            let public = match parse_public_build_url(
                target_url,
                expected_organization.as_str(),
                expected_pipeline.as_str(),
            ) {
                Ok(value) => value,
                Err(_) => {
                    results[index] = Some(issue_result(rule, Provider::Buildkite, "buildkite_url", "matching GitHub status does not contain the configured canonical Buildkite build URL", false));
                    continue;
                }
            };
            let build = match public.build_key() {
                Ok(key) => key,
                Err(_) => {
                    results[index] = Some(issue_result(
                        rule,
                        Provider::Buildkite,
                        "buildkite_url",
                        "matching GitHub status contains an invalid Buildkite build identity",
                        false,
                    ));
                    continue;
                }
            };
            if let Some((_, members)) = groups.iter_mut().find(|(key, _)| *key == build) {
                members.push(index);
            } else {
                groups.push((build, vec![index]));
            }
        }
        for (build_key, members) in groups {
            match {
                let _permit = provider_request_semaphore()
                    .acquire_owned()
                    .await
                    .expect("global provider semaphore is never closed");
                self.buildkite.build(&build_key).await
            } {
                Ok(build) => {
                    for index in members {
                        results[index] = Some(build_candidate(
                            request,
                            &request.rules[index],
                            snapshot,
                            &build,
                            &build_key,
                        ));
                    }
                }
                Err(error) => fill_issues(
                    results,
                    &request.rules,
                    members,
                    buildkite_rule_issue(&error),
                ),
            }
        }
    }
}

fn empty_report(request: &MonitorRequest) -> MonitorReport {
    MonitorReport {
        subject_key: request.subject.key.clone(),
        metadata: None,
        revision: None,
        results: Vec::new(),
        subject_issue: None,
    }
}

fn rule_indexes(rules: &[VersionedRule], predicate: impl Fn(&RuleConfig) -> bool) -> Vec<usize> {
    rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| predicate(&rule.definition.config).then_some(index))
        .collect()
}

fn fill_issues(
    results: &mut [Option<RulePollResult>],
    rules: &[VersionedRule],
    indexes: Vec<usize>,
    issue: SourceIssueDraft,
) {
    for index in indexes {
        let mut issue = issue.clone();
        issue.rule_id = Some(rules[index].rule.id.clone());
        results[index] = Some(RulePollResult::Issue {
            rule: rules[index].key(),
            issue,
        });
    }
}

fn check_candidate(
    request: &MonitorRequest,
    index: usize,
    checks: &[CheckRun],
    snapshot: &PullRequestSnapshot,
) -> RulePollResult {
    let rule = &request.rules[index];
    let RuleConfig::GitHubCheckCompletes { check_name } = &rule.definition.config else {
        return issue_result(
            rule,
            Provider::GitHub,
            "invalid_rule",
            "rule is not a GitHub check rule",
            false,
        );
    };
    let Some(check) = newest_check(checks, check_name) else {
        return candidate_result(waiting_candidate(request, rule, snapshot));
    };
    let source = source(&check.id);
    let conclusion = check
        .conclusion
        .as_deref()
        .map(|value| format!("conclusion: {value}"));
    let completed = check.status == "completed";
    let alert_intent = completed.then(|| AlertIntent {
        source_identity: source.clone(),
        title: format!("{} completed", check.name),
        body: conclusion
            .clone()
            .unwrap_or_else(|| format!("{} completed", check.name)),
    });
    candidate_result(Candidate {
        rule_key: rule.key(),
        subject_key: request.subject.key.clone(),
        revision: snapshot.head_revision.clone(),
        state: if completed {
            CandidateState::Completed
        } else {
            CandidateState::InProgress
        },
        source_identity: Some(source),
        source_url: canonical_github_url(check.details_url.as_deref()),
        detail: conclusion,
        alert_intent,
        observed_at: request.observed_at.clone(),
    })
}

fn build_candidate(
    request: &MonitorRequest,
    rule: &VersionedRule,
    snapshot: &PullRequestSnapshot,
    build: &BuildSnapshot,
    build_key: &BuildkiteBuildKey,
) -> RulePollResult {
    let RuleConfig::BuildkiteJobCompletes {
        job_name,
        notify_on,
        ..
    } = &rule.definition.config
    else {
        return issue_result(
            rule,
            Provider::Buildkite,
            "invalid_rule",
            "rule is not a Buildkite job rule",
            false,
        );
    };
    let jobs: Vec<&JobSnapshot> = build
        .jobs
        .iter()
        .filter(|job| job.name == job_name.as_str())
        .collect();
    if jobs.is_empty() {
        let state = if build.is_finished() {
            CandidateState::Unavailable
        } else {
            CandidateState::Waiting
        };
        return candidate_result(base_candidate(
            request,
            rule,
            snapshot,
            state,
            None,
            build.web_url.clone(),
            Some(format!("no job named {job_name}")),
        ));
    }
    let identities = jobs.iter().map(|job| source(&job.id)).collect::<Vec<_>>();
    let source_identity = if identities.len() == 1 {
        identities[0].clone()
    } else {
        match multi_job_source_identity(identities) {
            Ok(identity) => identity,
            Err(_) => {
                return issue_result(
                    rule,
                    Provider::Buildkite,
                    "build_jobs",
                    "Buildkite returned duplicate or invalid job IDs",
                    false,
                )
            }
        }
    };
    let source_url = canonical_buildkite_build_url(build.web_url.as_deref(), build_key);
    let detail = Some(format!(
        "matched jobs: {}",
        jobs.iter()
            .map(|job| format!("{}={:?}", job.id, job.state))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if jobs.iter().any(|job| !job.state.is_terminal()) {
        return candidate_result(base_candidate(
            request,
            rule,
            snapshot,
            CandidateState::InProgress,
            Some(source_identity),
            source_url,
            detail,
        ));
    }
    let passed = jobs.iter().all(|job| job.state.is_passed());
    let state = if passed {
        CandidateState::Completed
    } else {
        CandidateState::Failed
    };
    let should_alert = matches!(notify_on, airborne_core::BuildkiteNotifyOn::Terminal) || passed;
    let alert_intent = should_alert.then(|| AlertIntent {
        source_identity: source_identity.clone(),
        title: format!(
            "Buildkite job {} {}",
            job_name,
            if passed { "passed" } else { "finished" }
        ),
        body: detail
            .clone()
            .unwrap_or_else(|| "Buildkite job finished".into()),
    });
    candidate_result(Candidate {
        alert_intent,
        ..base_candidate(
            request,
            rule,
            snapshot,
            state,
            Some(source_identity),
            source_url,
            detail,
        )
    })
}

fn waiting_candidate(
    request: &MonitorRequest,
    rule: &VersionedRule,
    snapshot: &PullRequestSnapshot,
) -> Candidate {
    base_candidate(
        request,
        rule,
        snapshot,
        CandidateState::Waiting,
        None,
        None,
        None,
    )
}
fn base_candidate(
    request: &MonitorRequest,
    rule: &VersionedRule,
    snapshot: &PullRequestSnapshot,
    state: CandidateState,
    source_identity: Option<SourceIdentity>,
    source_url: Option<String>,
    detail: Option<String>,
) -> Candidate {
    Candidate {
        rule_key: rule.key(),
        subject_key: request.subject.key.clone(),
        revision: snapshot.head_revision.clone(),
        state,
        source_identity,
        source_url,
        detail,
        alert_intent: None,
        observed_at: request.observed_at.clone(),
    }
}
fn candidate_result(candidate: Candidate) -> RulePollResult {
    RulePollResult::Candidate(candidate)
}
fn issue_result(
    rule: &VersionedRule,
    provider: Provider,
    kind: &str,
    message: &str,
    retryable: bool,
) -> RulePollResult {
    RulePollResult::Issue {
        rule: rule.key(),
        issue: rule_issue(rule, provider, kind, message, retryable),
    }
}
fn rule_issue(
    rule: &VersionedRule,
    provider: Provider,
    kind: &str,
    message: &str,
    retryable: bool,
) -> SourceIssueDraft {
    SourceIssueDraft {
        scope: SourceIssueScope::Source,
        rule_id: Some(rule.rule.id.clone()),
        provider,
        kind: kind.into(),
        safe_message: message.into(),
        retryable,
    }
}
fn subject_issue(kind: &str, message: &str, retryable: bool) -> SourceIssueDraft {
    SourceIssueDraft {
        scope: SourceIssueScope::Subject,
        rule_id: None,
        provider: Provider::GitHub,
        kind: kind.into(),
        safe_message: message.into(),
        retryable,
    }
}
fn github_subject_issue(error: &GitHubError) -> SourceIssueDraft {
    subject_issue("pull_request", &error.to_string(), error.retryable())
}
fn github_rule_issue(error: &GitHubError, kind: &str) -> SourceIssueDraft {
    SourceIssueDraft {
        scope: SourceIssueScope::Source,
        rule_id: None,
        provider: Provider::GitHub,
        kind: kind.into(),
        safe_message: error.to_string(),
        retryable: error.retryable(),
    }
}
fn buildkite_rule_issue(error: &BuildkiteError) -> SourceIssueDraft {
    SourceIssueDraft {
        scope: SourceIssueScope::Source,
        rule_id: None,
        provider: Provider::Buildkite,
        kind: "build".into(),
        safe_message: error.to_string(),
        retryable: error.retryable(),
    }
}
fn source(value: &str) -> SourceIdentity {
    SourceIdentity::new(value).expect("provider IDs must be nonempty")
}

fn newest_check<'a>(checks: &'a [CheckRun], name: &str) -> Option<&'a CheckRun> {
    checks
        .iter()
        .filter(|check| check.name == name)
        .max_by(|left, right| {
            compare_provider_items(
                left.started_at.as_deref(),
                &left.id,
                right.started_at.as_deref(),
                &right.id,
            )
        })
}
fn newest_status<'a>(statuses: &'a [CommitStatus], context: &str) -> Option<&'a CommitStatus> {
    statuses
        .iter()
        .filter(|status| status.context == context)
        .max_by(|left, right| {
            compare_provider_items(
                left.created_at.as_deref(),
                &left.id,
                right.created_at.as_deref(),
                &right.id,
            )
        })
}
fn compare_provider_items(
    left_time: Option<&str>,
    left_id: &str,
    right_time: Option<&str>,
    right_id: &str,
) -> std::cmp::Ordering {
    let parse = |value: Option<&str>| {
        value
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    };
    parse(left_time)
        .cmp(&parse(right_time))
        .then_with(|| numeric_id(left_id).cmp(&numeric_id(right_id)))
        .then_with(|| left_id.cmp(right_id))
}
fn numeric_id(value: &str) -> u128 {
    value.parse().unwrap_or(0)
}

fn canonical_github_url(value: Option<&str>) -> Option<String> {
    let url = Url::parse(value?).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !url.path().is_empty()
        && !url.path().contains('%'))
    .then(|| url.into())
}

fn canonical_buildkite_build_url(value: Option<&str>, key: &BuildkiteBuildKey) -> Option<String> {
    let value = value?;
    let parsed =
        parse_public_build_url(value, key.organization.as_str(), key.pipeline.as_str()).ok()?;
    (parsed.number.to_string() == key.number.as_str()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    use super::*;
    use airborne_buildkite::{BuildState, JobState};
    use airborne_core::{
        BuildkiteJobName, BuildkiteNotifyOn, BuildkiteOrganization, BuildkitePipeline,
        GitHubPullRequestKey, GitHubStatusContext, Revision, Rule, RuleDefinition, RuleId,
        RuleVersion, Subject, SubjectKey, Timestamp,
    };

    struct FakeGitHub {
        pr_calls: Arc<AtomicUsize>,
        check_calls: Arc<AtomicUsize>,
        status_calls: Arc<AtomicUsize>,
        checks: Result<Vec<CheckRun>, GitHubError>,
        statuses: Result<Vec<CommitStatus>, GitHubError>,
    }
    #[async_trait]
    impl GitHubApi for FakeGitHub {
        async fn pull_request(
            &self,
            _: &GitHubPullRequestKey,
        ) -> Result<PullRequestSnapshot, GitHubError> {
            self.pr_calls.fetch_add(1, Ordering::SeqCst);
            Ok(PullRequestSnapshot {
                title: "PR".into(),
                head_revision: Revision::new("a".to_owned()).unwrap(),
            })
        }
        async fn check_runs(
            &self,
            _: &airborne_core::GitHubRepositoryKey,
            _: &Revision,
        ) -> Result<Vec<CheckRun>, GitHubError> {
            self.check_calls.fetch_add(1, Ordering::SeqCst);
            clone_github_result(&self.checks)
        }
        async fn commit_statuses(
            &self,
            _: &airborne_core::GitHubRepositoryKey,
            _: &Revision,
        ) -> Result<Vec<CommitStatus>, GitHubError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            clone_github_result(&self.statuses)
        }
    }
    struct FakeBuildkite {
        calls: Arc<AtomicUsize>,
        build: Result<BuildSnapshot, String>,
    }

    struct BlockingGitHub {
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        gate: Arc<Semaphore>,
    }
    #[async_trait]
    impl GitHubApi for BlockingGitHub {
        async fn pull_request(
            &self,
            _: &GitHubPullRequestKey,
        ) -> Result<PullRequestSnapshot, GitHubError> {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(current, Ordering::SeqCst);
            let permit = self.gate.acquire().await.expect("test gate remains open");
            drop(permit);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(PullRequestSnapshot {
                title: "PR".into(),
                head_revision: Revision::new("a").unwrap(),
            })
        }
        async fn check_runs(
            &self,
            _: &airborne_core::GitHubRepositoryKey,
            _: &Revision,
        ) -> Result<Vec<CheckRun>, GitHubError> {
            Ok(vec![])
        }
        async fn commit_statuses(
            &self,
            _: &airborne_core::GitHubRepositoryKey,
            _: &Revision,
        ) -> Result<Vec<CommitStatus>, GitHubError> {
            Ok(vec![])
        }
    }
    #[async_trait]
    impl BuildkiteApi for FakeBuildkite {
        async fn build(&self, _: &BuildkiteBuildKey) -> Result<BuildSnapshot, BuildkiteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.build.clone().map_err(BuildkiteError::ProviderResponse)
        }
    }
    fn clone_github_result<T: Clone>(
        value: &Result<Vec<T>, GitHubError>,
    ) -> Result<Vec<T>, GitHubError> {
        value.clone().map_err(|error| match error {
            GitHubError::Network => GitHubError::Network,
            _ => GitHubError::ProviderResponse {
                message: "fake error".into(),
            },
        })
    }
    fn ts() -> Timestamp {
        Timestamp::parse("2026-09-08T12:00:00Z").unwrap()
    }
    fn subject() -> Subject {
        Subject {
            key: SubjectKey::new("github.com/o/r#1").unwrap(),
            kind: SubjectKind::GitHubPullRequest,
            canonical_url: "https://github.com/o/r/pull/1".into(),
            display_title: "old".into(),
            current_revision: None,
            metadata_refreshed_at: None,
            created_at: ts(),
        }
    }
    fn rule(id: &str, config: RuleConfig) -> VersionedRule {
        let kind = config.kind();
        VersionedRule {
            rule: Rule {
                id: RuleId::new(id).unwrap(),
                watch_id: airborne_core::WatchId::new("w").unwrap(),
                kind,
                enabled: true,
                current_version: RuleVersion::new(1).unwrap(),
                created_at: ts(),
                updated_at: ts(),
                archived_at: None,
            },
            definition: RuleDefinition {
                rule_id: RuleId::new(id).unwrap(),
                version: RuleVersion::new(1).unwrap(),
                config,
                created_at: ts(),
            },
        }
    }
    fn check_rule(id: &str) -> VersionedRule {
        rule(
            id,
            RuleConfig::GitHubCheckCompletes {
                check_name: "Cursor Bugbot".into(),
            },
        )
    }
    fn build_rule(id: &str, job: &str, notify_on: BuildkiteNotifyOn) -> VersionedRule {
        rule(
            id,
            RuleConfig::BuildkiteJobCompletes {
                github_status_context: GitHubStatusContext::new("buildkite/test").unwrap(),
                expected_organization: BuildkiteOrganization::new("org").unwrap(),
                expected_pipeline: BuildkitePipeline::new("pipe").unwrap(),
                job_name: BuildkiteJobName::new(job).unwrap(),
                notify_on,
            },
        )
    }
    fn request(rules: Vec<VersionedRule>) -> MonitorRequest {
        MonitorRequest {
            subject: subject(),
            rules,
            observed_at: ts(),
        }
    }
    fn status() -> CommitStatus {
        CommitStatus {
            id: "9".into(),
            context: "buildkite/test".into(),
            state: "success".into(),
            target_url: Some("https://buildkite.com/org/pipe/builds/12".into()),
            created_at: Some("2026-09-08T11:00:00Z".into()),
        }
    }
    fn monitor(github: FakeGitHub, buildkite: FakeBuildkite) -> GitHubPullRequestMonitor {
        GitHubPullRequestMonitor::new(Arc::new(github), Arc::new(buildkite))
    }
    fn empty_github() -> FakeGitHub {
        FakeGitHub {
            pr_calls: Arc::new(AtomicUsize::new(0)),
            check_calls: Arc::new(AtomicUsize::new(0)),
            status_calls: Arc::new(AtomicUsize::new(0)),
            checks: Ok(vec![]),
            statuses: Ok(vec![]),
        }
    }
    fn build(jobs: Vec<JobSnapshot>, state: BuildState, finished: bool) -> BuildSnapshot {
        BuildSnapshot {
            state,
            finished_at: finished.then(|| "2026-09-08T12:00:00Z".into()),
            jobs,
            web_url: Some("https://buildkite.com/org/pipe/builds/12".into()),
        }
    }
    fn job(id: &str, state: JobState) -> JobSnapshot {
        JobSnapshot {
            id: id.into(),
            name: "test".into(),
            state,
            finished_at: None,
            web_url: Some(format!("https://buildkite.com/jobs/{id}")),
        }
    }

    #[tokio::test]
    async fn fetch_plan_is_selective_and_builds_are_grouped() {
        let mut github = empty_github();
        github.statuses = Ok(vec![status()]);
        let pr_calls = github.pr_calls.clone();
        let check_calls = github.check_calls.clone();
        let status_calls = github.status_calls.clone();
        let buildkite = FakeBuildkite {
            calls: Arc::new(AtomicUsize::new(0)),
            build: Ok(build(
                vec![job("j", JobState::Passed)],
                BuildState::Passed,
                true,
            )),
        };
        let build_calls = buildkite.calls.clone();
        let report = monitor(github, buildkite)
            .poll(request(vec![
                check_rule("c"),
                build_rule("b1", "test", BuildkiteNotifyOn::Terminal),
                build_rule("b2", "test", BuildkiteNotifyOn::Passed),
            ]))
            .await;
        assert_eq!(report.results.len(), 3);
        assert!(report.subject_issue.is_none());
        assert_eq!(pr_calls.load(Ordering::SeqCst), 1);
        assert_eq!(check_calls.load(Ordering::SeqCst), 1);
        assert_eq!(status_calls.load(Ordering::SeqCst), 1);
        assert_eq!(build_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn check_selection_is_exact_newest_and_numeric_tie_breaks() {
        let mut github = empty_github();
        github.checks = Ok(vec![
            CheckRun {
                id: "2".into(),
                name: "Cursor Bugbot".into(),
                status: "queued".into(),
                conclusion: None,
                details_url: None,
                started_at: Some("2026-01-01T00:00:00Z".into()),
            },
            CheckRun {
                id: "10".into(),
                name: "Cursor Bugbot".into(),
                status: "completed".into(),
                conclusion: Some("neutral".into()),
                details_url: Some("https://x".into()),
                started_at: Some("2026-01-01T00:00:00Z".into()),
            },
            CheckRun {
                id: "99".into(),
                name: "cursor bugbot".into(),
                status: "completed".into(),
                conclusion: None,
                details_url: None,
                started_at: Some("2027-01-01T00:00:00Z".into()),
            },
        ]);
        let report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Err("unused".into()),
            },
        )
        .poll(request(vec![check_rule("c")]))
        .await;
        let RulePollResult::Candidate(candidate) = &report.results[0] else {
            panic!()
        };
        assert_eq!(candidate.state, CandidateState::Completed);
        assert_eq!(candidate.source_identity.as_ref().unwrap().as_str(), "10");
        assert!(candidate.alert_intent.is_some());
    }

    #[tokio::test]
    async fn buildkite_states_and_notify_policy_are_exact() {
        let cases = [
            (
                vec![],
                BuildState::Running,
                false,
                CandidateState::Waiting,
                false,
            ),
            (
                vec![],
                BuildState::Passed,
                true,
                CandidateState::Unavailable,
                false,
            ),
            (
                vec![job("j", JobState::Running)],
                BuildState::Running,
                false,
                CandidateState::InProgress,
                false,
            ),
            (
                vec![job("j", JobState::Passed)],
                BuildState::Passed,
                true,
                CandidateState::Completed,
                true,
            ),
            (
                vec![job("j", JobState::Failed)],
                BuildState::Failed,
                true,
                CandidateState::Failed,
                true,
            ),
        ];
        for (jobs, state, finished, expected, alert) in cases {
            let mut github = empty_github();
            github.statuses = Ok(vec![status()]);
            let report = monitor(
                github,
                FakeBuildkite {
                    calls: Arc::new(AtomicUsize::new(0)),
                    build: Ok(build(jobs, state, finished)),
                },
            )
            .poll(request(vec![build_rule(
                "b",
                "test",
                BuildkiteNotifyOn::Terminal,
            )]))
            .await;
            let RulePollResult::Candidate(candidate) = &report.results[0] else {
                panic!()
            };
            assert_eq!(candidate.state, expected);
            assert_eq!(candidate.alert_intent.is_some(), alert);
        }
        let mut github = empty_github();
        github.statuses = Ok(vec![status()]);
        let report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Ok(build(
                    vec![job("j", JobState::Failed)],
                    BuildState::Failed,
                    true,
                )),
            },
        )
        .poll(request(vec![build_rule(
            "b",
            "test",
            BuildkiteNotifyOn::Passed,
        )]))
        .await;
        let RulePollResult::Candidate(candidate) = &report.results[0] else {
            panic!()
        };
        assert!(candidate.alert_intent.is_none());
    }

    #[tokio::test]
    async fn source_failures_stay_scoped_and_input_order_is_preserved() {
        let mut github = empty_github();
        github.checks = Err(GitHubError::Network);
        github.statuses = Ok(vec![status()]);
        let report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Ok(build(
                    vec![job("b", JobState::Passed)],
                    BuildState::Passed,
                    true,
                )),
            },
        )
        .poll(request(vec![
            check_rule("check"),
            build_rule("build", "test", BuildkiteNotifyOn::Terminal),
        ]))
        .await;
        assert!(
            matches!(&report.results[0], RulePollResult::Issue { rule, .. } if rule.rule_id.as_str() == "check")
        );
        assert!(
            matches!(&report.results[1], RulePollResult::Candidate(candidate) if candidate.rule_key.rule_id.as_str() == "build")
        );
    }

    #[tokio::test]
    async fn invalid_status_url_is_scoped_and_never_fetches_buildkite() {
        let mut github = empty_github();
        github.statuses = Ok(vec![CommitStatus {
            target_url: Some("https://evil.example/org/pipe/builds/12".into()),
            ..status()
        }]);
        let buildkite = FakeBuildkite {
            calls: Arc::new(AtomicUsize::new(0)),
            build: Err("must not fetch".into()),
        };
        let build_calls = buildkite.calls.clone();
        let report = monitor(github, buildkite)
            .poll(request(vec![build_rule(
                "b",
                "test",
                BuildkiteNotifyOn::Terminal,
            )]))
            .await;
        assert!(matches!(report.results[0], RulePollResult::Issue { .. }));
        assert_eq!(build_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unsafe_provider_urls_are_omitted_from_candidates() {
        let mut github = empty_github();
        github.checks = Ok(vec![CheckRun {
            id: "check-id".into(),
            name: "Cursor Bugbot".into(),
            status: "completed".into(),
            conclusion: None,
            details_url: Some("https://github.com.evil.example/run".into()),
            started_at: None,
        }]);
        let check_report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Err("unused".into()),
            },
        )
        .poll(request(vec![check_rule("c")]))
        .await;
        let RulePollResult::Candidate(check) = &check_report.results[0] else {
            panic!()
        };
        assert!(check.source_url.is_none());

        let mut github = empty_github();
        github.statuses = Ok(vec![status()]);
        let mut unsafe_build = build(vec![job("job", JobState::Passed)], BuildState::Passed, true);
        unsafe_build.web_url =
            Some("https://buildkite.com/org/pipe/builds/12?redirect=https://evil.example".into());
        let build_report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Ok(unsafe_build),
            },
        )
        .poll(request(vec![build_rule(
            "b",
            "test",
            BuildkiteNotifyOn::Terminal,
        )]))
        .await;
        let RulePollResult::Candidate(build) = &build_report.results[0] else {
            panic!()
        };
        assert!(build.source_url.is_none());
    }

    #[tokio::test]
    async fn buildkite_failure_does_not_suppress_check_candidate() {
        let mut github = empty_github();
        github.checks = Ok(vec![CheckRun {
            id: "check-id".into(),
            name: "Cursor Bugbot".into(),
            status: "completed".into(),
            conclusion: None,
            details_url: None,
            started_at: None,
        }]);
        github.statuses = Ok(vec![status()]);
        let report = monitor(
            github,
            FakeBuildkite {
                calls: Arc::new(AtomicUsize::new(0)),
                build: Err("unavailable".into()),
            },
        )
        .poll(request(vec![
            check_rule("c"),
            build_rule("b", "test", BuildkiteNotifyOn::Terminal),
        ]))
        .await;
        assert!(
            matches!(&report.results[0], RulePollResult::Candidate(candidate) if candidate.state == CandidateState::Completed)
        );
        assert!(matches!(&report.results[1], RulePollResult::Issue { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn provider_requests_are_capped_across_monitor_instances() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let github: Arc<dyn GitHubApi> = Arc::new(BlockingGitHub {
            active: active.clone(),
            maximum: maximum.clone(),
            gate: gate.clone(),
        });
        let buildkite: Arc<dyn BuildkiteApi> = Arc::new(FakeBuildkite {
            calls: Arc::new(AtomicUsize::new(0)),
            build: Err("unused".into()),
        });
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let monitor = GitHubPullRequestMonitor::new(github.clone(), buildkite.clone());
            tasks.push(tokio::spawn(
                async move { monitor.poll(request(vec![])).await },
            ));
        }
        for _ in 0..100 {
            if active.load(Ordering::SeqCst) == MAX_CONCURRENT_PROVIDER_REQUESTS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(
            active.load(Ordering::SeqCst),
            MAX_CONCURRENT_PROVIDER_REQUESTS
        );
        assert_eq!(
            maximum.load(Ordering::SeqCst),
            MAX_CONCURRENT_PROVIDER_REQUESTS
        );
        gate.add_permits(12);
        for task in tasks {
            assert!(task.await.unwrap().subject_issue.is_none());
        }
        assert!(maximum.load(Ordering::SeqCst) <= MAX_CONCURRENT_PROVIDER_REQUESTS);
    }

    #[test]
    fn source_identity_uses_sorted_multi_job_hash() {
        let value = multi_job_source_identity([source("b"), source("a")]).unwrap();
        assert_eq!(
            value,
            multi_job_source_identity([source("a"), source("b")]).unwrap()
        );
    }
}
