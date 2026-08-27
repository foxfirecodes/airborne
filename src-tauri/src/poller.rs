use crate::{
    alerts::AlertSink,
    buildkite::{parse_build_url, Buildkite},
    github::Github,
    models::*,
    rules::{self, Evaluation},
    store::Store,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::sync::Arc;

pub struct Poller {
    pub store: Arc<Store>,
    pub tokens: Arc<TokenStore>,
    pub sink: Arc<dyn AlertSink>,
}
impl Poller {
    pub fn new(store: Arc<Store>, tokens: Arc<TokenStore>, sink: Arc<dyn AlertSink>) -> Self {
        Self {
            store,
            tokens,
            sink,
        }
    }
    pub async fn run_forever(self: Arc<Self>) {
        loop {
            let _ = self.refresh().await;
            let seconds = self
                .store
                .settings()
                .map(|s| s.poll_interval_seconds)
                .unwrap_or(60);
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        }
    }
    pub async fn refresh(&self) -> Result<()> {
        for watch in self.store.watches()?.into_iter().filter(|w| w.active) {
            if let Err(e) = self.poll_watch(&watch).await {
                self.store
                    .finish_poll(watch.id, None, Some(&e.to_string()))?;
            }
        }
        self.sink.dashboard_changed();
        Ok(())
    }
    async fn poll_watch(&self, watch: &Watch) -> Result<()> {
        let github = Github::new(self.tokens.github_token()?);
        let pr = github
            .pull_request(
                &watch.github_owner,
                &watch.github_repo,
                watch.github_pr_number,
            )
            .await?;
        let rules = self.store.rules_for_watch(watch.id)?;
        let checks = github
            .checks(&watch.github_owner, &watch.github_repo, &pr.head_sha)
            .await?;
        let statuses = github
            .statuses(&watch.github_owner, &watch.github_repo, &pr.head_sha)
            .await?;
        for rule in rules.into_iter().filter(|r| r.enabled) {
            let evaluation = match rule.kind {
                RuleKind::CursorBugbotCompleted => {
                    let cfg: serde_json::Value = serde_json::from_str(&rule.config_json)?;
                    rules::evaluate_bugbot(
                        cfg.get("check_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Cursor Bugbot"),
                        &checks,
                    )
                }
                RuleKind::BuildkiteJobCompleted => {
                    self.buildkite_evaluation(&rule, &statuses).await?
                }
            };
            self.persist_evaluation(&rule, &pr.head_sha, evaluation)?;
        }
        self.store.finish_poll(watch.id, Some(&pr.head_sha), None)?;
        Ok(())
    }
    async fn buildkite_evaluation(
        &self,
        rule: &Rule,
        statuses: &[crate::github::CommitStatus],
    ) -> Result<Evaluation> {
        let cfg: serde_json::Value = serde_json::from_str(&rule.config_json)?;
        let context = cfg["github_status_context"]
            .as_str()
            .context("rule github_status_context missing")?;
        let org = cfg["organization"]
            .as_str()
            .context("rule organization missing")?;
        let pipeline = cfg["pipeline"].as_str().context("rule pipeline missing")?;
        let name = cfg["job_name"].as_str().context("rule job_name missing")?;
        let notify = cfg["notify_on"].as_str().unwrap_or("terminal");
        let status = statuses
            .iter()
            .filter(|s| s.context == context)
            .max_by_key(|s| s.created_at.as_deref().unwrap_or(""));
        let Some(status) = status else {
            return Ok(rules::evaluate_buildkite(name, notify, &[], false));
        };
        let Some(url) = status.target_url.as_deref() else {
            return Ok(rules::evaluate_buildkite(name, notify, &[], false));
        };
        let build_ref = match parse_build_url(url, org, pipeline) {
            Ok(v) => v,
            Err(e) => return Ok(rules::unavailable(&e.to_string())),
        };
        let token = self.tokens.buildkite_token()?;
        let build = Buildkite::new(token).build(&build_ref).await?;
        Ok(rules::evaluate_buildkite(
            name,
            notify,
            &build.jobs,
            build.finished,
        ))
    }
    fn persist_evaluation(&self, rule: &Rule, sha: &str, e: Evaluation) -> Result<()> {
        let first_ever = !self.store.has_any_observation(rule.id)?;
        let obs = RuleObservation {
            rule_id: rule.id,
            head_sha: sha.into(),
            state: e.state,
            source_identity: e.source_identity,
            source_url: e.source_url,
            detail: e.detail,
            observed_at: Utc::now().to_rfc3339(),
        };
        let inserted = self.store.record(&obs, e.alert.as_ref(), !first_ever)?;
        if inserted {
            if let Some(alert) = self.store.alerts()?.into_iter().find(|a| {
                a.rule_id == rule.id
                    && a.head_sha == sha
                    && Some(&a.source_identity) == e.alert.as_ref().map(|x| &x.source_identity)
            }) {
                self.sink.notify(&alert)
            }
        }
        Ok(())
    }
}
pub struct TokenStore;
impl TokenStore {
    const SERVICE: &'static str = "com.prwatcher.v0";
    pub fn github_token(&self) -> Result<String> {
        self.get("github_token")
    }
    pub fn buildkite_token(&self) -> Result<String> {
        self.get("buildkite_token")
    }
    pub fn has_github_token(&self) -> bool {
        self.github_token().is_ok()
    }
    pub fn has_buildkite_token(&self) -> bool {
        self.buildkite_token().is_ok()
    }
    pub fn set_github_token(&self, t: &str) -> Result<()> {
        self.set("github_token", t)
    }
    pub fn set_buildkite_token(&self, t: &str) -> Result<()> {
        self.set("buildkite_token", t)
    }
    fn get(&self, key: &str) -> Result<String> {
        keyring::Entry::new(Self::SERVICE, key)?
            .get_password()
            .context("token is not configured")
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        keyring::Entry::new(Self::SERVICE, key)?.set_password(value)?;
        Ok(())
    }
}
