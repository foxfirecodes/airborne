use std::{path::Path, sync::Arc, time::Duration};

use airborne_core::{
    AlertIntent, Candidate, CandidateState, Provider, Revision, RuleConfig, RuleId, RuleKey,
    SourceIdentity, SourceIssueDraft, SourceIssueScope, Subject, SubjectKey, SubjectKind,
    Timestamp, WatchId, WatchState,
};
use airborne_runtime::{
    Clock, MonitorReport, MonitorRequest, NeverCancelled, RefreshScope, RulePollResult, Runtime,
    RuntimeStore, SubjectMonitor,
};
use airborne_store_sqlite::{
    AlertRepository, CatalogRepository, NewRule, NewWatch, RuleChange, SqliteStore,
};
use async_trait::async_trait;
use tempfile::TempDir;

fn at(second: u8) -> Timestamp {
    Timestamp::parse(&format!("2026-09-08T12:00:{second:02}Z")).unwrap()
}

fn config(name: &str) -> RuleConfig {
    RuleConfig::GitHubCheckCompletes {
        check_name: name.into(),
        alert_on_start: false,
        alert_if_missing_after_seconds: None,
    }
}

struct FixedClock(Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0.clone()
    }
}

enum Response {
    Candidate {
        revision: &'static str,
        state: CandidateState,
        source: Option<&'static str>,
        intent: bool,
    },
    Missing {
        revision: &'static str,
        after_seconds: u64,
    },
    Issue,
}

struct ScriptedMonitor {
    response: Response,
    observed_at: Timestamp,
}

#[async_trait]
impl SubjectMonitor for ScriptedMonitor {
    fn kind(&self) -> SubjectKind {
        SubjectKind::GitHubPullRequest
    }

    async fn poll(&self, request: MonitorRequest) -> MonitorReport {
        match self.response {
            Response::Candidate {
                revision,
                state,
                source,
                intent,
            } => {
                let source_identity = source.map(|value| SourceIdentity::new(value).unwrap());
                let candidate = Candidate {
                    rule_key: request.rules[0].key(),
                    subject_key: request.subject.key.clone(),
                    revision: Revision::new(revision).unwrap(),
                    state,
                    source_identity: source_identity.clone(),
                    source_url: Some("https://example.test/check".into()),
                    detail: Some("test result".into()),
                    alert_intent: intent.then(|| AlertIntent::Terminal {
                        source_identity: source_identity.unwrap(),
                        title: "check finished".into(),
                        body: "test result".into(),
                    }),
                    observed_at: self.observed_at.clone(),
                };
                MonitorReport {
                    subject_key: request.subject.key,
                    metadata: None,
                    revision: Some(candidate.revision.clone()),
                    results: vec![RulePollResult::Candidate(candidate)],
                    subject_issue: None,
                }
            }
            Response::Missing {
                revision,
                after_seconds,
            } => {
                let candidate = Candidate {
                    rule_key: request.rules[0].key(),
                    subject_key: request.subject.key.clone(),
                    revision: Revision::new(revision).unwrap(),
                    state: CandidateState::NotDetected,
                    source_identity: None,
                    source_url: None,
                    detail: None,
                    alert_intent: Some(AlertIntent::Missing {
                        after_seconds,
                        title: "check missing".into(),
                        body: "check has not appeared".into(),
                    }),
                    observed_at: self.observed_at.clone(),
                };
                MonitorReport {
                    subject_key: request.subject.key,
                    metadata: None,
                    revision: Some(candidate.revision.clone()),
                    results: vec![RulePollResult::Candidate(candidate)],
                    subject_issue: None,
                }
            }
            Response::Issue => MonitorReport {
                subject_key: request.subject.key,
                metadata: None,
                revision: None,
                results: vec![RulePollResult::Issue {
                    rule: request.rules[0].key(),
                    issue: SourceIssueDraft {
                        scope: SourceIssueScope::Source,
                        rule_id: Some(request.rules[0].rule.id.clone()),
                        provider: Provider::GitHub,
                        kind: "network".into(),
                        safe_message: "provider request failed".into(),
                        retryable: true,
                    },
                }],
                subject_issue: None,
            },
        }
    }
}

