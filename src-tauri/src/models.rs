use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    CursorBugbotCompleted,
    BuildkiteJobCompleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    Waiting,
    InProgress,
    Completed,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    Unread,
    Read,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watch {
    pub id: i64,
    pub github_owner: String,
    pub github_repo: String,
    pub github_pr_number: i64,
    pub title: String,
    pub active: bool,
    pub created_at: String,
    pub head_sha: Option<String>,
    pub last_poll_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub watch_id: i64,
    pub kind: RuleKind,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleObservation {
    pub rule_id: i64,
    pub head_sha: String,
    pub state: RuleState,
    pub source_identity: Option<String>,
    pub source_url: Option<String>,
    pub detail: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: i64,
    pub rule_id: i64,
    pub head_sha: String,
    pub source_identity: String,
    pub title: String,
    pub body: String,
    pub status: AlertStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PipelineMapping {
    pub github_status_context: String,
    pub organization: String,
    pub pipeline: String,
    #[serde(default)]
    pub available_job_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub pipeline_mappings: Vec<PipelineMapping>,
}
fn default_poll_interval() -> u64 {
    60
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60,
            pipeline_mappings: vec![],
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsResponse {
    #[serde(flatten)]
    pub settings: Settings,
    pub github_token_configured: bool,
    pub buildkite_token_configured: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSettingsInput {
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub pipeline_mappings: Vec<PipelineMapping>,
    pub github_token: Option<String>,
    pub buildkite_token: Option<String>,
}
impl SaveSettingsInput {
    pub fn settings(&self) -> Settings {
        Settings {
            poll_interval_seconds: self.poll_interval_seconds,
            pipeline_mappings: self.pipeline_mappings.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRuleInput {
    pub kind: RuleKind,
    pub config: serde_json::Value,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}
fn enabled_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRuleInput {
    pub config: serde_json::Value,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub watches: Vec<Watch>,
    pub rules: Vec<Rule>,
    pub observations: Vec<RuleObservation>,
    pub alerts: Vec<Alert>,
    pub unread_count: usize,
}
