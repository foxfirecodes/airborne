//! Pure rule evaluation. It deliberately knows nothing about SQLite, HTTP, or Tauri.
use crate::models::{RuleKind, RuleState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRun {
    pub id: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildkiteJob {
    pub id: String,
    pub name: String,
    pub state: String,
    pub web_url: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evaluation {
    pub state: RuleState,
    pub source_identity: Option<String>,
    pub source_url: Option<String>,
    pub detail: Option<String>,
    pub alert: Option<AlertCandidate>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertCandidate {
    pub source_identity: String,
    pub title: String,
    pub body: String,
}

pub fn evaluate_bugbot(check_name: &str, checks: &[CheckRun]) -> Evaluation {
    let Some(check) = checks.iter().find(|c| c.name == check_name) else {
        return waiting();
    };
    if check.status != "completed" {
        return Evaluation {
            state: RuleState::InProgress,
            source_identity: Some(check.id.clone()),
            source_url: check.details_url.clone(),
            detail: None,
            alert: None,
        };
    }
    let conclusion = check
        .conclusion
        .clone()
        .unwrap_or_else(|| "completed".into());
    Evaluation {
        state: RuleState::Completed,
        source_identity: Some(check.id.clone()),
        source_url: check.details_url.clone(),
        detail: Some(conclusion.clone()),
        alert: Some(AlertCandidate {
            source_identity: check.id.clone(),
            title: format!("{check_name} finished — {conclusion}"),
            body: format!("{check_name} completed for this pull request revision."),
        }),
    }
}

pub fn evaluate_buildkite(
    job_name: &str,
    notify_on: &str,
    jobs: &[BuildkiteJob],
    build_finished: bool,
) -> Evaluation {
    let selected: Vec<_> = jobs.iter().filter(|j| j.name == job_name).collect();
    if selected.is_empty() {
        return if build_finished {
            unavailable("No matching Buildkite job")
        } else {
            waiting()
        };
    }
    let ids = source_identity(&selected);
    let url = selected.first().and_then(|j| j.web_url.clone());
    if selected.iter().any(|j| !is_terminal(&j.state)) {
        return Evaluation {
            state: RuleState::InProgress,
            source_identity: Some(ids),
            source_url: url,
            detail: None,
            alert: None,
        };
    }
    let all_passed = selected.iter().all(|j| j.state == "passed");
    let best = selected
        .iter()
        .find(|j| j.state != "passed")
        .map(|j| j.state.as_str())
        .unwrap_or("passed");
    let state = if all_passed {
        RuleState::Completed
    } else {
        RuleState::Failed
    };
    let identity = source_identity(&selected);
    let should_alert = notify_on == "terminal" || (notify_on == "passed" && all_passed);
    let title = if all_passed {
        format!("{job_name} passed")
    } else {
        format!("{job_name} finished — {best}")
    };
    Evaluation {
        state,
        source_identity: Some(identity.clone()),
        source_url: url,
        detail: Some(best.into()),
        alert: should_alert.then(|| AlertCandidate {
            source_identity: identity,
            title,
            body: format!("Buildkite job {job_name} completed for this pull request revision."),
        }),
    }
}

pub fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "passed" | "failed" | "timed_out" | "canceled" | "skipped" | "broken" | "expired"
    )
}
pub fn stable_ids(ids: &[&str]) -> String {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    let mut h = Sha256::new();
    for id in ids {
        h.update(id.as_bytes());
        h.update([0]);
    }
    format!("jobs:{}", &format!("{:x}", h.finalize())[..24])
}
fn source_identity(jobs: &[&BuildkiteJob]) -> String {
    if jobs.len() == 1 {
        jobs[0].id.clone()
    } else {
        stable_ids(&jobs.iter().map(|job| job.id.as_str()).collect::<Vec<_>>())
    }
}
fn waiting() -> Evaluation {
    Evaluation {
        state: RuleState::Waiting,
        source_identity: None,
        source_url: None,
        detail: None,
        alert: None,
    }
}
pub fn unavailable(detail: &str) -> Evaluation {
    Evaluation {
        state: RuleState::Unavailable,
        source_identity: None,
        source_url: None,
        detail: Some(detail.into()),
        alert: None,
    }
}

pub fn validate_kind(kind: &RuleKind, config: &serde_json::Value) -> Result<(), String> {
    match kind {
        RuleKind::CursorBugbotCompleted => config
            .get("check_name")
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(|_| ())
            .ok_or("Cursor Bugbot rules need check_name".into()),
        RuleKind::BuildkiteJobCompleted => {
            for key in [
                "github_status_context",
                "organization",
                "pipeline",
                "job_name",
            ] {
                if config
                    .get(key)
                    .and_then(|x| x.as_str())
                    .filter(|x| !x.is_empty())
                    .is_none()
                {
                    return Err(format!("Buildkite rules need {key}"));
                }
            }
            match config
                .get("notify_on")
                .and_then(|x| x.as_str())
                .unwrap_or("terminal")
            {
                "terminal" | "passed" => Ok(()),
                _ => Err("notify_on must be terminal or passed".into()),
            }
        }
    }
}
