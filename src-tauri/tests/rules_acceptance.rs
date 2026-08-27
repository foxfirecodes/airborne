use pr_watcher::{
    buildkite::parse_build_url,
    models::{CreateRuleInput, RuleKind, RuleObservation, RuleState},
    rules::{evaluate_bugbot, evaluate_buildkite, stable_ids, BuildkiteJob, CheckRun},
    store::Store,
};
use tempfile::NamedTempFile;

fn bugbot(status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun {
        id: "check-309883".into(),
        name: "Cursor Bugbot".into(),
        status: status.into(),
        conclusion: conclusion.map(str::to_owned),
        details_url: Some("https://cursor.com/docs/bugbot".into()),
    }
}

fn job(id: &str, name: &str, state: &str) -> BuildkiteJob {
    BuildkiteJob {
        id: id.into(),
        name: name.into(),
        state: state.into(),
        web_url: Some(format!(
            "https://buildkite.com/discord/discord-web/builds/594583#{}",
            id
        )),
    }
}

#[test]
fn completed_neutral_bugbot_creates_a_completion_alert() {
    // This mirrors the observed PR 309883 fixture. `neutral` must not turn
    // completion into a success-only condition.
    let result = evaluate_bugbot("Cursor Bugbot", &[bugbot("completed", Some("neutral"))]);

    assert_eq!(result.state, RuleState::Completed);
    assert_eq!(result.detail.as_deref(), Some("neutral"));
    assert_eq!(result.source_identity.as_deref(), Some("check-309883"));
    assert_eq!(
        result.alert.unwrap().title,
        "Cursor Bugbot finished — neutral"
    );
}

#[test]
fn in_progress_bugbot_creates_no_alert() {
    let result = evaluate_bugbot("Cursor Bugbot", &[bugbot("in_progress", None)]);

    assert_eq!(result.state, RuleState::InProgress);
    assert!(result.alert.is_none());
}

#[test]
fn missing_bugbot_check_waits_without_an_alert() {
    let result = evaluate_bugbot("Cursor Bugbot", &[]);

    assert_eq!(result.state, RuleState::Waiting);
    assert!(result.alert.is_none());
}

#[test]
fn a_passed_matching_buildkite_job_creates_a_passed_alert() {
    let result = evaluate_buildkite(
        "Web lint",
        "terminal",
        &[job("job-1", "Web lint", "passed")],
        false,
    );

    assert_eq!(result.state, RuleState::Completed);
    assert_eq!(result.alert.unwrap().title, "Web lint passed");
}

#[test]
fn failed_buildkite_job_alerts_for_terminal_but_not_passed_only() {
    let jobs = [job("job-1", "Web lint", "failed")];

    let terminal = evaluate_buildkite("Web lint", "terminal", &jobs, false);
    assert_eq!(terminal.state, RuleState::Failed);
    assert_eq!(terminal.alert.unwrap().title, "Web lint finished — failed");

    let passed_only = evaluate_buildkite("Web lint", "passed", &jobs, false);
    assert_eq!(passed_only.state, RuleState::Failed);
    assert!(passed_only.alert.is_none());
}

#[test]
fn parallel_matching_jobs_wait_until_every_job_is_terminal() {
    let result = evaluate_buildkite(
        "Web lint",
        "terminal",
        &[
            job("job-1", "Web lint", "passed"),
            job("job-2", "Web lint", "running"),
        ],
        false,
    );

    assert_eq!(result.state, RuleState::InProgress);
    assert!(result.alert.is_none());
}

#[test]
fn all_parallel_terminal_jobs_emit_one_aggregate_alert_with_stable_identity() {
    let jobs = [
        job("job-2", "Web lint", "failed"),
        job("job-1", "Web lint", "passed"),
    ];
    let result = evaluate_buildkite("Web lint", "terminal", &jobs, true);

    assert_eq!(result.state, RuleState::Failed);
    let alert = result.alert.unwrap();
    assert_eq!(alert.source_identity, stable_ids(&["job-1", "job-2"]));
    assert_eq!(alert.title, "Web lint finished — failed");
}

#[test]
fn missing_buildkite_job_waits_while_build_runs_then_becomes_unavailable() {
    let waiting = evaluate_buildkite(
        "Web lint",
        "terminal",
        &[job("other", "Other job", "running")],
        false,
    );
    assert_eq!(waiting.state, RuleState::Waiting);
    assert!(waiting.alert.is_none());

    let unavailable = evaluate_buildkite(
        "Web lint",
        "terminal",
        &[job("other", "Other job", "passed")],
        true,
    );
    assert_eq!(unavailable.state, RuleState::Unavailable);
    assert!(unavailable.alert.is_none());
}

