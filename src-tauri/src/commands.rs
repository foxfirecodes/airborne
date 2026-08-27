use crate::{
    alerts::AlertSink,
    github::{parse_pr_url, Github},
    models::*,
    poller::{Poller, TokenStore},
    rules,
    store::Store,
};
use std::sync::Arc;
use tauri::State;

pub struct AppState {
    pub store: Arc<Store>,
    pub poller: Arc<Poller>,
    pub tokens: Arc<TokenStore>,
}
impl AppState {
    pub fn new(store: Arc<Store>, sink: Arc<dyn AlertSink>) -> Self {
        let tokens = Arc::new(TokenStore);
        let poller = Arc::new(Poller::new(store.clone(), tokens.clone(), sink));
        Self {
            store,
            poller,
            tokens,
        }
    }
}
fn err<E: ToString>(e: E) -> String {
    e.to_string()
}
#[tauri::command]
pub fn get_dashboard(state: State<'_, AppState>) -> Result<Dashboard, String> {
    let store = &state.store;
    Ok(Dashboard {
        watches: store.watches().map_err(err)?,
        rules: store.rules().map_err(err)?,
        observations: store.observations().map_err(err)?,
        alerts: store.alerts().map_err(err)?,
        unread_count: store.unread_count().map_err(err)?,
    })
}
#[tauri::command]
pub async fn add_watch(url: String, state: State<'_, AppState>) -> Result<Watch, String> {
    let (owner, repo, number) = parse_pr_url(&url).map_err(err)?;
    let token = state.tokens.github_token().map_err(err)?;
    let pr = Github::new(token)
        .pull_request(&owner, &repo, number)
        .await
        .map_err(err)?;
    state
        .store
        .add_watch(&owner, &repo, number, &pr.title, Some(&pr.head_sha))
        .map_err(err)
}
#[tauri::command]
pub fn create_rule(
    watch_id: i64,
    input: CreateRuleInput,
    state: State<'_, AppState>,
) -> Result<Rule, String> {
    rules::validate_kind(&input.kind, &input.config).map_err(err)?;
    state.store.add_rule(watch_id, &input).map_err(err)
}
#[tauri::command]
pub fn update_rule(
    rule_id: i64,
    input: UpdateRuleInput,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let rule = state
        .store
        .rules()
        .map_err(err)?
        .into_iter()
        .find(|rule| rule.id == rule_id)
        .ok_or_else(|| "rule not found".to_string())?;
    rules::validate_kind(&rule.kind, &input.config).map_err(err)?;
    state.store.update_rule(rule_id, &input).map_err(err)
}
#[tauri::command]
pub fn delete_rule(rule_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    state.store.delete_rule(rule_id).map_err(err)
}
#[tauri::command]
pub async fn refresh_now(state: State<'_, AppState>) -> Result<(), String> {
    state.poller.refresh().await.map_err(err)
}
#[tauri::command]
pub fn mark_alert_read(alert_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    state.store.mark_read(alert_id).map_err(err)
}
#[tauri::command]
pub fn mark_all_alerts_read(state: State<'_, AppState>) -> Result<(), String> {
    state.store.mark_all_read().map_err(err)
}
#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<SettingsResponse, String> {
    let settings = state.store.settings().map_err(err)?;
    Ok(SettingsResponse {
        settings,
        github_token_configured: state.tokens.has_github_token(),
        buildkite_token_configured: state.tokens.has_buildkite_token(),
    })
}
#[tauri::command]
pub fn save_settings(
    settings: SaveSettingsInput,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let persisted = settings.settings();
    state.store.save_settings(&persisted).map_err(err)?;
    if let Some(token) = settings.github_token.filter(|x| !x.is_empty()) {
        state.tokens.set_github_token(&token).map_err(err)?
    }
    if let Some(token) = settings.buildkite_token.filter(|x| !x.is_empty()) {
        state.tokens.set_buildkite_token(&token).map_err(err)?
    }
    Ok(())
}
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(err)?;
    if parsed.scheme() != "https"
        || !matches!(
            parsed.host_str(),
            Some("github.com" | "buildkite.com" | "cursor.com")
        )
    {
        return Err("only GitHub, Buildkite, and Cursor HTTPS links can be opened".into());
    }
    open::that_detached(url).map_err(err)
}
