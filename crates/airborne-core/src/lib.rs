//! Provider-neutral domain types and lifecycle policy for Airborne.
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} contains a forbidden control character")]
    ControlCharacter { kind: &'static str },
    #[error("rule version must be at least 1")]
    InvalidRuleVersion,
    #[error("rule and rule definition must have matching ID, kind, and current version")]
    InvalidRuleDefinition,
    #[error("pull request number must be at least 1")]
    InvalidPullRequestNumber,
    #[error("timestamp must be UTC RFC 3339: {0}")]
    InvalidTimestamp(String),
    #[error("a candidate alert intent must use the candidate source identity")]
    AlertSourceMismatch,
    #[error("an alert intent requires a source identity")]
    MissingAlertSource,
    #[error("a multi-job source needs at least one job ID")]
    EmptyJobSet,
    #[error("a multi-job source cannot contain duplicate job IDs")]
    DuplicateJobId,
}

macro_rules! string_value {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(DomainError::Empty { kind: $label });
                }
                if value.chars().any(char::is_control) {
                    return Err(DomainError::ControlCharacter { kind: $label });
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_inner(self) -> String {
                self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = DomainError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl TryFrom<&str> for $name {
            type Error = DomainError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl FromStr for $name {
            type Err = DomainError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_value!(WatchId, "watch ID");
string_value!(RuleId, "rule ID");
string_value!(AlertId, "alert ID");
string_value!(PollAttemptId, "poll attempt ID");
string_value!(RefreshId, "refresh ID");
string_value!(SubjectKey, "subject key");
string_value!(Revision, "revision");
string_value!(SourceIdentity, "source identity");
string_value!(GitHubOwner, "GitHub owner");
string_value!(GitHubRepository, "GitHub repository");
string_value!(GitHubStatusContext, "GitHub status context");
string_value!(BuildkiteOrganization, "Buildkite organization");
string_value!(BuildkitePipeline, "Buildkite pipeline");
string_value!(BuildkiteBuildNumber, "Buildkite build number");
string_value!(BuildkiteJobName, "Buildkite job name");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuleVersion(u64);
impl RuleVersion {
    pub fn new(value: u64) -> Result<Self, DomainError> {
        if value == 0 {
            Err(DomainError::InvalidRuleVersion)
        } else {
            Ok(Self(value))
        }
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}
impl TryFrom<u64> for RuleVersion {
    type Error = DomainError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl fmt::Display for RuleVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitHubPullRequestNumber(u64);
impl GitHubPullRequestNumber {
    pub fn new(value: u64) -> Result<Self, DomainError> {
        if value == 0 {
            Err(DomainError::InvalidPullRequestNumber)
        } else {
            Ok(Self(value))
        }
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl TryFrom<u64> for GitHubPullRequestNumber {
    type Error = DomainError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl fmt::Display for GitHubPullRequestNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubRepositoryKey {
    pub host: String,
    pub owner: GitHubOwner,
    pub repository: GitHubRepository,
}
impl GitHubRepositoryKey {
    pub fn new(
        host: impl Into<String>,
        owner: GitHubOwner,
        repository: GitHubRepository,
    ) -> Result<Self, DomainError> {
        let host = host.into().to_ascii_lowercase();
        if host.trim().is_empty() {
            return Err(DomainError::Empty {
                kind: "GitHub host",
            });
        }
        if host.chars().any(char::is_control) {
            return Err(DomainError::ControlCharacter {
                kind: "GitHub host",
            });
        }
        Ok(Self {
            host,
            owner,
            repository,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubPullRequestKey {
    pub repository: GitHubRepositoryKey,
    pub number: GitHubPullRequestNumber,
}
impl GitHubPullRequestKey {
    pub fn new(repository: GitHubRepositoryKey, number: GitHubPullRequestNumber) -> Self {
        Self { repository, number }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BuildkiteBuildKey {
    pub organization: BuildkiteOrganization,
    pub pipeline: BuildkitePipeline,
    pub number: BuildkiteBuildNumber,
}
impl BuildkiteBuildKey {
    pub fn new(
        organization: BuildkiteOrganization,
        pipeline: BuildkitePipeline,
        number: BuildkiteBuildNumber,
    ) -> Self {
        Self {
            organization,
            pipeline,
            number,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(DateTime<Utc>);
impl Timestamp {
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        DateTime::parse_from_rfc3339(value)
            .map(|time| Self(time.with_timezone(&Utc)))
            .map_err(|_| DomainError::InvalidTimestamp(value.to_owned()))
    }
    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value)
    }
    pub fn as_datetime(&self) -> DateTime<Utc> {
        self.0
    }
}
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_rfc3339_opts(SecondsFormat::Secs, true))
    }
}
impl Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    GitHubPullRequest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub key: SubjectKey,
    pub kind: SubjectKind,
    pub canonical_url: String,
    pub display_title: String,
    pub current_revision: Option<Revision>,
    pub metadata_refreshed_at: Option<Timestamp>,
    pub created_at: Timestamp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchState {
    Active,
    Paused,
    Archived,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub id: WatchId,
    pub subject_key: SubjectKey,
    pub state: WatchState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub archived_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    GitHubCheckCompletes,
    BuildkiteJobCompletes,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: RuleId,
    pub watch_id: WatchId,
    pub kind: RuleKind,
    pub enabled: bool,
    pub current_version: RuleVersion,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub archived_at: Option<Timestamp>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildkiteNotifyOn {
    Terminal,
    Passed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleConfig {
    GitHubCheckCompletes {
        check_name: String,
    },
    BuildkiteJobCompletes {
        github_status_context: GitHubStatusContext,
        expected_organization: BuildkiteOrganization,
        expected_pipeline: BuildkitePipeline,
        job_name: BuildkiteJobName,
        notify_on: BuildkiteNotifyOn,
    },
}
impl RuleConfig {
    pub const fn kind(&self) -> RuleKind {
        match self {
            Self::GitHubCheckCompletes { .. } => RuleKind::GitHubCheckCompletes,
            Self::BuildkiteJobCompletes { .. } => RuleKind::BuildkiteJobCompletes,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDefinition {
    pub rule_id: RuleId,
    pub version: RuleVersion,
    pub config: RuleConfig,
    pub created_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RuleKey {
    pub rule_id: RuleId,
    pub rule_version: RuleVersion,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedRule {
    pub rule: Rule,
    pub definition: RuleDefinition,
}
impl VersionedRule {
    pub fn key(&self) -> RuleKey {
        RuleKey {
            rule_id: self.rule.id.clone(),
            rule_version: self.definition.version,
        }
    }
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.rule.id != self.definition.rule_id
            || self.rule.current_version != self.definition.version
            || self.rule.kind != self.definition.config.kind()
        {
            return Err(DomainError::InvalidRuleDefinition);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Waiting,
    InProgress,
    Completed,
    Failed,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertIntent {
    pub source_identity: SourceIdentity,
    pub title: String,
    pub body: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub rule_key: RuleKey,
    pub subject_key: SubjectKey,
    pub revision: Revision,
    pub state: CandidateState,
    pub source_identity: Option<SourceIdentity>,
    pub source_url: Option<String>,
    pub detail: Option<String>,
    pub alert_intent: Option<AlertIntent>,
    pub observed_at: Timestamp,
}
impl Candidate {
    pub fn validate(&self) -> Result<(), DomainError> {
        if let Some(intent) = &self.alert_intent {
            let Some(source) = &self.source_identity else {
                return Err(DomainError::MissingAlertSource);
            };
            if *source != intent.source_identity {
                return Err(DomainError::AlertSourceMismatch);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub rule_id: RuleId,
    pub rule_version: RuleVersion,
    pub revision: Revision,
    pub state: CandidateState,
    pub source_identity: Option<SourceIdentity>,
    pub source_url: Option<String>,
    pub detail: Option<String>,
    pub observed_at: Timestamp,
}
impl From<&Candidate> for Observation {
    fn from(candidate: &Candidate) -> Self {
        Self {
            rule_id: candidate.rule_key.rule_id.clone(),
            rule_version: candidate.rule_key.rule_version,
            revision: candidate.revision.clone(),
            state: candidate.state,
            source_identity: candidate.source_identity.clone(),
            source_url: candidate.source_url.clone(),
            detail: candidate.detail.clone(),
            observed_at: candidate.observed_at.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AlertKey {
    pub rule_id: RuleId,
    pub rule_version: RuleVersion,
    pub revision: Revision,
    pub source_identity: SourceIdentity,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertDraft {
    pub key: AlertKey,
    pub watch_id: WatchId,
    pub subject_key: SubjectKey,
    pub rule_kind: RuleKind,
    pub title: String,
    pub body: String,
    pub source_url: Option<String>,
    pub created_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    pub id: AlertId,
    pub key: AlertKey,
    pub watch_id: WatchId,
    pub subject_key: SubjectKey,
    pub rule_kind: RuleKind,
    pub title: String,
    pub body: String,
    pub source_url: Option<String>,
    pub created_at: Timestamp,
    pub acknowledged_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleHistory {
    pub watch_id: WatchId,
    pub rule_kind: RuleKind,
    pub latest_observation: Option<Observation>,
    pub existing_alert_keys: BTreeSet<AlertKey>,
}
impl RuleHistory {
    pub fn has_baseline(&self) -> bool {
        self.latest_observation.is_some()
    }
    pub fn contains(&self, key: &AlertKey) -> bool {
        self.existing_alert_keys.contains(key)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleHistorySet(pub BTreeMap<RuleKey, RuleHistory>);
impl RuleHistorySet {
    pub fn get(&self, key: &RuleKey) -> Option<&RuleHistory> {
        self.0.get(key)
    }
    pub fn insert(&mut self, key: RuleKey, history: RuleHistory) -> Option<RuleHistory> {
        self.0.insert(key, history)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciliation {
    pub observation: Observation,
    pub alert: Option<AlertDraft>,
}
/// Applies baseline, revision re-arm, and alert-key deduplication without I/O.
pub fn reconcile(
    candidate: Candidate,
    history: RuleHistory,
) -> Result<Reconciliation, DomainError> {
    candidate.validate()?;
    let observation = Observation::from(&candidate);
    if !history.has_baseline() {
        return Ok(Reconciliation {
            observation,
            alert: None,
        });
    }
    let alert = candidate.alert_intent.as_ref().and_then(|intent| {
        let key = AlertKey {
            rule_id: candidate.rule_key.rule_id.clone(),
            rule_version: candidate.rule_key.rule_version,
            revision: candidate.revision.clone(),
            source_identity: intent.source_identity.clone(),
        };
        (!history.contains(&key)).then(|| AlertDraft {
            key,
            watch_id: history.watch_id.clone(),
            subject_key: candidate.subject_key.clone(),
            rule_kind: history.rule_kind,
            title: intent.title.clone(),
            body: intent.body.clone(),
            source_url: candidate.source_url.clone(),
            created_at: candidate.observed_at.clone(),
        })
    });
    Ok(Reconciliation { observation, alert })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectMetadataUpdate {
    pub display_title: String,
    pub current_revision: Revision,
    pub metadata_refreshed_at: Timestamp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollOutcome {
    Success,
    Partial,
    Failure,
    Canceled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollAttempt {
    pub id: PollAttemptId,
    pub refresh_id: RefreshId,
    pub watch_id: WatchId,
    pub subject_key: SubjectKey,
    pub revision: Option<Revision>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub outcome: PollOutcome,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollAttemptDraft {
    pub refresh_id: RefreshId,
    pub watch_id: WatchId,
    pub subject_key: SubjectKey,
    pub revision: Option<Revision>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub outcome: PollOutcome,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceIssueScope {
    Subject,
    Rule,
    Source,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    GitHub,
    Buildkite,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIssueDraft {
    pub scope: SourceIssueScope,
    pub rule_id: Option<RuleId>,
    pub provider: Provider,
    pub kind: String,
    pub safe_message: String,
    pub retryable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIssue {
    pub poll_attempt_id: PollAttemptId,
    pub scope: SourceIssueScope,
    pub rule_id: Option<RuleId>,
    pub provider: Provider,
    pub kind: String,
    pub safe_message: String,
    pub retryable: bool,
}

/// The fixed v1 encoding for a Buildkite set: UTF-8 `airborne:buildkite-job-ids:v1\\0`,
/// then each sorted ID encoded as its byte length in decimal, `:`, the ID, and `\\n`.
pub fn multi_job_source_identity(
    job_ids: impl IntoIterator<Item = SourceIdentity>,
) -> Result<SourceIdentity, DomainError> {
    let mut ids: Vec<_> = job_ids.into_iter().collect();
    if ids.is_empty() {
        return Err(DomainError::EmptyJobSet);
    }
    ids.sort();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(DomainError::DuplicateJobId);
    }
    let mut hash = Sha256::new();
    hash.update(b"airborne:buildkite-job-ids:v1");
    hash.update([0]);
    for id in ids {
        hash.update(id.as_str().len().to_string().as_bytes());
        hash.update(b":");
        hash.update(id.as_str().as_bytes());
        hash.update(b"\n");
    }
    SourceIdentity::new(format!("buildkite-jobs:v1:{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ts() -> Timestamp {
        Timestamp::parse("2026-09-08T12:00:00Z").unwrap()
    }
    fn id(value: &str) -> SourceIdentity {
        SourceIdentity::new(value).unwrap()
    }
    fn candidate(revision: &str, source: Option<&str>, intent: bool) -> Candidate {
        let source_identity = source.map(id);
        Candidate {
            rule_key: RuleKey {
                rule_id: RuleId::new("rule").unwrap(),
                rule_version: RuleVersion::new(1).unwrap(),
            },
            subject_key: SubjectKey::new("github.com/o/r#1").unwrap(),
            revision: Revision::new(revision).unwrap(),
            state: CandidateState::Completed,
            source_identity: source_identity.clone(),
            source_url: Some("https://example.test/source".into()),
            detail: None,
            alert_intent: intent.then(|| AlertIntent {
                source_identity: source_identity.unwrap(),
                title: "done".into(),
                body: "finished".into(),
            }),
            observed_at: ts(),
        }
    }
    fn empty_history() -> RuleHistory {
        RuleHistory {
            watch_id: WatchId::new("watch").unwrap(),
            rule_kind: RuleKind::GitHubCheckCompletes,
            latest_observation: None,
            existing_alert_keys: BTreeSet::new(),
        }
    }
    fn history(candidate: &Candidate) -> RuleHistory {
        RuleHistory {
            latest_observation: Some(Observation::from(candidate)),
            ..empty_history()
        }
    }
    fn reconcile_test(candidate: Candidate, history: RuleHistory) -> Reconciliation {
        reconcile(candidate, history).unwrap()
    }
    #[test]
    fn checked_values_reject_empty_and_invalid_numbers() {
        assert!(WatchId::new(" ").is_err());
        assert!(Revision::new("x\ny").is_err());
        assert!(RuleVersion::new(0).is_err());
        assert!(GitHubPullRequestNumber::new(0).is_err());
    }
    #[test]
    fn timestamp_serializes_canonical_utc_rfc3339() {
        assert_eq!(
            serde_json::to_string(&Timestamp::parse("2026-09-08T08:00:00-04:00").unwrap()).unwrap(),
            "\"2026-09-08T12:00:00Z\""
        );
    }
    #[test]
    fn lifecycle_table() {
        let baseline = candidate("a", Some("1"), true);
        let cases = [
            (
                "first terminal is baseline",
                empty_history(),
                baseline.clone(),
                false,
            ),
            (
                "waiting to completion alerts",
                history(&candidate("a", None, false)),
                baseline.clone(),
                true,
            ),
            (
                "same source does not repeat",
                {
                    let mut h = history(&baseline);
                    h.existing_alert_keys.insert(AlertKey {
                        rule_id: RuleId::new("rule").unwrap(),
                        rule_version: RuleVersion::new(1).unwrap(),
                        revision: Revision::new("a").unwrap(),
                        source_identity: id("1"),
                    });
                    h
                },
                baseline.clone(),
                false,
            ),
            (
                "new revision terminal alerts",
                history(&baseline),
                candidate("b", Some("2"), true),
                true,
            ),
            (
                "rule change is a baseline",
                empty_history(),
                candidate("a", Some("1"), true),
                false,
            ),
            (
                "unavailable without intent does not alert",
                history(&baseline),
                candidate("a", None, false),
                false,
            ),
        ];
        for (name, history, candidate, expected) in cases {
            assert_eq!(
                reconcile_test(candidate, history).alert.is_some(),
                expected,
                "{name}"
            );
        }
    }
    #[test]
    fn every_candidate_replaces_observation() {
        let candidate = candidate("a", None, false);
        assert_eq!(
            reconcile_test(candidate.clone(), empty_history()).observation,
            Observation::from(&candidate)
        );
    }
    #[test]
    fn alert_intent_requires_matching_source() {
        let mut candidate = candidate("a", Some("one"), true);
        candidate.alert_intent.as_mut().unwrap().source_identity = id("two");
        assert_eq!(candidate.validate(), Err(DomainError::AlertSourceMismatch));
    }
    #[test]
    fn multi_job_identity_is_order_independent_and_versioned() {
        let forward = multi_job_source_identity([id("42"), id("7")]).unwrap();
        let reverse = multi_job_source_identity([id("7"), id("42")]).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(
            forward.as_str(),
            "buildkite-jobs:v1:8af4b6ce6fc0493fa4540cdb029e959b64c615ce122791eaf2f64cd0bf69da06"
        );
        assert_eq!(
            multi_job_source_identity([id("a"), id("10")])
                .unwrap()
                .as_str(),
            "buildkite-jobs:v1:929a8a533adeffea5fd27d035f2f10283950f719857a41ad337861b8b7ac2608"
        );
    }
    #[test]
    fn multi_job_identity_rejects_bad_sets() {
        assert_eq!(
            multi_job_source_identity(Vec::new()),
            Err(DomainError::EmptyJobSet)
        );
        assert_eq!(
            multi_job_source_identity(vec![id("7"), id("7")]),
            Err(DomainError::DuplicateJobId)
        );
    }
}