#[test]
fn aggregate_job_identity_is_independent_of_api_order() {
    assert_eq!(
        stable_ids(&["job-1", "job-2"]),
        stable_ids(&["job-2", "job-1"])
    );
}

#[test]
fn buildkite_status_url_must_resolve_the_configured_build() {
    let build = parse_build_url(
        "https://buildkite.com/discord/discord-web/builds/594583",
        "discord",
        "discord-web",
    )
    .unwrap();

    assert_eq!(build.organization, "discord");
    assert_eq!(build.pipeline, "discord-web");
    assert_eq!(build.number, 594583);
}

#[test]
fn malformed_or_wrong_buildkite_urls_are_unavailable_not_fetchable() {
    for url in [
        "https://buildkite.com/discord/another-pipeline/builds/594583",
        "https://buildkite.com/discord/discord-web/builds/594583?redirect=elsewhere",
        "https://buildkite.com/discord/discord-web/builds/594583/",
        "https://buildkite.com:443/discord/discord-web/builds/594583",
        "https://user@buildkite.com/discord/discord-web/builds/594583",
        "https://api.buildkite.com/v2/organizations/discord/pipelines/discord-web/builds/594583",
        "not a URL",
    ] {
        assert!(
            parse_build_url(url, "discord", "discord-web").is_err(),
            "{url}"
        );
    }
}

fn recorded_observation(
    rule_id: i64,
    sha: &str,
    result: &pr_watcher::rules::Evaluation,
) -> RuleObservation {
    RuleObservation {
        rule_id,
        head_sha: sha.into(),
        state: result.state.clone(),
        source_identity: result.source_identity.clone(),
        source_url: result.source_url.clone(),
        detail: result.detail.clone(),
        observed_at: "2026-08-21T00:00:00Z".into(),
    }
}

#[test]
fn completed_source_at_rule_creation_is_a_baseline_not_an_alert() {
    let store = Store::memory().unwrap();
    let watch = store
        .add_watch("discord", "discord", 309883, "fixture", Some("sha-1"))
        .unwrap();
    let rule = store
        .add_rule(
            watch.id,
            &CreateRuleInput {
                kind: RuleKind::CursorBugbotCompleted,
                config: serde_json::json!({"check_name": "Cursor Bugbot"}),
                enabled: true,
            },
        )
        .unwrap();
    let result = evaluate_bugbot("Cursor Bugbot", &[bugbot("completed", Some("neutral"))]);

    let inserted = store
        .record(
            &recorded_observation(rule.id, "sha-1", &result),
            result.alert.as_ref(),
            false,
        )
        .unwrap();

    assert!(!inserted);
    assert!(store.alerts().unwrap().is_empty());
}

#[test]
fn new_head_completion_alerts_once_and_remains_deduped_after_restart() {
    let database = NamedTempFile::new().unwrap();
    let path = database.path().to_str().unwrap().to_owned();
    let store = Store::open(&path).unwrap();
    let watch = store
        .add_watch("discord", "discord", 309883, "fixture", Some("sha-old"))
        .unwrap();
    let rule = store
        .add_rule(
            watch.id,
            &CreateRuleInput {
                kind: RuleKind::CursorBugbotCompleted,
                config: serde_json::json!({"check_name": "Cursor Bugbot"}),
                enabled: true,
            },
        )
        .unwrap();
    let result = evaluate_bugbot("Cursor Bugbot", &[bugbot("completed", Some("neutral"))]);

    // The rule starts from an already completed source, so this first revision
    // becomes its baseline. A later SHA is armed and may alert.
    assert!(!store
        .record(
            &recorded_observation(rule.id, "sha-old", &result),
            result.alert.as_ref(),
            false
        )
        .unwrap());
    assert!(store
        .record(
            &recorded_observation(rule.id, "sha-new", &result),
            result.alert.as_ref(),
            true
        )
        .unwrap());
    assert_eq!(store.alerts().unwrap().len(), 1);
    drop(store);

    let restarted = Store::open(&path).unwrap();
    assert!(!restarted
        .record(
            &recorded_observation(rule.id, "sha-new", &result),
            result.alert.as_ref(),
            true
        )
        .unwrap());
    assert_eq!(restarted.alerts().unwrap().len(), 1);
}
