//! Application lifecycle ports and orchestration for Airborne refreshes.
//!
//! Concrete monitors and stores live outside this crate. Keeping their boundary
//! here makes refresh policy testable without provider clients or `SQLite`.

use std::{
    fmt::Debug,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use airborne_core::{
    reconcile, Alert, AlertDraft, Candidate, Observation, PollAttemptDraft, PollOutcome, RefreshId,
    Revision, RuleHistorySet, RuleId, RuleKey, RuleKind, SourceIssueDraft, Subject, SubjectKey,
    SubjectMetadataUpdate, Timestamp, VersionedRule, Watch, WatchId,
};
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use thiserror::Error;

pub const MAX_CONCURRENT_SUBJECTS: usize = 4;
pub const LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefreshScope {
    AllActive,
    Watch(WatchId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshTarget {
    pub watch: Watch,
    pub subject: Subject,
    pub rules: Vec<VersionedRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorRequest {
    pub subject: Subject,
    pub rules: Vec<VersionedRule>,
    pub observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorReport {
    pub subject_key: SubjectKey,
    pub metadata: Option<SubjectMetadataUpdate>,
    pub revision: Option<airborne_core::Revision>,
    pub results: Vec<RulePollResult>,
    pub subject_issue: Option<SourceIssueDraft>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RulePollResult {
    Candidate(Candidate),
    Issue {
        rule: RuleKey,
        issue: SourceIssueDraft,
    },
}

#[async_trait]
pub trait SubjectMonitor: Send + Sync {
    fn kind(&self) -> airborne_core::SubjectKind;

    async fn poll(&self, request: MonitorRequest) -> MonitorReport;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollCommit {
    pub attempt: PollAttemptDraft,
    pub observations: Vec<Observation>,
    pub alerts: Vec<AlertDraft>,
    pub issues: Vec<SourceIssueDraft>,
    pub metadata: Option<SubjectMetadataUpdate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedPoll {
    pub new_alerts: Vec<airborne_core::Alert>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreError {
    #[error("{message}")]
    Failed { message: String },
    #[error(
        "watch {watch_id} already has an active {kind:?} rule ({existing_rule_id}); archive it before adding another"
    )]
    RuleKindConflict {
        watch_id: WatchId,
        kind: RuleKind,
        existing_rule_id: RuleId,
    },
}

#[async_trait]
pub trait RuntimeStore: Send + Sync {
    async fn load_refresh_targets(
        &self,
        scope: RefreshScope,
    ) -> Result<Vec<RefreshTarget>, StoreError>;

    async fn load_rule_history(
        &self,
        candidates: &[(RuleKey, Revision)],
    ) -> Result<RuleHistorySet, StoreError>;

    /// Applies all durable effects of one subject poll in one transaction.
    async fn apply_poll(&self, commit: PollCommit) -> Result<AppliedPoll, StoreError>;
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LeaseError {
    #[error("another {kind} is already active")]
    Busy { kind: &'static str },
    #[error("{message}")]
    Failed { message: String },
}

#[async_trait]
pub trait LeaseGuard: Send + Sync + Debug {
    async fn renew(&self) -> Result<(), LeaseError>;

    async fn release(&self) -> Result<(), LeaseError>;
}

#[derive(Clone, Debug)]
pub struct RefreshLease(Arc<dyn LeaseGuard>);

impl RefreshLease {
    pub fn new(guard: Arc<dyn LeaseGuard>) -> Self {
        Self(guard)
    }

    /// Releases this process's refresh lease.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage adapter cannot release its lease.
    pub async fn release(&self) -> Result<(), LeaseError> {
        self.0.release().await
    }

    /// Extends this process's refresh lease.
    ///
    /// # Errors
    ///
    /// Returns an error when the adapter cannot verify or extend the lease.
    pub async fn renew(&self) -> Result<(), LeaseError> {
        self.0.renew().await
    }
}

#[derive(Clone, Debug)]
pub struct RunnerLease(Arc<dyn LeaseGuard>);

impl RunnerLease {
    pub fn new(guard: Arc<dyn LeaseGuard>) -> Self {
        Self(guard)
    }

    /// Releases this process's runner lease.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage adapter cannot release its lease.
    pub async fn release(&self) -> Result<(), LeaseError> {
        self.0.release().await
    }

    /// Extends this process's runner lease.
    ///
    /// # Errors
    ///
    /// Returns an error when the adapter cannot verify or extend the lease.
    pub async fn renew(&self) -> Result<(), LeaseError> {
        self.0.renew().await
    }
}

#[async_trait]
pub trait LeaseStore: Send + Sync {
    async fn acquire_refresh(&self, wait: Duration) -> Result<RefreshLease, LeaseError>;

    async fn acquire_runner(&self) -> Result<RunnerLease, LeaseError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[async_trait]
pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;

    async fn cancelled(&self);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

#[async_trait]
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }

    async fn cancelled(&self) {
        std::future::pending::<()>().await;
    }
}

struct LeaseAwareCancellation<'a> {
    requested: &'a dyn Cancellation,
    lease_lost: Arc<std::sync::atomic::AtomicBool>,
    lost_notification: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Cancellation for LeaseAwareCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.requested.is_cancelled() || self.lease_lost.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        tokio::select! {
            () = self.requested.cancelled() => {},
            () = self.lost_notification.notified() => {},
        }
    }
}

#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Returns `true` after the duration elapses and `false` when cancelled.
    async fn sleep(&self, duration: Duration, cancellation: &dyn Cancellation) -> bool;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubjectRefreshReport {
    pub watch_id: WatchId,
    pub subject_key: SubjectKey,
    pub subject_title: String,
    pub outcome: PollOutcome,
    pub observations: Vec<Observation>,
    pub issues: Vec<SourceIssueDraft>,
    pub new_alerts: Vec<Alert>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshReport {
    pub refresh_id: RefreshId,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub outcome: PollOutcome,
    pub subjects: Vec<SubjectRefreshReport>,
}

impl RefreshReport {
    pub fn new_alerts(&self) -> impl Iterator<Item = &Alert> {
        self.subjects
            .iter()
            .flat_map(|subject| subject.new_alerts.iter())
    }

    pub fn issues(&self) -> impl Iterator<Item = &SourceIssueDraft> {
        self.subjects
            .iter()
            .flat_map(|subject| subject.issues.iter())
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("storage failed: {0}")]
    Store(#[from] StoreError),
    #[error("lease failed: {0}")]
    Lease(#[from] LeaseError),
    #[error("lifecycle reconciliation failed: {0}")]
    Domain(#[from] airborne_core::DomainError),
    #[error("no monitor handles {0:?}")]
    MissingMonitor(airborne_core::SubjectKind),
    #[error("monitor returned a report for a different subject")]
    WrongSubject,
    #[error("monitor returned a result for a rule that was not requested")]
    UnexpectedRule,
    #[error("monitor report did not contain exactly one result for every requested rule")]
    IncompleteMonitorReport,
    #[error("store did not return history for {0:?}")]
    MissingHistory(RuleKey),
    #[error("a subject task stopped unexpectedly")]
    SubjectTaskStopped,
}

/// Refresh application service. It owns scheduling and lifecycle policy, while
/// monitors and stores remain replaceable adapters.
#[derive(Clone)]
pub struct Runtime {
    store: Arc<dyn RuntimeStore>,
    leases: Arc<dyn LeaseStore>,
    clock: Arc<dyn Clock>,
    monitors: Vec<Arc<dyn SubjectMonitor>>,
    next_refresh: Arc<AtomicU64>,
}

impl Runtime {
    pub fn new(
        store: Arc<dyn RuntimeStore>,
        leases: Arc<dyn LeaseStore>,
        clock: Arc<dyn Clock>,
        monitors: Vec<Arc<dyn SubjectMonitor>>,
    ) -> Self {
        Self {
            store,
            leases,
            clock,
            monitors,
            next_refresh: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Runs one refresh while holding the refresh lease.
    ///
    /// # Errors
    ///
    /// Returns an error for lease, store, monitor-contract, or lifecycle failures.
    pub async fn refresh(
        &self,
        scope: RefreshScope,
        wait: Duration,
        cancellation: &dyn Cancellation,
    ) -> Result<RefreshReport, RuntimeError> {
        let lease = self.leases.acquire_refresh(wait).await?;
        let lease_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lost_notification = Arc::new(tokio::sync::Notify::new());
        let combined = LeaseAwareCancellation {
            requested: cancellation,
            lease_lost: lease_lost.clone(),
            lost_notification: lost_notification.clone(),
        };
        let refresh = self.refresh_held(scope, &combined);
        tokio::pin!(refresh);
        let mut renewal = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
        let result = loop {
            tokio::select! {
                result = &mut refresh => break result,
                _ = renewal.tick() => match lease.renew().await {
                    Ok(()) => {},
                    Err(error) => {
                        lease_lost.store(true, Ordering::Release);
                        lost_notification.notify_waiters();
                        let _ = refresh.await;
                        break Err(error.into());
                    }
                }
            }
        };
        let release = lease.release().await;
        match (result, release) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error.into()),
        }
    }

    async fn refresh_held(
        &self,
        scope: RefreshScope,
        cancellation: &dyn Cancellation,
    ) -> Result<RefreshReport, RuntimeError> {
        let started_at = self.clock.now();
        let sequence = self.next_refresh.fetch_add(1, Ordering::Relaxed);
        let refresh_id = RefreshId::new(format!("refresh-{started_at}-{sequence}"))?;
        let mut targets = self.store.load_refresh_targets(scope).await?;
        targets.sort_by(|left, right| left.watch.id.cmp(&right.watch.id));

        let runtime = self.clone();
        let mut reports = stream::iter(targets.into_iter().map(|target| {
            let runtime = runtime.clone();
            let refresh_id = refresh_id.clone();
            async move {
                if cancellation.is_cancelled() {
                    Ok(canceled_subject(&target))
                } else {
                    runtime.poll_subject(target, refresh_id, cancellation).await
                }
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_SUBJECTS)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
        reports.sort_by(|left, right| left.watch_id.cmp(&right.watch_id));
        let outcome = refresh_outcome(&reports);
        Ok(RefreshReport {
            refresh_id,
            started_at,
            finished_at: self.clock.now(),
            outcome,
            subjects: reports,
        })
    }

    #[allow(clippy::too_many_lines)] // This is the one-subject transaction boundary.
    async fn poll_subject(
        &self,
        target: RefreshTarget,
        refresh_id: RefreshId,
        cancellation: &dyn Cancellation,
    ) -> Result<SubjectRefreshReport, RuntimeError> {
        let started_at = self.clock.now();
        let monitor = self
            .monitors
            .iter()
            .find(|monitor| monitor.kind() == target.subject.kind)
            .ok_or(RuntimeError::MissingMonitor(target.subject.kind))?;
        let poll = monitor.poll(MonitorRequest {
            subject: target.subject.clone(),
            rules: target.rules.clone(),
            observed_at: started_at.clone(),
        });
        tokio::pin!(poll);
        let report = tokio::select! {
            report = &mut poll => report,
            () = cancellation.cancelled() => return Ok(canceled_subject(&target)),
        };
        if report.subject_key != target.subject.key {
            return Err(RuntimeError::WrongSubject);
        }
        if cancellation.is_cancelled() {
            return Ok(canceled_subject(&target));
        }

        let requested: std::collections::BTreeSet<_> =
            target.rules.iter().map(VersionedRule::key).collect();
        let result_keys: Vec<_> = report
            .results
            .iter()
            .map(|result| match result {
                RulePollResult::Candidate(candidate) => candidate.rule_key.clone(),
                RulePollResult::Issue { rule, .. } => rule.clone(),
            })
            .collect();
        if result_keys.iter().any(|key| !requested.contains(key)) {
            return Err(RuntimeError::UnexpectedRule);
        }
        if report.subject_issue.is_some() && !result_keys.is_empty() {
            return Err(RuntimeError::IncompleteMonitorReport);
        }
        if report.subject_issue.is_none()
            && (result_keys.len() != requested.len()
                || result_keys
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    != result_keys.len())
        {
            return Err(RuntimeError::IncompleteMonitorReport);
        }
        let candidate_revisions: Vec<_> = report
            .results
            .iter()
            .filter_map(|result| match result {
                RulePollResult::Candidate(candidate) => {
                    Some((candidate.rule_key.clone(), candidate.revision.clone()))
                }
                RulePollResult::Issue { .. } => None,
            })
            .collect();
        let histories = self.store.load_rule_history(&candidate_revisions).await?;

        let mut observations = Vec::new();
        let mut alerts = Vec::new();
        let mut issues = report.subject_issue.into_iter().collect::<Vec<_>>();
        for result in report.results {
            match result {
                RulePollResult::Candidate(candidate) => {
                    let history = histories
                        .get(&candidate.rule_key)
                        .cloned()
                        .ok_or_else(|| RuntimeError::MissingHistory(candidate.rule_key.clone()))?;
                    let reconciliation = reconcile(candidate, history)?;
                    observations.push(reconciliation.observation);
                    if let Some(alert) = reconciliation.alert {
                        alerts.push(alert);
                    }
                }
                RulePollResult::Issue { rule, issue } => {
                    if !requested.contains(&rule) {
                        return Err(RuntimeError::UnexpectedRule);
                    }
                    issues.push(issue);
                }
            }
        }
        observations.sort_by(|left, right| {
            left.rule_id
                .cmp(&right.rule_id)
                .then(left.rule_version.cmp(&right.rule_version))
        });
        issues.sort_by(|left, right| {
            left.rule_id
                .cmp(&right.rule_id)
                .then(left.kind.cmp(&right.kind))
        });
        let outcome = if issues.is_empty() {
            PollOutcome::Success
        } else if observations.is_empty() {
            PollOutcome::Failure
        } else {
            PollOutcome::Partial
        };
        let commit = PollCommit {
            attempt: PollAttemptDraft {
                refresh_id,
                watch_id: target.watch.id.clone(),
                subject_key: target.subject.key.clone(),
                revision: report.revision,
                started_at,
                finished_at: self.clock.now(),
                outcome,
            },
            observations: observations.clone(),
            alerts,
            issues: issues.clone(),
            metadata: report.metadata,
        };
        if cancellation.is_cancelled() {
            return Ok(canceled_subject(&target));
        }
        let applied = self.store.apply_poll(commit).await?;
        Ok(SubjectRefreshReport {
            watch_id: target.watch.id,
            subject_key: target.subject.key,
            subject_title: target.subject.display_title,
            outcome,
            observations,
            issues,
            new_alerts: applied.new_alerts,
        })
    }

    /// Runs immediate refreshes followed by cancellable sleeps.
    ///
    /// # Errors
    ///
    /// Returns an error when acquiring or releasing a lease, refreshing, or committing fails.
    pub async fn run(
        &self,
        interval: Duration,
        sleeper: &dyn Sleeper,
        cancellation: &dyn Cancellation,
        on_report: &mut dyn FnMut(&RefreshReport),
    ) -> Result<(), RuntimeError> {
        let lease = self.leases.acquire_runner().await?;
        let mut result = Ok(());
        while !cancellation.is_cancelled() {
            if let Err(error) = lease.renew().await {
                result = Err(error.into());
                break;
            }
            match self
                .refresh(RefreshScope::AllActive, Duration::ZERO, cancellation)
                .await
            {
                Ok(report) => on_report(&report),
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
            match sleep_while_renewing(&lease, sleeper, interval, cancellation).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    result = Err(error.into());
                    break;
                }
            }
        }
        let release = lease.release().await;
        result.and(release.map_err(RuntimeError::from))
    }
}

async fn sleep_while_renewing(
    lease: &RunnerLease,
    sleeper: &dyn Sleeper,
    duration: Duration,
    cancellation: &dyn Cancellation,
) -> Result<bool, LeaseError> {
    let sleep = sleeper.sleep(duration, cancellation);
    tokio::pin!(sleep);
    let mut renewal = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
    loop {
        tokio::select! {
            complete = &mut sleep => return Ok(complete),
            _ = renewal.tick() => lease.renew().await?,
        }
    }
}

fn canceled_subject(target: &RefreshTarget) -> SubjectRefreshReport {
    SubjectRefreshReport {
        watch_id: target.watch.id.clone(),
        subject_key: target.subject.key.clone(),
        subject_title: target.subject.display_title.clone(),
        outcome: PollOutcome::Canceled,
        observations: Vec::new(),
        issues: Vec::new(),
        new_alerts: Vec::new(),
    }
}

fn refresh_outcome(reports: &[SubjectRefreshReport]) -> PollOutcome {
    if reports
        .iter()
        .any(|report| report.outcome == PollOutcome::Canceled)
    {
        PollOutcome::Canceled
    } else if reports
        .iter()
        .all(|report| report.outcome == PollOutcome::Success)
    {
        PollOutcome::Success
    } else if reports.iter().any(|report| !report.observations.is_empty()) {
        PollOutcome::Partial
    } else {
        PollOutcome::Failure
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet, VecDeque},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use airborne_core::{
        AlertEventKind, AlertId, AlertIntent, CandidateState, Rule, RuleConfig, RuleDefinition,
        RuleHistory, RuleKind, RuleVersion, SourceIdentity, WatchState,
    };
    use tokio::sync::Mutex;

    fn timestamp() -> Timestamp {
        Timestamp::parse("2026-09-08T12:00:00Z").unwrap()
    }
    fn target(id: &str) -> RefreshTarget {
        let at = timestamp();
        let watch_id = WatchId::new(id).unwrap();
        let subject_key = SubjectKey::new(format!("github.com/acme/repo#{id}")).unwrap();
        let subject = Subject {
            key: subject_key.clone(),
            kind: airborne_core::SubjectKind::GitHubPullRequest,
            canonical_url: format!("https://github.com/acme/repo/pull/{id}"),
            display_title: id.into(),
            current_revision: None,
            metadata_refreshed_at: None,
            created_at: at.clone(),
        };
        let rule_id = airborne_core::RuleId::new(format!("rule-{id}")).unwrap();
        let version = RuleVersion::new(1).unwrap();
        let rule = Rule {
            id: rule_id.clone(),
            watch_id: watch_id.clone(),
            kind: RuleKind::GitHubCheckCompletes,
            enabled: true,
            current_version: version,
            created_at: at.clone(),
            updated_at: at.clone(),
            archived_at: None,
        };
        let definition = RuleDefinition {
            rule_id,
            version,
            config: RuleConfig::GitHubCheckCompletes {
                check_name: "Cursor Bugbot".into(),
                alert_on_start: false,
                alert_if_missing_after_seconds: None,
            },
            created_at: at.clone(),
        };
        RefreshTarget {
            watch: Watch {
                id: watch_id,
                subject_key,
                state: WatchState::Active,
                created_at: at.clone(),
                updated_at: at,
                archived_at: None,
            },
            subject,
            rules: vec![VersionedRule { rule, definition }],
        }
    }
    #[derive(Debug)]
    struct Guard {
        releases: AtomicUsize,
        renewals: AtomicUsize,
    }
    #[async_trait]
    impl LeaseGuard for Guard {
        async fn renew(&self) -> Result<(), LeaseError> {
            self.renewals.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn release(&self) -> Result<(), LeaseError> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[derive(Debug)]
    struct FakeLeases {
        refreshes: AtomicUsize,
        runners: AtomicUsize,
        releases: Arc<Guard>,
    }
    #[async_trait]
    impl LeaseStore for FakeLeases {
        async fn acquire_refresh(&self, _: Duration) -> Result<RefreshLease, LeaseError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(RefreshLease::new(self.releases.clone()))
        }
        async fn acquire_runner(&self) -> Result<RunnerLease, LeaseError> {
            self.runners.fetch_add(1, Ordering::SeqCst);
            Ok(RunnerLease::new(self.releases.clone()))
        }
    }
    struct FakeStore {
        targets: Vec<RefreshTarget>,
        commits: Mutex<Vec<PollCommit>>,
    }
    #[async_trait]
    impl RuntimeStore for FakeStore {
        async fn load_refresh_targets(
            &self,
            _: RefreshScope,
        ) -> Result<Vec<RefreshTarget>, StoreError> {
            Ok(self.targets.clone())
        }
        async fn load_rule_history(
            &self,
            candidates: &[(RuleKey, Revision)],
        ) -> Result<RuleHistorySet, StoreError> {
            let mut histories = RuleHistorySet::default();
            for (key, _) in candidates {
                let target = self
                    .targets
                    .iter()
                    .find(|target| target.rules.iter().any(|rule| rule.key() == *key))
                    .unwrap();
                histories.insert(
                    key.clone(),
                    RuleHistory {
                        watch_id: target.watch.id.clone(),
                        rule_kind: RuleKind::GitHubCheckCompletes,
                        latest_observation: None,
                        first_observed_at: None,
                        first_source_seen_at: None,
                        existing_alert_keys: std::collections::BTreeSet::default(),
                    },
                );
            }
            Ok(histories)
        }
        async fn apply_poll(&self, commit: PollCommit) -> Result<AppliedPoll, StoreError> {
            self.commits.lock().await.push(commit);
            Ok(AppliedPoll {
                new_alerts: Vec::new(),
            })
        }
    }
    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            timestamp()
        }
    }
    struct BlockingMonitor {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }
    #[async_trait]
    impl SubjectMonitor for BlockingMonitor {
        fn kind(&self) -> airborne_core::SubjectKind {
            airborne_core::SubjectKind::GitHubPullRequest
        }
        async fn poll(&self, request: MonitorRequest) -> MonitorReport {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(15)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            let rule = request.rules[0].key();
            let subject_key = request.subject.key;
            MonitorReport {
                subject_key: subject_key.clone(),
                metadata: None,
                revision: Some(airborne_core::Revision::new("head").unwrap()),
                results: vec![RulePollResult::Candidate(Candidate {
                    rule_key: rule,
                    subject_key,
                    revision: airborne_core::Revision::new("head").unwrap(),
                    state: CandidateState::Waiting,
                    source_identity: None,
                    source_url: None,
                    detail: None,
                    alert_intent: None,
                    observed_at: request.observed_at,
                })],
                subject_issue: None,
            }
        }
    }
    #[tokio::test]
    async fn refresh_limits_subject_work_to_four_and_orders_reports_by_watch() {
        let targets = vec![
            target("5"),
            target("3"),
            target("1"),
            target("4"),
            target("2"),
        ];
        let store = Arc::new(FakeStore {
            targets,
            commits: Mutex::new(Vec::new()),
        });
        let guard = Arc::new(Guard {
            releases: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        });
        let leases = Arc::new(FakeLeases {
            refreshes: AtomicUsize::new(0),
            runners: AtomicUsize::new(0),
            releases: guard.clone(),
        });
        let monitor = Arc::new(BlockingMonitor {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        });
        let runtime = Runtime::new(
            store.clone(),
            leases.clone(),
            Arc::new(FixedClock),
            vec![monitor.clone()],
        );
        let report = runtime
            .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(
            monitor.maximum.load(Ordering::SeqCst),
            MAX_CONCURRENT_SUBJECTS
        );
        assert_eq!(
            report
                .subjects
                .iter()
                .map(|subject| subject.watch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3", "4", "5"]
        );
        assert_eq!(store.commits.lock().await.len(), 5);
        assert_eq!(leases.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(guard.releases.load(Ordering::SeqCst), 1);
        assert!(guard.renewals.load(Ordering::SeqCst) >= 1);
    }
    struct PartialMonitor;
    #[async_trait]
    impl SubjectMonitor for PartialMonitor {
        fn kind(&self) -> airborne_core::SubjectKind {
            airborne_core::SubjectKind::GitHubPullRequest
        }
        async fn poll(&self, request: MonitorRequest) -> MonitorReport {
            let rule = request.rules[0].key();
            let results = if request.subject.display_title == "issue" {
                vec![RulePollResult::Issue {
                    rule,
                    issue: SourceIssueDraft {
                        scope: airborne_core::SourceIssueScope::Rule,
                        rule_id: None,
                        provider: airborne_core::Provider::GitHub,
                        kind: "network".into(),
                        safe_message: "unavailable".into(),
                        retryable: true,
                    },
                }]
            } else {
                vec![RulePollResult::Candidate(Candidate {
                    rule_key: rule,
                    subject_key: request.subject.key.clone(),
                    revision: airborne_core::Revision::new("head").unwrap(),
                    state: CandidateState::Completed,
                    source_identity: None,
                    source_url: None,
                    detail: None,
                    alert_intent: None,
                    observed_at: request.observed_at,
                })]
            };
            MonitorReport {
                subject_key: request.subject.key,
                metadata: None,
                revision: Some(airborne_core::Revision::new("head").unwrap()),
                results,
                subject_issue: None,
            }
        }
    }
    #[tokio::test]
    async fn refresh_commits_valid_subjects_when_another_has_an_issue() {
        let mut failed = target("2");
        failed.subject.display_title = "issue".into();
        let store = Arc::new(FakeStore {
            targets: vec![failed, target("1")],
            commits: Mutex::new(Vec::new()),
        });
        let runtime = Runtime::new(
            store.clone(),
            Arc::new(FakeLeases {
                refreshes: AtomicUsize::new(0),
                runners: AtomicUsize::new(0),
                releases: Arc::new(Guard {
                    releases: AtomicUsize::new(0),
                    renewals: AtomicUsize::new(0),
                }),
            }),
            Arc::new(FixedClock),
            vec![Arc::new(PartialMonitor)],
        );
        let report = runtime
            .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(report.outcome, PollOutcome::Partial);
        assert_eq!(report.subjects[0].outcome, PollOutcome::Success);
        assert_eq!(report.subjects[1].outcome, PollOutcome::Failure);
        assert_eq!(store.commits.lock().await.len(), 2);
    }

    struct LifecycleStore {
        targets: Vec<RefreshTarget>,
        history: Mutex<LifecycleHistory>,
        commits: Mutex<Vec<PollCommit>>,
        next_alert: AtomicUsize,
    }

    #[derive(Default)]
    struct LifecycleHistory {
        latest: BTreeMap<RuleKey, Observation>,
        revisions: BTreeMap<(RuleKey, Revision), (Timestamp, Option<Timestamp>)>,
        alert_keys: BTreeMap<RuleKey, BTreeSet<airborne_core::AlertKey>>,
    }

    impl LifecycleStore {
        fn history_for(
            &self,
            key: &RuleKey,
            revision: &Revision,
            stored: &LifecycleHistory,
        ) -> RuleHistory {
            let target = self
                .targets
                .iter()
                .find(|target| target.rules.iter().any(|rule| rule.key() == *key))
                .expect("test rule belongs to target");
            RuleHistory {
                watch_id: target.watch.id.clone(),
                rule_kind: target.rules[0].rule.kind,
                latest_observation: stored.latest.get(key).cloned(),
                first_observed_at: stored
                    .revisions
                    .get(&(key.clone(), revision.clone()))
                    .map(|(first_observed_at, _)| first_observed_at.clone()),
                first_source_seen_at: stored
                    .revisions
                    .get(&(key.clone(), revision.clone()))
                    .and_then(|(_, first_source_seen_at)| first_source_seen_at.clone()),
                existing_alert_keys: stored.alert_keys.get(key).cloned().unwrap_or_default(),
            }
        }
    }

    #[async_trait]
    impl RuntimeStore for LifecycleStore {
        async fn load_refresh_targets(
            &self,
            _: RefreshScope,
        ) -> Result<Vec<RefreshTarget>, StoreError> {
            Ok(self.targets.clone())
        }

        async fn load_rule_history(
            &self,
            candidates: &[(RuleKey, Revision)],
        ) -> Result<RuleHistorySet, StoreError> {
            let stored = self.history.lock().await;
            let mut loaded = RuleHistorySet::default();
            for (key, revision) in candidates {
                loaded.insert(key.clone(), self.history_for(key, revision, &stored));
            }
            Ok(loaded)
        }

        async fn apply_poll(&self, commit: PollCommit) -> Result<AppliedPoll, StoreError> {
            // Keep the observation and its alert keys under one lock to mirror the
            // store transaction required by the runtime boundary.
            let mut stored = self.history.lock().await;
            let mut new_alerts = Vec::new();
            for observation in &commit.observations {
                let key = RuleKey {
                    rule_id: observation.rule_id.clone(),
                    rule_version: observation.rule_version,
                };
                stored.latest.insert(key.clone(), observation.clone());
                stored.revisions.insert(
                    (key, observation.revision.clone()),
                    (
                        observation.first_observed_at.clone(),
                        observation.first_source_seen_at.clone(),
                    ),
                );
            }
            for draft in &commit.alerts {
                let key = RuleKey {
                    rule_id: draft.key.rule_id.clone(),
                    rule_version: draft.key.rule_version,
                };
                if stored
                    .alert_keys
                    .entry(key)
                    .or_default()
                    .insert(draft.key.clone())
                {
                    let id = self.next_alert.fetch_add(1, Ordering::SeqCst);
                    new_alerts.push(Alert {
                        id: AlertId::new(format!("alert-{id}")).unwrap(),
                        key: draft.key.clone(),
                        watch_id: draft.watch_id.clone(),
                        subject_key: draft.subject_key.clone(),
                        rule_kind: draft.rule_kind,
                        title: draft.title.clone(),
                        body: draft.body.clone(),
                        source_url: draft.source_url.clone(),
                        created_at: draft.created_at.clone(),
                        acknowledged_at: None,
                    });
                }
            }
            drop(stored);
            self.commits.lock().await.push(commit);
            Ok(AppliedPoll { new_alerts })
        }
    }

    struct SequenceMonitor(Mutex<VecDeque<Candidate>>);

    #[async_trait]
    impl SubjectMonitor for SequenceMonitor {
        fn kind(&self) -> airborne_core::SubjectKind {
            airborne_core::SubjectKind::GitHubPullRequest
        }

        async fn poll(&self, request: MonitorRequest) -> MonitorReport {
            let candidate = self.0.lock().await.pop_front().expect("queued candidate");
            MonitorReport {
                subject_key: request.subject.key,
                metadata: None,
                revision: Some(candidate.revision.clone()),
                results: vec![RulePollResult::Candidate(candidate)],
                subject_issue: None,
            }
        }
    }

    fn lifecycle_candidate(
        target: &RefreshTarget,
        revision: &str,
        at: &str,
        state: CandidateState,
        source: Option<&str>,
        alert_intent: Option<AlertIntent>,
    ) -> Candidate {
        Candidate {
            rule_key: target.rules[0].key(),
            subject_key: target.subject.key.clone(),
            revision: airborne_core::Revision::new(revision).unwrap(),
            state,
            source_identity: source.map(|source| SourceIdentity::new(source).unwrap()),
            source_url: None,
            detail: None,
            alert_intent,
            observed_at: Timestamp::parse(at).unwrap(),
        }
    }

    fn alert_intent(kind: AlertEventKind, source: Option<&str>) -> AlertIntent {
        match kind {
            AlertEventKind::Missing => AlertIntent::Missing {
                after_seconds: 60,
                title: "missing".into(),
                body: "missing".into(),
            },
            AlertEventKind::Started => AlertIntent::Started {
                source_identity: SourceIdentity::new(source.unwrap()).unwrap(),
                title: "started".into(),
                body: "started".into(),
            },
            AlertEventKind::Terminal => AlertIntent::Terminal {
                source_identity: SourceIdentity::new(source.unwrap()).unwrap(),
                title: "terminal".into(),
                body: "terminal".into(),
            },
        }
    }

    fn lifecycle_candidates(target: &RefreshTarget) -> Vec<Candidate> {
        vec![
            // The first observation only establishes the baseline.
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:00:00Z",
                CandidateState::NotDetected,
                None,
                Some(alert_intent(AlertEventKind::Missing, None)),
            ),
            // A missing source alerts only after its configured delay.
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:01:00Z",
                CandidateState::NotDetected,
                None,
                Some(alert_intent(AlertEventKind::Missing, None)),
            ),
            // Seeing a source suppresses later missing alerts for this revision.
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:02:00Z",
                CandidateState::Waiting,
                Some("run-1"),
                Some(alert_intent(AlertEventKind::Started, Some("run-1"))),
            ),
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:03:00Z",
                CandidateState::NotDetected,
                None,
                Some(alert_intent(AlertEventKind::Missing, None)),
            ),
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:04:00Z",
                CandidateState::Completed,
                Some("run-1"),
                Some(alert_intent(AlertEventKind::Terminal, Some("run-1"))),
            ),
            // A source first seen terminal emits only its terminal event.
            lifecycle_candidate(
                target,
                "rev-2",
                "2026-09-08T12:05:00Z",
                CandidateState::Completed,
                Some("run-2"),
                Some(alert_intent(AlertEventKind::Terminal, Some("run-2"))),
            ),
            // Returning to an earlier revision keeps that revision's timer.
            lifecycle_candidate(
                target,
                "rev-1",
                "2026-09-08T12:06:00Z",
                CandidateState::NotDetected,
                None,
                Some(alert_intent(AlertEventKind::Missing, None)),
            ),
            // The persisted terminal key suppresses a duplicate after restart.
            lifecycle_candidate(
                target,
                "rev-2",
                "2026-09-08T12:07:00Z",
                CandidateState::Completed,
                Some("run-2"),
                Some(alert_intent(AlertEventKind::Terminal, Some("run-2"))),
            ),
        ]
    }

    #[tokio::test]
    async fn runtime_persists_lifecycle_history_and_alert_keys_across_restarts() {
        let target = target("lifecycle");
        let candidates = lifecycle_candidates(&target);
        let store = Arc::new(LifecycleStore {
            targets: vec![target],
            history: Mutex::new(LifecycleHistory::default()),
            commits: Mutex::new(Vec::new()),
            next_alert: AtomicUsize::new(1),
        });
        let leases = Arc::new(FakeLeases {
            refreshes: AtomicUsize::new(0),
            runners: AtomicUsize::new(0),
            releases: Arc::new(Guard {
                releases: AtomicUsize::new(0),
                renewals: AtomicUsize::new(0),
            }),
        });
        let monitor = Arc::new(SequenceMonitor(Mutex::new(candidates.into())));
        let runtime = Runtime::new(
            store.clone(),
            leases.clone(),
            Arc::new(FixedClock),
            vec![monitor.clone()],
        );
        let mut event_kinds = Vec::new();
        for _ in 0..6 {
            let report = runtime
                .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
                .await
                .unwrap();
            event_kinds.extend(report.new_alerts().map(|alert| alert.key.event_kind));
        }
        let returned_to_a = runtime
            .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(
            returned_to_a.subjects[0].observations[0].first_observed_at,
            Timestamp::parse("2026-09-08T12:00:00Z").unwrap()
        );
        assert_eq!(
            event_kinds,
            vec![
                AlertEventKind::Missing,
                AlertEventKind::Started,
                AlertEventKind::Terminal,
                AlertEventKind::Terminal,
            ]
        );

        let restarted = Runtime::new(store, leases, Arc::new(FixedClock), vec![monitor]);
        let report = restarted
            .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
            .await
            .unwrap();
        assert!(report.new_alerts().next().is_none());
    }
    struct TestCancellation {
        cancelled: std::sync::atomic::AtomicBool,
        notification: tokio::sync::Notify,
    }
    impl TestCancellation {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.notification.notify_waiters();
        }
    }
    #[async_trait]
    impl Cancellation for TestCancellation {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Acquire)
        }
        async fn cancelled(&self) {
            if !self.is_cancelled() {
                self.notification.notified().await;
            }
        }
    }
    struct BlockingUntilCancelled {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl SubjectMonitor for BlockingUntilCancelled {
        fn kind(&self) -> airborne_core::SubjectKind {
            airborne_core::SubjectKind::GitHubPullRequest
        }
        async fn poll(&self, _: MonitorRequest) -> MonitorReport {
            self.started.notify_waiters();
            std::future::pending::<MonitorReport>().await
        }
    }
    #[tokio::test]
    async fn cancellation_drops_an_in_flight_monitor_without_committing() {
        let store = Arc::new(FakeStore {
            targets: vec![target("1")],
            commits: Mutex::new(Vec::new()),
        });
        let guard = Arc::new(Guard {
            releases: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let runtime = Runtime::new(
            store.clone(),
            Arc::new(FakeLeases {
                refreshes: AtomicUsize::new(0),
                runners: AtomicUsize::new(0),
                releases: guard.clone(),
            }),
            Arc::new(FixedClock),
            vec![Arc::new(BlockingUntilCancelled {
                started: started.clone(),
            })],
        );
        let cancellation = TestCancellation {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            notification: tokio::sync::Notify::new(),
        };
        let refresh = runtime.refresh(RefreshScope::AllActive, Duration::ZERO, &cancellation);
        tokio::pin!(refresh);
        tokio::select! { () = started.notified() => cancellation.cancel(), result = &mut refresh => panic!("refresh ended before cancellation: {result:?}"), }
        let report = tokio::time::timeout(Duration::from_secs(1), refresh)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.outcome, PollOutcome::Canceled);
        assert!(store.commits.lock().await.is_empty());
        assert_eq!(guard.releases.load(Ordering::SeqCst), 1);
    }
    struct StopSleeper(AtomicUsize);
    #[async_trait]
    impl Sleeper for StopSleeper {
        async fn sleep(&self, _: Duration, _: &dyn Cancellation) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            false
        }
    }
    #[tokio::test]
    async fn runner_polls_before_sleep_and_releases_its_lease() {
        let store = Arc::new(FakeStore {
            targets: vec![target("1")],
            commits: Mutex::new(Vec::new()),
        });
        let guard = Arc::new(Guard {
            releases: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        });
        let leases = Arc::new(FakeLeases {
            refreshes: AtomicUsize::new(0),
            runners: AtomicUsize::new(0),
            releases: guard.clone(),
        });
        let runtime = Runtime::new(
            store.clone(),
            leases.clone(),
            Arc::new(FixedClock),
            vec![Arc::new(BlockingMonitor {
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            })],
        );
        let sleeper = StopSleeper(AtomicUsize::new(0));
        let mut reports = Vec::new();
        runtime
            .run(
                Duration::from_secs(30),
                &sleeper,
                &NeverCancelled,
                &mut |report| reports.push(report.clone()),
            )
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(sleeper.0.load(Ordering::SeqCst), 1);
        assert_eq!(leases.runners.load(Ordering::SeqCst), 1);
        assert_eq!(leases.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(guard.releases.load(Ordering::SeqCst), 2);
        assert!(guard.renewals.load(Ordering::SeqCst) >= 2);
        assert_eq!(store.commits.lock().await.len(), 1);
    }
    struct BlockingSleeper {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl Sleeper for BlockingSleeper {
        async fn sleep(&self, _: Duration, cancellation: &dyn Cancellation) -> bool {
            self.started.notify_waiters();
            cancellation.cancelled().await;
            false
        }
    }
    fn discard(_: &RefreshReport) {}
    #[tokio::test]
    async fn runner_cancellation_during_sleep_releases_its_lease() {
        let store = Arc::new(FakeStore {
            targets: vec![target("1")],
            commits: Mutex::new(Vec::new()),
        });
        let guard = Arc::new(Guard {
            releases: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        });
        let leases = Arc::new(FakeLeases {
            refreshes: AtomicUsize::new(0),
            runners: AtomicUsize::new(0),
            releases: guard.clone(),
        });
        let runtime = Runtime::new(
            store,
            leases,
            Arc::new(FixedClock),
            vec![Arc::new(BlockingMonitor {
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            })],
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let sleeper = BlockingSleeper {
            started: started.clone(),
        };
        let cancellation = TestCancellation {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            notification: tokio::sync::Notify::new(),
        };
        let mut handler = discard;
        let run = runtime.run(
            Duration::from_secs(30),
            &sleeper,
            &cancellation,
            &mut handler,
        );
        tokio::pin!(run);
        tokio::select! { () = started.notified() => cancellation.cancel(), result = &mut run => panic!("runner ended before cancellation: {result:?}"), }
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(guard.releases.load(Ordering::SeqCst), 2);
    }
}