async fn setup(path: &Path) -> (WatchId, RuleId) {
    let store = SqliteStore::open(path).unwrap();
    let subject = Subject {
        key: SubjectKey::new("github.com/acme/airborne#42").unwrap(),
        kind: SubjectKind::GitHubPullRequest,
        canonical_url: "https://github.com/acme/airborne/pull/42".into(),
        display_title: "CLI lifecycle test".into(),
        current_revision: None,
        metadata_refreshed_at: None,
        created_at: at(0),
    };
    let watch = store
        .add_watch(NewWatch {
            subject,
            state: WatchState::Active,
        })
        .await
        .unwrap();
    let rule = store
        .add_rule(NewRule {
            watch_id: watch.id.clone(),
            config: config("ci"),
            enabled: true,
            created_at: at(0),
        })
        .await
        .unwrap();
    (watch.id, rule.id)
}

async fn refresh(path: &Path, response: Response, observed_at: Timestamp) -> usize {
    let store = Arc::new(SqliteStore::open(path).unwrap());
    let runtime = Runtime::new(
        store.clone(),
        store,
        Arc::new(FixedClock(observed_at.clone())),
        vec![Arc::new(ScriptedMonitor {
            response,
            observed_at,
        })],
    );
    runtime
        .refresh(RefreshScope::AllActive, Duration::ZERO, &NeverCancelled)
        .await
        .unwrap()
        .new_alerts()
        .count()
}

async fn alerts(path: &Path) -> usize {
    SqliteStore::open(path)
        .unwrap()
        .list_alerts(false, None)
        .await
        .unwrap()
        .len()
}

async fn latest_observation(path: &Path, rule: RuleId, version: u64) -> airborne_core::Observation {
    let store = SqliteStore::open(path).unwrap();
    store
        .load_rule_history(&[(
            RuleKey {
                rule_id: rule,
                rule_version: airborne_core::RuleVersion::new(version).unwrap(),
            },
            airborne_core::Revision::new("a").unwrap(),
        )])
        .await
        .unwrap()
        .0
        .into_values()
        .next()
        .unwrap()
        .latest_observation
        .unwrap()
}

#[tokio::test]
async fn first_terminal_candidate_remains_a_baseline_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    setup(&path).await;

    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true,
            },
            at(1),
        )
        .await,
        0
    );
    assert_eq!(alerts(&path).await, 0);
}

#[tokio::test]
async fn waiting_to_terminal_same_revision_alerts_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    setup(&path).await;
    refresh(
        &path,
        Response::Candidate {
            revision: "a",
            state: CandidateState::Waiting,
            source: None,
            intent: false,
        },
        at(1),
    )
    .await;

    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true,
            },
            at(2),
        )
        .await,
        1
    );
}

#[tokio::test]
async fn same_alert_identity_deduplicates_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    setup(&path).await;
    for second in [1, 2] {
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: if second == 1 {
                    CandidateState::Waiting
                } else {
                    CandidateState::Completed
                },
                source: (second == 2).then_some("check-a"),
                intent: second == 2,
            },
            at(second),
        )
        .await;
    }

    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true,
            },
            at(3),
        )
        .await,
        0
    );
    assert_eq!(alerts(&path).await, 1);
}

#[tokio::test]
async fn terminal_candidate_for_new_revision_alerts_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    setup(&path).await;
    refresh(
        &path,
        Response::Candidate {
            revision: "a",
            state: CandidateState::Completed,
            source: Some("check-a"),
            intent: true,
        },
        at(1),
    )
    .await;

    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "b",
                state: CandidateState::Completed,
                source: Some("check-b"),
                intent: true,
            },
            at(2),
        )
        .await,
        1
    );
}

#[tokio::test]
async fn updated_reenabled_and_resumed_rules_baseline_terminal_results_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    let (watch, rule) = setup(&path).await;
    refresh(
        &path,
        Response::Candidate {
            revision: "a",
            state: CandidateState::Completed,
            source: Some("check-a"),
            intent: true,
        },
        at(1),
    )
    .await;

    let store = SqliteStore::open(&path).unwrap();
    let updated = store
        .update_rule(RuleChange {
            id: rule.clone(),
            config: config("ci-updated"),
            at: at(2),
        })
        .await
        .unwrap();
    assert_eq!(updated.current_version.get(), 2);
    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true
            },
            at(3)
        )
        .await,
        0
    );

    let store = SqliteStore::open(&path).unwrap();
    store
        .change_rule_state(rule.clone(), false, at(4))
        .await
        .unwrap();
    let enabled = store
        .change_rule_state(rule.clone(), true, at(5))
        .await
        .unwrap();
    assert_eq!(enabled.current_version.get(), 3);
    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true
            },
            at(6)
        )
        .await,
        0
    );

    let store = SqliteStore::open(&path).unwrap();
    store
        .change_watch_state(watch.clone(), WatchState::Paused, at(7))
        .await
        .unwrap();
    let resumed = store
        .change_watch_state(watch, WatchState::Active, at(8))
        .await
        .unwrap();
    assert_eq!(resumed.state, WatchState::Active);
    assert_eq!(
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state: CandidateState::Completed,
                source: Some("check-a"),
                intent: true
            },
            at(9)
        )
        .await,
        0
    );
    assert_eq!(alerts(&path).await, 0);
}

#[tokio::test]
async fn source_issue_after_restart_keeps_last_observation() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    let (_, rule) = setup(&path).await;
    refresh(
        &path,
        Response::Candidate {
            revision: "a",
            state: CandidateState::Waiting,
            source: None,
            intent: false,
        },
        at(1),
    )
    .await;
    let before = latest_observation(&path, rule.clone(), 1).await;

    assert_eq!(refresh(&path, Response::Issue, at(2)).await, 0);
    assert_eq!(latest_observation(&path, rule, 1).await, before);
}

#[tokio::test]
async fn source_first_seen_time_survives_same_revision_source_transitions_and_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    let (_, rule) = setup(&path).await;

    for (second, state, source) in [
        (1, CandidateState::NotDetected, None),
        (2, CandidateState::Waiting, Some("check-a")),
        (3, CandidateState::NotDetected, None),
    ] {
        refresh(
            &path,
            Response::Candidate {
                revision: "a",
                state,
                source,
                intent: false,
            },
            at(second),
        )
        .await;
    }

    let observation = latest_observation(&path, rule, 1).await;
    assert_eq!(observation.first_observed_at, at(1));
    assert_eq!(observation.first_source_seen_at, Some(at(2)));
}

#[tokio::test]
async fn revisiting_a_revision_keeps_its_missing_deadline_after_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("airborne.sqlite");
    let (_, rule) = setup(&path).await;
    SqliteStore::open(&path)
        .unwrap()
        .update_rule(RuleChange {
            id: rule,
            config: RuleConfig::GitHubCheckCompletes {
                check_name: "ci".into(),
                alert_on_start: false,
                alert_if_missing_after_seconds: Some(2),
            },
            at: at(0),
        })
        .await
        .unwrap();

    assert_eq!(
        refresh(
            &path,
            Response::Missing {
                revision: "a",
                after_seconds: 2,
            },
            at(1),
        )
        .await,
        0
    );
    assert_eq!(
        refresh(
            &path,
            Response::Missing {
                revision: "b",
                after_seconds: 2,
            },
            at(2),
        )
        .await,
        0
    );
    assert_eq!(
        refresh(
            &path,
            Response::Missing {
                revision: "a",
                after_seconds: 2,
            },
            at(3),
        )
        .await,
        1
    );
}
