#![allow(
    clippy::ignored_unit_patterns,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]

use std::{
    env,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode},
    sync::Arc,
    time::Duration,
};

use airborne_buildkite::{BuildSnapshot, BuildkiteApi, BuildkiteError, ReqwestBuildkiteClient};
use airborne_core::{
    BuildkiteJobName, BuildkiteNotifyOn, BuildkiteOrganization, BuildkitePipeline,
    GitHubStatusContext, PresetId, PresetRuleId, RuleConfig, RuleId, Subject, SubjectKey,
    SubjectKind, Timestamp, WatchId, WatchState,
};
#[cfg(not(debug_assertions))]
use airborne_credentials_macos::CredentialPresence;
use airborne_credentials_macos::{
    CredentialError, CredentialStore, ExposeSecret, MacosCredentialStore, ProviderCredential,
    SecretString,
};
use airborne_github::{parse_pull_request_url, GitHubApi, ReqwestGitHubApi};
use airborne_pr::GitHubPullRequestMonitor;
use airborne_runtime::{
    Cancellation, Clock, NeverCancelled, RefreshReport, RefreshScope, Runtime, RuntimeStore,
    Sleeper,
};
use airborne_store_sqlite::{
    AlertRepository, CatalogRepository, NewPreset, NewRule, NewWatch, PresetRepository,
    PresetRuleChange, RuleChange, SqliteStore,
};
use chrono::{DateTime, Local, Utc};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use notify_rust::Notification;
use serde_json::{json, Value};

const CHECK: &str = "Cursor Bugbot";
const FAILURE: u8 = 1;
const INPUT: u8 = 2;
const PARTIAL: u8 = 3;
const LEASE: u8 = 4;
const CREDENTIAL: u8 = 5;

#[derive(Parser)]
#[command(
    name = "airborne",
    about = "Watch pull requests and report when important checks finish",
    version
)]
struct Cli {
    #[arg(long, global = true, help = "Print machine-readable output")]
    json: bool,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use a different data directory"
    )]
    data_dir: Option<PathBuf>,
    #[arg(short, long, global=true, action=ArgAction::Count, help = "Show more detail; repeat for debug output")]
    verbose: u8,
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "verbose",
        help = "Print only errors and requested output"
    )]
    quiet: bool,
    #[arg(long, global = true, help = "Disable colored output")]
    no_color: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Manage watched pull requests.
    Watch(WatchCmd),
    /// Manage rules for watched pull requests.
    Rule(RuleCmd),
    /// Manage reusable rule presets.
    Preset(PresetCmd),
    /// Check active watches once, then exit.
    Refresh(Refresh),
    /// Poll active watches in the foreground.
    Run(Run),
    /// Show watches, rules, and their latest state.
    Status(Status),
    /// List and acknowledge alerts.
    Alerts(Alerts),
    /// Configure GitHub and Buildkite access.
    Auth(Auth),
    /// View and change local settings.
    Config(Config),
    /// Check credentials, storage, and provider access.
    Doctor(Doctor),
    /// Import data from the desktop prototype.
    Migrate(Migrate),
}
#[derive(Args)]
struct WatchCmd {
    #[command(subcommand)]
    command: WatchSub,
}
#[derive(Subcommand)]
enum WatchSub {
    /// Add a pull request watch.
    Add {
        #[arg(
            value_name = "GITHUB_PR_URL",
            help = "Canonical HTTPS GitHub pull request URL"
        )]
        github_pr_url: String,
        #[arg(
            long,
            value_name = "PRESET_ID_OR_NAME",
            help = "Copy this preset's rules into the new watch"
        )]
        preset: Option<String>,
        #[arg(long, help = "Create the watch paused")]
        paused: bool,
    },
    /// List watches.
    List {
        #[arg(long, conflicts_with = "all", help = "Show active watches")]
        active: bool,
        #[arg(long, help = "Include paused and archived watches")]
        all: bool,
    },
    /// Show one watch.
    Show {
        #[arg(value_name = "WATCH_ID_OR_GITHUB_PR_URL")]
        watch_id: String,
    },
    /// Pause a watch.
    Pause { watch_id: String },
    /// Resume a watch and reset its rule baselines.
    Resume { watch_id: String },
    /// Archive a watch.
    Remove {
        #[arg(value_name = "WATCH_ID_OR_GITHUB_PR_URL")]
        watch_id: String,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Args)]
struct PresetCmd {
    #[command(subcommand)]
    command: PresetSub,
}
#[derive(Subcommand)]
enum PresetSub {
    /// Add a reusable rule preset.
    Add {
        #[arg(value_name = "NAME", help = "A unique name for the preset")]
        name: String,
        #[arg(long, value_name = "DESCRIPTION", help = "Describe this preset")]
        description: Option<String>,
    },
    /// List presets.
    List {
        #[arg(long, help = "Include archived presets")]
        all: bool,
    },
    /// Show a preset and its rules.
    Show {
        #[arg(value_name = "PRESET_ID_OR_NAME", help = "Preset ID or name")]
        preset_id: String,
    },
    /// Rename a preset.
    Rename {
        #[arg(value_name = "PRESET_ID_OR_NAME", help = "Preset ID or name")]
        preset_id: String,
        #[arg(value_name = "NEW_NAME", help = "New unique preset name")]
        name: String,
    },
    /// Archive a preset.
    Remove {
        #[arg(value_name = "PRESET_ID_OR_NAME", help = "Preset ID or name")]
        preset_id: String,
        #[arg(long, help = "Skip the confirmation prompt")]
        yes: bool,
    },
}
#[derive(Args)]
struct RuleCmd {
    #[command(subcommand)]
    command: RuleSub,
}
#[derive(Subcommand)]
enum RuleSub {
    /// Add a rule.
    Add {
        #[command(subcommand)]
        kind: RuleAdd,
    },
    /// List rules, optionally for one watch or preset.
    List {
        #[arg(
            long,
            value_name = "WATCH_ID_OR_GITHUB_PR_URL",
            help = "Limit rules to this watch"
        )]
        watch: Option<String>,
        #[arg(
            long,
            value_name = "PRESET_ID_OR_NAME",
            conflicts_with = "watch",
            help = "Limit rules to this preset"
        )]
        preset: Option<String>,
        #[arg(long, conflicts_with = "all", help = "Show enabled rules")]
        enabled: bool,
        #[arg(long, help = "Include disabled and archived rules")]
        all: bool,
    },
    /// Show a rule and its current version.
    Show {
        #[arg(value_name = "RULE_ID_OR_NAME")]
        rule_id: String,
        #[arg(
            long,
            value_name = "WATCH_ID_OR_GITHUB_PR_URL",
            help = "Select the rule on this watch"
        )]
        watch: Option<String>,
        #[arg(
            long,
            value_name = "PRESET_ID_OR_NAME",
            conflicts_with = "watch",
            help = "Select the rule in this preset"
        )]
        preset: Option<String>,
    },
    /// Enable a rule and reset its baseline.
    Enable {
        #[arg(value_name = "RULE_ID_OR_NAME")]
        rule_id: String,
        #[arg(long)]
        watch: Option<String>,
    },
    /// Disable a rule without deleting its history.
    Disable {
        #[arg(value_name = "RULE_ID_OR_NAME")]
        rule_id: String,
        #[arg(long)]
        watch: Option<String>,
    },
    /// Update a rule's matching policy.
    Update(RuleUpdate),
    /// Copy a preset's rules onto a watch.
    Apply {
        #[arg(long, value_name = "PRESET_ID_OR_NAME", help = "Preset to copy")]
        preset: String,
        #[arg(
            long,
            value_name = "WATCH_ID_OR_GITHUB_PR_URL",
            help = "Watch that receives the copied rules"
        )]
        watch: String,
        #[arg(long, help = "Replace same-kind rules already on the watch")]
        replace: bool,
    },
    /// Archive a rule.
    Remove {
        #[arg(value_name = "RULE_ID_OR_NAME")]
        rule_id: String,
        #[arg(
            long,
            value_name = "WATCH_ID_OR_GITHUB_PR_URL",
            help = "Select the rule on this watch"
        )]
        watch: Option<String>,
        #[arg(
            long,
            value_name = "PRESET_ID_OR_NAME",
            conflicts_with = "watch",
            help = "Select the rule in this preset"
        )]
        preset: Option<String>,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Subcommand)]
enum RuleAdd {
    /// Alert when Cursor Bugbot completes.
    #[command(
        override_usage = "airborne rule add bugbot (--watch <WATCH_ID_OR_GITHUB_PR_URL> | --preset <PRESET_ID_OR_NAME>) [OPTIONS]"
    )]
    Bugbot {
        #[command(flatten)]
        target: RuleAddTarget,
        #[arg(long, help = "Replace the current Bugbot rule, if any")]
        replace: bool,
        #[arg(long, default_value=CHECK)]
        check_name: String,
        #[arg(long, help = "Alert when Bugbot starts")]
        alert_on_start: bool,
        #[arg(long, value_parser = parse_positive_duration, help = "Alert if Bugbot is not detected after this delay (for example, 5m)")]
        alert_if_missing_after: Option<Duration>,
    },
    #[command(name = "buildkite-job")]
    /// Alert when a Buildkite job completes.
    BuildkiteJob(BuildkiteArgs),
}
#[derive(Args)]
struct BuildkiteArgs {
    #[command(flatten)]
    target: RuleAddTarget,
    #[arg(long, help = "Replace the current Buildkite rule, if any")]
    replace: bool,
    #[arg(long)]
    context: String,
    #[arg(long)]
    organization: String,
    #[arg(long)]
    pipeline: String,
    #[arg(long)]
    job: String,
    #[arg(long, value_enum, default_value_t=Notify::Terminal)]
    notify_on: Notify,
}
#[derive(Args)]
struct RuleAddTarget {
    #[arg(
        long,
        value_name = "WATCH_ID_OR_GITHUB_PR_URL",
        required_unless_present = "preset",
        conflicts_with = "preset",
        help = "Add the rule to this watch"
    )]
    watch: Option<String>,
    #[arg(
        long,
        value_name = "PRESET_ID_OR_NAME",
        required_unless_present = "watch",
        conflicts_with = "watch",
        help = "Add the rule to this preset"
    )]
    preset: Option<String>,
}
#[derive(Clone, Copy, ValueEnum)]
enum Notify {
    Terminal,
    Passed,
}
#[derive(Args)]
struct RuleUpdate {
    #[arg(value_name = "RULE_ID_OR_NAME")]
    rule_id: String,
    #[arg(
        long,
        value_name = "WATCH_ID_OR_GITHUB_PR_URL",
        help = "Select the rule on this watch"
    )]
    watch: Option<String>,
    #[arg(
        long,
        value_name = "PRESET_ID_OR_NAME",
        conflicts_with = "watch",
        help = "Select the rule in this preset"
    )]
    preset: Option<String>,
    #[arg(long)]
    check_name: Option<String>,
    #[arg(long)]
    context: Option<String>,
    #[arg(long)]
    organization: Option<String>,
    #[arg(long)]
    pipeline: Option<String>,
    #[arg(long)]
    job: Option<String>,
    #[arg(long, value_enum)]
    notify_on: Option<Notify>,
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_alert_on_start")]
    alert_on_start: bool,
    #[arg(long = "no-alert-on-start", action = ArgAction::SetTrue, conflicts_with = "alert_on_start")]
    no_alert_on_start: bool,
    #[arg(long, value_parser = parse_positive_duration, conflicts_with = "no_alert_if_missing_after")]
    alert_if_missing_after: Option<Duration>,
    #[arg(long = "no-alert-if-missing-after", action = ArgAction::SetTrue, conflicts_with = "alert_if_missing_after")]
    no_alert_if_missing_after: bool,
}
#[derive(Args)]
struct Refresh {
    watch_id: Option<String>,
    #[arg(long, default_value="5s", value_parser=parse_duration)]
    wait: Duration,
}
#[derive(Args)]
struct Run {
    #[arg(long, value_parser=parse_duration)]
    interval: Option<Duration>,
}
#[derive(Args)]
struct Status {
    #[arg(long)]
    watch: Option<String>,
}
#[derive(Args)]
struct Alerts {
    #[command(subcommand)]
    command: AlertsSub,
}
#[derive(Subcommand)]
enum AlertsSub {
    /// List alerts.
    List {
        #[arg(long, conflicts_with = "all", help = "Show pending alerts")]
        pending: bool,
        #[arg(long, help = "Include acknowledged alerts")]
        all: bool,
        #[arg(long)]
        watch: Option<String>,
    },
    /// Show one alert without acknowledging it.
    Show { alert_id: String },
    /// Acknowledge one alert or all pending alerts.
    Acknowledge {
        #[arg(conflicts_with = "all")]
        alert_id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Args)]
struct Auth {
    #[command(subcommand)]
    command: AuthSub,
}
#[derive(Subcommand)]
enum AuthSub {
    /// Set a credential from hidden terminal input.
    Set { provider: ProviderArg },
    /// Show credential sources without exposing tokens.
    Status,
    /// Remove a credential.
    Remove {
        provider: ProviderArg,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum ProviderArg {
    Github,
    Buildkite,
}
#[derive(Args)]
struct Config {
    #[command(subcommand)]
    command: ConfigSub,
}
#[derive(Subcommand)]
enum ConfigSub {
    /// Read one setting or the default settings.
    Get { key: Option<String> },
    /// Set a local configuration value.
    Set {
        key: ConfigKey,
        #[arg(value_parser=parse_duration)]
        value: Duration,
    },
    /// Print the data directory and database path.
    Path,
}
#[derive(Clone, Copy, ValueEnum)]
enum ConfigKey {
    #[value(name = "poll-interval")]
    PollInterval,
}
#[derive(Args)]
struct Doctor {
    #[arg(long)]
    live: bool,
    #[arg(long, help = "Send a test system notification")]
    test_notifications: bool,
}
#[derive(Args)]
struct Migrate {
    #[command(subcommand)]
    command: MigrateSub,
}
#[derive(Subcommand)]
enum MigrateSub {
    /// Import supported data from the desktop prototype.
    Prototype {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        credentials: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone)]
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(Utc::now())
    }
}
struct Cancel(tokio_util::sync::CancellationToken);
impl Cancel {
    fn new() -> Self {
        Self(tokio_util::sync::CancellationToken::new())
    }
    fn cancel(&self) {
        self.0.cancel();
    }
}
#[async_trait::async_trait]
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    async fn cancelled(&self) {
        self.0.cancelled().await;
    }
}
struct TokioSleep;
#[async_trait::async_trait]
impl Sleeper for TokioSleep {
    async fn sleep(&self, duration: Duration, canceled: &dyn Cancellation) -> bool {
        if canceled.is_cancelled() {
            return false;
        }
        tokio::select! { _ = tokio::time::sleep(duration) => true, _ = canceled.cancelled() => false }
    }
}
struct MissingBuildkite;
#[async_trait::async_trait]
impl BuildkiteApi for MissingBuildkite {
    async fn build(
        &self,
        _: &airborne_core::BuildkiteBuildKey,
    ) -> Result<BuildSnapshot, BuildkiteError> {
        Err(BuildkiteError::CredentialRejected)
    }
}
struct Error {
    code: u8,
    message: String,
}
impl Error {
    fn input(message: impl Into<String>) -> Self {
        Self {
            code: INPUT,
            message: message.into(),
        }
    }
    fn fail(message: impl Into<String>) -> Self {
        Self {
            code: FAILURE,
            message: message.into(),
        }
    }
}
struct Out {
    json: bool,
    quiet: bool,
    color: bool,
}
struct EchoGuard;
impl EchoGuard {
    fn disable() -> Result<Self, Error> {
        let status = ProcessCommand::new("stty")
            .arg("-echo")
            .status()
            .map_err(|e| Error::fail(format!("could not disable terminal echo: {e}")))?;
        if status.success() {
            Ok(Self)
        } else {
            Err(Error::fail("could not disable terminal echo"))
        }
    }
}
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new("stty").arg("echo").status();
    }
}
impl Out {
    fn emit(&self, command: &str, data: Value) -> Result<(), Error> {
        if self.json {
            println!(
                "{}",
                json!({"schema_version":1,"command":command,"outcome":"success","data":data})
            );
        } else if !self.quiet {
            println!("{}", human(command, &data, self.color));
        }
        Ok(())
    }
}
fn human(command: &str, data: &Value, color: bool) -> String {
    match command {
        "watch.list" => render_watch_list(data, color),
        "watch.show" => render_watch_show(data, color),
        "rule.list"
            if data["rules"]
                .as_array()
                .and_then(|rules| rules.first())
                .is_some_and(|rule| rule.get("preset_id").is_some()) =>
        {
            render_preset_rule_list(data, color)
        }
        "rule.list" => render_rule_list(data, color),
        "rule.show" if data.get("preset_id").is_some() => render_preset_rule_show(data, color),
        "rule.show" => render_rule_show(data, color),
        "preset.list" => render_preset_list(data, color),
        "preset.show" => render_preset_show(data, color),
        "alerts.list" => render_alert_list(data, color),
        "alerts.show" => render_alert_show(data, color),
        "refresh" | "run" => format!(
            "{}: {} watch(es), {} new alert(s), {} issue(s)",
            data["outcome"].as_str().unwrap_or("completed"),
            data["subjects"].as_array().map_or(0, Vec::len),
            data["subjects"]
                .as_array()
                .map(|items| items
                    .iter()
                    .map(|item| item["new_alerts"].as_array().map_or(0, Vec::len))
                    .sum::<usize>())
                .unwrap_or(0),
            data["subjects"]
                .as_array()
                .map(|items| items
                    .iter()
                    .map(|item| item["issues"].as_array().map_or(0, Vec::len))
                    .sum::<usize>())
                .unwrap_or(0)
        ),
        "status" => render_status(data, color),
        _ => serde_json::to_string_pretty(data).unwrap_or_else(|_| "completed".into()),
    }
}
fn style(value: impl AsRef<str>, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{}\x1b[0m", value.as_ref())
    } else {
        value.as_ref().to_owned()
    }
}
fn bold(value: impl AsRef<str>, color: bool) -> String {
    style(value, "1", color)
}
fn dim(value: impl AsRef<str>, color: bool) -> String {
    style(value, "2", color)
}
fn state(value: &str, color: bool) -> String {
    let normalized = value.to_ascii_lowercase();
    let code = match normalized.as_str() {
        "active" | "enabled" | "completed" | "passed" | "success" | "terminal" => "32",
        "paused" | "pending" | "waiting" | "in progress" | "in_progress" | "acknowledged"
        | "started" => "33",
        "failed" | "unavailable" | "missing" | "not detected" | "not_detected" => "31",
        _ => "2",
    };
    style(value.replace('_', " "), code, color)
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn id(value: &Value) -> String {
    let value = value.as_str().unwrap_or("unknown");
    let end = value.char_indices().nth(12).map_or(value.len(), |(i, _)| i);
    format!(
        "{}{}",
        &value[..end],
        if end < value.len() { "…" } else { "" }
    )
}
fn timestamp(value: &Value) -> String {
    value
        .as_str()
        .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
        .map(|x| {
            x.with_timezone(&Local)
                .format("%b %-d, %Y at %-I:%M %p %Z")
                .to_string()
        })
        .unwrap_or_else(|| "never".into())
}
fn pr(subject: &Value) -> String {
    let url = text(subject, "canonical_url");
    let key = text(subject, "key")
        .strip_prefix("github.com/")
        .unwrap_or(text(subject, "key"));
    let key = key.strip_prefix("https://github.com/").unwrap_or(key);
    let key = key.strip_prefix("github.com/").unwrap_or(key);
    let key = key.replace("/pull/", " #");
    let key = if key.is_empty() {
        url.trim_start_matches("https://github.com/")
            .replace("/pull/", " #")
    } else {
        key
    };
    let title = text(subject, "display_title");
    if title.is_empty() {
        key
    } else {
        format!("{key} — {title}")
    }
}
fn rule_name(rule: &Value) -> String {
    let config = if rule["definition"]["config"].is_object() {
        &rule["definition"]["config"]
    } else {
        &rule["config"]
    };
    match text(config, "kind") {
        "git_hub_check_completes" => text(config, "check_name").to_owned(),
        "buildkite_job_completes" => format!("Buildkite · {}", text(config, "job_name")),
        _ => text(
            rule["rule"].as_object().map_or(rule, |_| &rule["rule"]),
            "kind",
        )
        .replace('_', " "),
    }
}
fn observation_state(rule: &Value) -> String {
    rule["latest_observation"]["state"].as_str().map_or_else(
        || "no observation".into(),
        |x| human_candidate_state(x).to_owned(),
    )
}
fn rule_entity(rule: &Value) -> &Value {
    rule["rule"].as_object().map_or(rule, |_| &rule["rule"])
}
fn rule_display_state(rule: &Value) -> String {
    let entity = rule_entity(rule);
    if !entity["archived_at"].is_null() {
        "archived".into()
    } else if entity["enabled"].as_bool() == Some(false) {
        "disabled".into()
    } else {
        observation_state(rule)
    }
}
fn missing_for(observation: &Value) -> Option<String> {
    let first = observation["first_observed_at"].as_str()?;
    let first = DateTime::parse_from_rfc3339(first)
        .ok()?
        .with_timezone(&Utc);
    let elapsed = u64::try_from(Utc::now().signed_duration_since(first).num_seconds()).unwrap_or(0);
    Some(format!("missing for {}", duration(elapsed)))
}
fn event_label(event: &str) -> String {
    let event = event.replace('_', " ");
    let mut chars = event.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}
fn plural(count: u64, singular: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {singular}s")
    }
}
fn policy(rule: &Value) -> Option<String> {
    let c = if rule["definition"]["config"].is_object() {
        &rule["definition"]["config"]
    } else {
        &rule["config"]
    };
    if text(c, "kind") != "git_hub_check_completes" {
        return Some("Alert on terminal result".into());
    }
    let mut values = Vec::new();
    if c["alert_on_start"].as_bool() == Some(true) {
        values.push("Alert when started".into());
    }
    if let Some(seconds) = c["alert_if_missing_after_seconds"].as_u64() {
        values.push(format!("alert if missing after {}", duration(seconds)));
    }
    values.push("Alert when completed".into());
    Some(values.join("  ·  "))
}
fn duration(seconds: u64) -> String {
    if seconds % 3600 == 0 {
        plural(seconds / 3600, "hour")
    } else if seconds % 60 == 0 {
        plural(seconds / 60, "minute")
    } else {
        plural(seconds, "second")
    }
}
fn render_status(data: &Value, color: bool) -> String {
    let watches = data["watches"].as_array().cloned().unwrap_or_default();
    if watches.is_empty() {
        return format!("{}\nNo watches.", bold("Airborne status", color));
    }
    let pending: u64 = watches
        .iter()
        .map(|x| x["pending_alert_count"].as_u64().unwrap_or(0))
        .sum();
    let mut out = vec![
        bold("Airborne status", color),
        format!(
            "{}  ·  {}",
            plural(watches.len() as u64, "watch"),
            plural(pending, "pending alert")
        ),
    ];
    for watch in watches {
        let w = &watch["watch"];
        out.push(format!(
            "\n{}  {}",
            bold(pr(&watch["subject"]), color),
            state(text(w, "state"), color)
        ));
        for rule in watch["rules"].as_array().into_iter().flatten() {
            out.push(format!(
                "  {}  {}",
                rule_name(rule),
                state(&rule_display_state(rule), color)
            ));
            if !rule["latest_observation"]["observed_at"].is_null() {
                out.push(format!(
                    "    {}",
                    dim(
                        format!(
                            "observed {}",
                            timestamp(&rule["latest_observation"]["observed_at"])
                        ),
                        color
                    )
                ));
            }
            if let Some(issue) = rule["latest_issue"].as_object() {
                out.push(format!(
                    "    {}",
                    style(
                        format!(
                            "Issue: {}",
                            issue
                                .get("safe_message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown source error")
                        ),
                        "31",
                        color
                    )
                ));
            }
        }
        if watch["pending_alert_count"].as_u64().unwrap_or(0) > 0 {
            out.push(format!(
                "  {}",
                state(
                    &plural(
                        watch["pending_alert_count"].as_u64().unwrap_or(0),
                        "pending alert"
                    ),
                    color
                )
            ));
        }
        if let Some(issue) = watch["latest_issue"].as_object() {
            out.push(format!(
                "  {}",
                style(
                    format!(
                        "Issue: {}",
                        issue
                            .get("safe_message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown source error")
                    ),
                    "31",
                    color
                )
            ));
        }
        if !watch["subject"]["current_revision"].is_null() {
            out.push(format!(
                "  {}",
                dim(
                    format!("Revision {}", text(&watch["subject"], "current_revision")),
                    color
                )
            ));
        }
        if let Some(checked) = watch["latest_poll_finished_at"].as_str() {
            let outcome = watch["latest_poll_outcome"].as_str().unwrap_or("unknown");
            out.push(format!(
                "  {}",
                dim(
                    format!(
                        "Checked {}  ·  last poll {outcome}",
                        timestamp(&Value::String(checked.into()))
                    ),
                    color
                )
            ));
        }
        out.push(format!(
            "  {}",
            dim(
                format!(
                    "{}  ·  {}",
                    id(&w["id"]),
                    text(&watch["subject"], "canonical_url")
                ),
                color
            )
        ));
    }
    out.join("\n")
}
fn render_watch_list(data: &Value, color: bool) -> String {
    let watches = data["watches"].as_array().cloned().unwrap_or_default();
    if watches.is_empty() {
        return format!("{}\nNo watches.", bold("Watches", color));
    }
    let active = watches
        .iter()
        .filter(|x| text(x, "state") == "active")
        .count();
    let paused = watches
        .iter()
        .filter(|x| text(x, "state") == "paused")
        .count();
    let mut out = vec![
        bold("Watches", color),
        format!(
            "{} total  ·  {} active  ·  {} paused",
            watches.len(),
            active,
            paused
        ),
    ];
    for w in watches {
        out.push(format!(
            "\n{}  {}",
            bold(pr(&w["subject"]), color),
            state(text(&w, "state"), color)
        ));
        let mut detail = plural(w["active_rule_count"].as_u64().unwrap_or(0), "rule");
        if w["pending_alert_count"].as_u64().unwrap_or(0) > 0 {
            write!(
                detail,
                "  ·  {}",
                plural(
                    w["pending_alert_count"].as_u64().unwrap_or(0),
                    "pending alert"
                )
            )
            .expect("write to string");
        }
        if !w["latest_poll"]["finished_at"].is_null() {
            write!(
                detail,
                "  ·  {}",
                dim(
                    format!("checked {}", timestamp(&w["latest_poll"]["finished_at"])),
                    color
                )
            )
            .expect("write to string");
        }
        out.push(format!("  {detail}"));
        out.push(format!(
            "  {}",
            dim(text(&w["subject"], "canonical_url"), color)
        ));
        out.push(format!("  {}", dim(id(&w["id"]), color)));
    }
    out.join("\n")
}
fn render_watch_show(w: &Value, color: bool) -> String {
    let mut out = vec![
        format!(
            "{}  {}",
            bold(pr(&w["subject"]), color),
            state(text(w, "state"), color)
        ),
        String::new(),
        format!(
            "Pull request   {}",
            dim(text(&w["subject"], "canonical_url"), color)
        ),
        format!(
            "Revision       {}",
            dim(text(&w["subject"], "current_revision"), color)
        ),
    ];
    if !w["latest_poll"]["finished_at"].is_null() {
        out.push(format!(
            "Last checked   {}",
            dim(timestamp(&w["latest_poll"]["finished_at"]), color)
        ));
    }
    out.push(format!("\n{}", bold("Rules", color)));
    let rules = w["rules"].as_array().cloned().unwrap_or_default();
    if rules.is_empty() {
        out.push("  No rules.".into());
    }
    for r in rules {
        out.push(format!(
            "  {}  {}",
            bold(rule_name(&r), color),
            state(&rule_display_state(&r), color)
        ));
        if let Some(p) = policy(&r) {
            out.push(format!("    {p}"));
        }
    }
    out.push(format!("\n{}", bold("Alerts", color)));
    let alerts = w["pending_alerts"].as_array().cloned().unwrap_or_default();
    if alerts.is_empty() {
        out.push("  No pending alerts".into());
    } else {
        for a in alerts {
            out.push(format!("  {}", bold(text(&a, "title"), color)));
        }
    }
    out.push(format!("\n{}", dim(id(&w["id"]), color)));
    out.join("\n")
}
fn render_rule_list(data: &Value, color: bool) -> String {
    let rules = data["rules"].as_array().cloned().unwrap_or_default();
    if rules.is_empty() {
        return format!("{}\nNo rules.", bold("Rules", color));
    }
    let enabled = rules
        .iter()
        .filter(|x| x["enabled"].as_bool() == Some(true))
        .count();
    let mut out = vec![
        bold("Rules", color),
        format!("{} total  ·  {} enabled", rules.len(), enabled),
    ];
    for r in rules {
        let enabled = if !r["archived_at"].is_null() {
            "archived"
        } else if r["enabled"].as_bool() == Some(true) {
            "enabled"
        } else {
            "disabled"
        };
        out.push(format!(
            "\n{}  {}",
            bold(rule_name(&r), color),
            state(enabled, color)
        ));
        out.push(format!("  {}", bold(pr(&r["subject"]), color)));
        let observed = &r["latest_observation"];
        let mut line = state(&rule_display_state(&r), color);
        if rule_display_state(&r) == "not detected" {
            if let Some(elapsed) = missing_for(observed) {
                write!(line, "  ·  {elapsed}").expect("write to string");
            }
        }
        if !observed["observed_at"].is_null() {
            write!(
                line,
                "  ·  {}",
                dim(
                    format!("checked {}", timestamp(&observed["observed_at"])),
                    color
                )
            )
            .expect("write to string");
        }
        out.push(format!("  {line}"));
        out.push(format!("  {}", dim(id(&r["id"]), color)));
    }
    out.join("\n")
}
fn render_preset_list(data: &Value, color: bool) -> String {
    let presets = data["presets"].as_array().cloned().unwrap_or_default();
    if presets.is_empty() {
        return format!("{}\nNo presets.", bold("Presets", color));
    }
    let mut out = vec![bold("Presets", color)];
    for preset in presets {
        let status = if preset["archived_at"].is_null() {
            "active"
        } else {
            "archived"
        };
        out.push(format!(
            "\n{}  {}\n  {}",
            bold(text(&preset, "name"), color),
            state(status, color),
            dim(id(&preset["id"]), color)
        ));
        if let Some(description) = preset["description"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            out.push(format!("  {description}"));
        }
    }
    out.join("\n")
}
fn render_preset_rule_list(data: &Value, color: bool) -> String {
    let rules = data["rules"].as_array().cloned().unwrap_or_default();
    if rules.is_empty() {
        return format!("{}\nNo rules.", bold("Preset rules", color));
    }
    let mut out = vec![bold("Preset rules", color)];
    for rule in rules {
        out.push(format!(
            "  {}  {}",
            bold(rule_name(&rule), color),
            dim(id(&rule["id"]), color)
        ));
        if let Some(policy) = policy(&rule) {
            out.push(format!("    {policy}"));
        }
    }
    out.join("\n")
}
fn render_preset_rule_show(rule: &Value, color: bool) -> String {
    let mut out = vec![
        bold(rule_name(rule), color),
        format!("ID              {}", dim(id(&rule["id"]), color)),
    ];
    if let Some(policy) = policy(rule) {
        out.push(policy);
    }
    out.join("\n")
}
fn render_preset_show(data: &Value, color: bool) -> String {
    let preset = &data["preset"];
    let mut out = vec![
        bold(text(preset, "name"), color),
        format!("ID              {}", dim(id(&preset["id"]), color)),
        format!(
            "State           {}",
            state(
                if preset["archived_at"].is_null() {
                    "active"
                } else {
                    "archived"
                },
                color
            )
        ),
        String::new(),
    ];
    if let Some(description) = preset["description"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        out.insert(2, format!("Description     {description}"));
    }
    out.push(render_preset_rule_list(
        &json!({"rules": data["rules"]}),
        color,
    ));
    out.join("\n")
}
fn render_rule_show(r: &Value, color: bool) -> String {
    let enabled = if !r["archived_at"].is_null() {
        "archived"
    } else if r["enabled"].as_bool() == Some(true) {
        "enabled"
    } else {
        "disabled"
    };
    let check = rule_name(r);
    let observed = &r["latest_observation"];
    let mut out = vec![
        format!("{}  {}", bold(&check, color), state(enabled, color)),
        bold(pr(&r["subject"]), color),
        String::new(),
    ];
    let config = &r["definition"]["config"];
    if text(config, "kind") == "git_hub_check_completes" {
        out.push(format!("Check name       {check}"));
    } else {
        out.push(format!("Job              {}", text(config, "job_name")));
        out.push(format!(
            "Context          {}",
            text(config, "github_status_context")
        ));
        out.push(format!(
            "Organization     {}",
            text(config, "expected_organization")
        ));
        out.push(format!(
            "Pipeline         {}",
            text(config, "expected_pipeline")
        ));
    }
    out.extend([
        format!("Current state    {}", state(&rule_display_state(r), color)),
        format!(
            "Last observed    {}",
            dim(timestamp(&observed["observed_at"]), color)
        ),
        format!(
            "Revision         {}",
            dim(text(observed, "revision"), color)
        ),
        format!("\n{}", bold("Alerts", color)),
    ]);
    if let Some(p) = policy(r) {
        for p in p.split("  ·  ") {
            let p = p
                .strip_prefix("Alert when ")
                .map_or_else(|| p.to_owned(), |value| format!("When {value}"));
            let p = if let Some(value) = p.strip_prefix("alert if ") {
                format!("If {value}")
            } else {
                p
            };
            out.push(format!("  {p}"));
        }
    }
    out.push(format!(
        "\n{}",
        dim(
            format!(
                "{}  ·  version {}",
                id(&r["id"]),
                r["current_version"].as_u64().unwrap_or(0)
            ),
            color
        )
    ));
    out.join("\n")
}
fn render_alert_list(data: &Value, color: bool) -> String {
    let alerts = data["alerts"].as_array().cloned().unwrap_or_default();
    let all = text(data, "mode") == "all";
    let heading = if all { "Alerts" } else { "Pending alerts" };
    if alerts.is_empty() {
        return format!(
            "{}\n{}",
            bold(heading, color),
            if all {
                "No alerts."
            } else {
                "No pending alerts."
            }
        );
    }
    let mut out = vec![bold(heading, color), plural(alerts.len() as u64, "alert")];
    for a in alerts {
        let status = if a["acknowledged_at"].is_null() {
            "pending"
        } else {
            "acknowledged"
        };
        out.push(format!(
            "\n{}  {}",
            bold(text(&a, "title"), color),
            state(status, color)
        ));
        out.push(format!("  {}", bold(pr(&a["subject"]), color)));
        let detail = if text(&a["key"], "event_kind") == "missing" {
            missing_for(&a["observation"]).unwrap_or_else(|| text(&a, "body").to_owned())
        } else {
            text(&a, "body").to_owned()
        };
        out.push(format!(
            "  {}  ·  {}",
            detail,
            dim(timestamp(&a["created_at"]), color)
        ));
        out.push(format!("  {}", dim(id(&a["id"]), color)));
    }
    out.join("\n")
}
fn render_alert_show(a: &Value, color: bool) -> String {
    let status = if a["acknowledged_at"].is_null() {
        "pending"
    } else {
        "acknowledged"
    };
    let event = text(&a["key"], "event_kind");
    let mut out = vec![
        format!(
            "{}  {}",
            bold(text(a, "title"), color),
            state(status, color)
        ),
        String::new(),
        format!("Pull request    {}", bold(pr(&a["subject"]), color)),
        format!("Rule            {}", rule_name(a)),
        format!("Event           {}", state(&event_label(event), color)),
        format!(
            "Detected        {}",
            dim(timestamp(&a["created_at"]), color)
        ),
        format!(
            "Revision        {}",
            dim(text(&a["key"], "revision"), color)
        ),
        String::new(),
        text(a, "body").to_owned(),
        String::new(),
    ];
    if a["source_url"].is_null() {
        out.push(dim(text(&a["subject"], "canonical_url"), color));
    } else {
        out.push(dim(text(&a["subject"], "canonical_url"), color));
        out.push(format!(
            "Source          {}",
            dim(text(a, "source_url"), color)
        ));
    }
    out.push(dim(id(&a["id"]), color));
    out.join("\n")
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = env::args_os().collect::<Vec<_>>();
    let json_requested = args.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return ExitCode::SUCCESS;
            }
            let is_auth_set =
                args.iter().any(|arg| arg == "auth") && args.iter().any(|arg| arg == "set");
            if json_requested {
                println!(
                    "{}",
                    json!({"schema_version":1,"command":"unknown","outcome":"failure","data":{},"errors":[{"message":"invalid command input"}]})
                );
            } else if is_auth_set {
                eprintln!("airborne: invalid auth set invocation");
            } else {
                let _ = error.print();
            }
            return ExitCode::from(INPUT);
        }
    };
    let json_output = cli.json;
    let command_name = command_name(&cli.command);
    match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_output && error.code != PARTIAL && error.message != "refresh failed" {
                println!(
                    "{}",
                    json!({"schema_version":1,"command":command_name,"outcome":"failure","data":{},"errors":[{"message":error.message}]})
                );
            } else {
                eprintln!("airborne: {}", error.message);
            }
            ExitCode::from(error.code)
        }
    }
}
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Watch(_) => "watch",
        Command::Rule(_) => "rule",
        Command::Preset(_) => "preset",
        Command::Refresh(_) => "refresh",
        Command::Run(_) => "run",
        Command::Status(_) => "status",
        Command::Alerts(_) => "alerts",
        Command::Auth(_) => "auth",
        Command::Config(_) => "config",
        Command::Doctor(_) => "doctor",
        Command::Migrate(_) => "migrate",
    }
}
async fn execute(cli: Cli) -> Result<(), Error> {
    let dir = data_dir(cli.data_dir)?;
    let store = Arc::new(
        SqliteStore::open(dir.join("airborne.sqlite3")).map_err(|e| Error::fail(e.to_string()))?,
    );
    secure_private_files(&dir);
    let out = Out {
        json: cli.json,
        quiet: cli.quiet,
        color: !cli.json
            && !cli.no_color
            && env::var_os("NO_COLOR").is_none()
            && io::stdout().is_terminal(),
    };
    match cli.command {
        Command::Watch(x) => watch(x.command, &store, &out).await,
        Command::Rule(x) => rule(x.command, &store, &out).await,
        Command::Preset(x) => preset(x.command, &store, &out).await,
        Command::Refresh(x) => refresh(x, &store, &out).await,
        Command::Run(x) => run(x, &store, &out).await,
        Command::Status(x) => status(x, &store, &out),
        Command::Alerts(x) => alerts(x.command, &store, &out).await,
        Command::Auth(x) => auth(x.command, &out),
        Command::Config(x) => config(x.command, &dir, &out),
        Command::Doctor(x) => doctor(x, &dir, &store, &out).await,
        Command::Migrate(x) => migrate(x.command, &dir, &out),
    }
}
fn secure_private_files(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in [
            "airborne.sqlite3",
            "airborne.sqlite3-wal",
            "airborne.sqlite3-shm",
        ] {
            let path = dir.join(name);
            if path.exists() {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            }
        }
    }
}
fn data_dir(flag: Option<PathBuf>) -> Result<PathBuf, Error> {
    let dir = flag
        .or_else(|| env::var_os("AIRBORNE_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Library/Application Support/Airborne")
        });
    fs::create_dir_all(&dir)
        .map_err(|e| Error::fail(format!("could not create data directory: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::fail(format!("could not secure data directory: {e}")))?;
    }
    Ok(dir)
}
fn now() -> Timestamp {
    Timestamp::from_datetime(Utc::now())
}
fn watch_id(value: String) -> Result<WatchId, Error> {
    WatchId::new(value).map_err(|e| Error::input(e.to_string()))
}
async fn resolve_preset_reference(store: &SqliteStore, value: &str) -> Result<PresetId, Error> {
    if let Ok(id) = PresetId::new(value.to_owned()) {
        if store
            .get_preset(id.clone())
            .await
            .map_err(store_err)?
            .is_some()
        {
            return Ok(id);
        }
    }
    let matches = store
        .list_presets(true)
        .await
        .map_err(store_err)?
        .into_iter()
        .filter(|preset| preset.name == value && preset.archived_at.is_none())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [preset] => Ok(preset.id.clone()),
        [] => Err(Error::fail(format!("preset `{value}` was not found"))),
        _ => Err(Error::input(format!(
            "preset name `{value}` matches more than one preset; use the full preset ID"
        ))),
    }
}

fn preset_rule_matches(config: &RuleConfig, value: &str) -> bool {
    match config {
        RuleConfig::GitHubCheckCompletes { check_name, .. } => {
            value == check_name || value.eq_ignore_ascii_case("bugbot")
        }
        RuleConfig::BuildkiteJobCompletes {
            github_status_context,
            job_name,
            ..
        } => value == github_status_context.as_str() || value == job_name.as_str(),
    }
}

async fn resolve_preset_rule(
    store: &SqliteStore,
    preset_id: PresetId,
    value: &str,
) -> Result<airborne_core::PresetRule, Error> {
    let rules = store
        .list_preset_rules(preset_id)
        .await
        .map_err(store_err)?;
    if let Ok(id) = PresetRuleId::new(value.to_owned()) {
        if let Some(rule) = rules.iter().find(|rule| rule.id == id) {
            return Ok(rule.clone());
        }
    }
    let matches = rules
        .into_iter()
        .filter(|rule| preset_rule_matches(&rule.config, value))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [rule] => Ok(rule.clone()),
        [] => Err(Error::fail(format!("rule `{value}` was not found"))),
        _ => Err(Error::input(format!(
            "rule name `{value}` matches more than one rule; use a more specific name"
        ))),
    }
}
fn resolve_watch_reference(store: &SqliteStore, value: &str) -> Result<WatchId, Error> {
    if value.starts_with("https://") {
        let pull_request = parse_pull_request_url(value).map_err(|_| {
            Error::input("expected a watch ID or canonical HTTPS GitHub pull request URL")
        })?;
        let subject_key = SubjectKey::new(format!(
            "github.com/{}/{}/pull/{}",
            pull_request.repository.owner, pull_request.repository.repository, pull_request.number
        ))
        .map_err(|error| Error::input(error.to_string()))?;
        return store
            .get_watch_id_by_subject_key(&subject_key)
            .map_err(|error| Error::fail(error.to_string()))?
            .ok_or_else(|| Error::fail("watch was not found"));
    }
    watch_id(value.to_owned())
}

fn resolve_rule_reference(
    store: &SqliteStore,
    value: &str,
    watch: Option<&str>,
) -> Result<RuleId, Error> {
    if let Ok(id) = RuleId::new(value.to_owned()) {
        if store
            .get_rule(&id)
            .map_err(|error| Error::fail(error.to_string()))?
            .is_some()
        {
            return Ok(id);
        }
    }

    let watch_id = watch
        .map(|reference| resolve_watch_reference(store, reference))
        .transpose()?;
    let rules = store
        .list_rule_views(watch_id.as_ref())
        .map_err(|error| Error::fail(error.to_string()))?;
    let matches = rules
        .into_iter()
        .filter(|rule| match &rule.definition.config {
            RuleConfig::GitHubCheckCompletes { check_name, .. } => {
                value == check_name || value.eq_ignore_ascii_case("bugbot")
            }
            RuleConfig::BuildkiteJobCompletes {
                github_status_context,
                job_name,
                ..
            } => value == github_status_context.as_str() || value == job_name.as_str(),
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(Error::fail(format!("rule `{value}` was not found")));
    }
    let current = matches
        .iter()
        .filter(|rule| rule.rule.archived_at.is_none())
        .collect::<Vec<_>>();
    if current.len() == 1 {
        return Ok(current[0].rule.id.clone());
    }
    if current.len() > 1 || matches.len() > 1 {
        let help = if watch.is_some() {
            "use the full rule ID"
        } else {
            "add --watch <GITHUB_PR_URL>"
        };
        return Err(Error::input(format!(
            "rule name `{value}` matches more than one rule; {help}"
        )));
    }
    Ok(matches[0].rule.id.clone())
}
#[derive(Clone, Copy)]
enum CredentialSource {
    Environment,
    #[cfg(debug_assertions)]
    Dotenv,
    #[cfg(not(debug_assertions))]
    Keychain,
    Missing,
}
impl CredentialSource {
    const fn name(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            #[cfg(debug_assertions)]
            Self::Dotenv => ".env",
            #[cfg(not(debug_assertions))]
            Self::Keychain => "keychain",
            Self::Missing => "missing",
        }
    }
}
struct CredentialResolution {
    secret: Option<SecretString>,
    source: CredentialSource,
}

fn need(provider: ProviderCredential) -> Result<SecretString, Error> {
    let resolution = resolve_credential(provider)?;
    if let Some(value) = resolution.secret {
        return Ok(value);
    }
    #[cfg(debug_assertions)]
    return Err(Error {
        code: CREDENTIAL,
        message: format!(
            "{} credential is missing from the environment or .env",
            provider.account_name()
        ),
    });
    #[cfg(not(debug_assertions))]
    MacosCredentialStore.get(provider).map_err(credential_error)
}

fn process_credential(provider: ProviderCredential) -> Option<SecretString> {
    env::var_os(credential_environment_name(provider))
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_string_lossy().into_owned()))
}

/// Debug builds read shell credentials and the exact `./.env` path; they never consult Keychain.
#[cfg(debug_assertions)]
fn resolve_credential(provider: ProviderCredential) -> Result<CredentialResolution, Error> {
    if let Some(secret) = process_credential(provider) {
        return Ok(CredentialResolution {
            secret: Some(secret),
            source: CredentialSource::Environment,
        });
    }
    let dotenv = dotenv_credentials()?;
    let secret = match provider {
        ProviderCredential::GitHub => dotenv.github,
        ProviderCredential::Buildkite => dotenv.buildkite,
    };
    Ok(CredentialResolution {
        source: if secret.is_some() {
            CredentialSource::Dotenv
        } else {
            CredentialSource::Missing
        },
        secret,
    })
}

#[cfg(debug_assertions)]
struct DotenvCredentials {
    github: Option<SecretString>,
    buildkite: Option<SecretString>,
}

#[cfg(debug_assertions)]
fn dotenv_credentials() -> Result<DotenvCredentials, Error> {
    let entries = match dotenvy::from_path_iter(".env") {
        Ok(entries) => entries,
        Err(error) if error.not_found() => {
            return Ok(DotenvCredentials {
                github: None,
                buildkite: None,
            });
        }
        Err(_) => return Err(Error::fail("could not read .env credentials")),
    };
    let mut github = None;
    let mut buildkite = None;
    let mut saw_github = false;
    let mut saw_buildkite = false;
    for entry in entries {
        let (key, candidate) =
            entry.map_err(|_| Error::fail("could not parse .env credentials"))?;
        match key.as_str() {
            "AIRBORNE_GITHUB_TOKEN" => {
                if saw_github {
                    return Err(Error::fail("invalid .env credential configuration"));
                }
                saw_github = true;
                if !candidate.is_empty() {
                    github = Some(SecretString::from(candidate));
                }
            }
            "AIRBORNE_BUILDKITE_TOKEN" => {
                if saw_buildkite {
                    return Err(Error::fail("invalid .env credential configuration"));
                }
                saw_buildkite = true;
                if !candidate.is_empty() {
                    buildkite = Some(SecretString::from(candidate));
                }
            }
            _ => {}
        }
    }
    Ok(DotenvCredentials { github, buildkite })
}

#[cfg(not(debug_assertions))]
fn resolve_credential(provider: ProviderCredential) -> Result<CredentialResolution, Error> {
    if let Some(secret) = process_credential(provider) {
        return Ok(CredentialResolution {
            secret: Some(secret),
            source: CredentialSource::Environment,
        });
    }
    let source = match MacosCredentialStore
        .presence(provider)
        .map_err(credential_error)?
    {
        CredentialPresence::Present => CredentialSource::Keychain,
        CredentialPresence::Missing => CredentialSource::Missing,
    };
    Ok(CredentialResolution {
        secret: None,
        source,
    })
}

fn credential_environment_name(provider: ProviderCredential) -> &'static str {
    match provider {
        ProviderCredential::GitHub => "AIRBORNE_GITHUB_TOKEN",
        ProviderCredential::Buildkite => "AIRBORNE_BUILDKITE_TOKEN",
    }
}
fn credential_error(error: CredentialError) -> Error {
    Error {
        code: if matches!(error, CredentialError::Missing { .. }) {
            CREDENTIAL
        } else {
            FAILURE
        },
        message: error.to_string(),
    }
}
fn source(provider: ProviderCredential) -> Result<&'static str, Error> {
    Ok(resolve_credential(provider)?.source.name())
}

async fn watch(cmd: WatchSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let WatchSub::List { active, all } = &cmd {
        let mut watches = store
            .list_watch_views(None)
            .map_err(|e| Error::fail(e.to_string()))?;
        if *active {
            watches.retain(|watch| watch.watch.state == WatchState::Active);
        } else if !*all {
            watches.retain(|watch| watch.watch.state != WatchState::Archived);
        }
        return out.emit("watch.list", json!({"watches": watches}));
    }
    if let WatchSub::Show { watch_id: id } = &cmd {
        let id = resolve_watch_reference(store, id)?;
        let watch = store
            .get_watch_view(&id)
            .map_err(|e| Error::fail(e.to_string()))?
            .ok_or_else(|| Error::fail("watch was not found"))?;
        return out.emit("watch.show", json!(watch));
    }
    if let WatchSub::Remove { watch_id: id, yes } = &cmd {
        let id = resolve_watch_reference(store, id)?;
        confirm(*yes, "archive this watch")?;
        store
            .archive_watch(&id, &now())
            .map_err(|e| Error::fail(e.to_string()))?;
        return out.emit("watch.remove", json!({"archived": true}));
    }
    match cmd { WatchSub::Add{github_pr_url,preset,paused}=>{let key=parse_pull_request_url(&github_pr_url).map_err(|_|Error::input("expected a canonical HTTPS GitHub pull request URL"))?;let token=need(ProviderCredential::GitHub)?;let api=ReqwestGitHubApi::new(token.expose_secret().to_owned()).map_err(|e|Error::fail(e.to_string()))?;let pr=api.pull_request(&key).await.map_err(|e|Error{code:if matches!(e,airborne_github::GitHubError::AuthenticationRejected){CREDENTIAL}else{FAILURE},message:e.to_string()})?;let subject=Subject{key:SubjectKey::new(format!("github.com/{}/{}/pull/{}",key.repository.owner,key.repository.repository,key.number)).map_err(|e|Error::input(e.to_string()))?,kind:SubjectKind::GitHubPullRequest,canonical_url:github_pr_url,display_title:pr.title,current_revision:Some(pr.head_revision),metadata_refreshed_at:Some(now()),created_at:now()};let draft=NewWatch{subject,state:if paused{WatchState::Paused}else{WatchState::Active}};let value=match preset {Some(reference)=>store.add_watch_from_preset(resolve_preset_reference(store,&reference).await?,draft).await, None=>store.add_watch(draft).await}.map_err(store_err)?;out.emit("watch.add",json!(value))},WatchSub::List{active,..}=>out.emit("watch.list",json!({"watches":store.list_watches(active.then_some(WatchState::Active)).await.map_err(store_err)?})),WatchSub::Pause{watch_id:id}=>set_watch(store,out,id,WatchState::Paused,"watch.pause").await,WatchSub::Resume{watch_id:id}=>set_watch(store,out,id,WatchState::Active,"watch.resume").await,WatchSub::Remove{watch_id:id,yes}=>{confirm(yes,"archive this watch")?;set_watch(store,out,id,WatchState::Archived,"watch.remove").await},WatchSub::Show{..}=>Err(Error::fail("watch show requires the status repository"))}
}
async fn set_watch(
    store: &Arc<SqliteStore>,
    out: &Out,
    id: String,
    state: WatchState,
    command: &str,
) -> Result<(), Error> {
    out.emit(
        command,
        json!(store
            .change_watch_state(watch_id(id)?, state, now())
            .await
            .map_err(store_err)?),
    )
}
async fn preset(cmd: PresetSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    match cmd {
        PresetSub::Add { name, description } => out.emit(
            "preset.add",
            json!(store
                .add_preset(NewPreset {
                    name,
                    description,
                    created_at: now(),
                })
                .await
                .map_err(store_err)?),
        ),
        PresetSub::List { all } => {
            let presets = store.list_presets(all).await.map_err(store_err)?;
            out.emit("preset.list", json!({"presets": presets}))
        }
        PresetSub::Show {
            preset_id: reference,
        } => {
            let id = resolve_preset_reference(store, &reference).await?;
            let preset = store
                .get_preset(id.clone())
                .await
                .map_err(store_err)?
                .ok_or_else(|| Error::fail("preset was not found"))?;
            let rules = store.list_preset_rules(id).await.map_err(store_err)?;
            out.emit("preset.show", json!({"preset": preset, "rules": rules}))
        }
        PresetSub::Rename {
            preset_id: reference,
            name,
        } => out.emit(
            "preset.rename",
            json!(store
                .rename_preset(
                    resolve_preset_reference(store, &reference).await?,
                    name,
                    now()
                )
                .await
                .map_err(store_err)?),
        ),
        PresetSub::Remove {
            preset_id: reference,
            yes,
        } => {
            confirm(yes, "archive this preset")?;
            let preset = store
                .archive_preset(resolve_preset_reference(store, &reference).await?, now())
                .await
                .map_err(store_err)?;
            out.emit("preset.remove", json!(preset))
        }
    }
}
async fn rule(cmd: RuleSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let RuleSub::List {
        watch,
        preset,
        enabled,
        all,
    } = &cmd
    {
        if let Some(reference) = preset {
            if *enabled || *all {
                return Err(Error::input(
                    "--enabled and --all apply only to watch rules; preset rules have no runtime state",
                ));
            }
            let rules = store
                .list_preset_rules(resolve_preset_reference(store, reference).await?)
                .await
                .map_err(store_err)?;
            return out.emit("rule.list", json!({"rules": rules}));
        }
        let watch = watch
            .as_deref()
            .map(|reference| resolve_watch_reference(store, reference))
            .transpose()?;
        let mut rules = store
            .list_rule_views(watch.as_ref())
            .map_err(|e| Error::fail(e.to_string()))?;
        if *enabled || !*all {
            rules.retain(|rule| rule.rule.enabled);
        }
        return out.emit("rule.list", json!({"rules":rules}));
    }
    match cmd {
        RuleSub::Add { kind } => {
            let (target, config, replace) = match kind {
                RuleAdd::Bugbot {
                    target,
                    replace,
                    check_name,
                    alert_on_start,
                    alert_if_missing_after,
                } => (
                    target,
                    RuleConfig::GitHubCheckCompletes {
                        check_name,
                        alert_on_start,
                        alert_if_missing_after_seconds: alert_if_missing_after
                            .map(|duration| duration.as_secs()),
                    },
                    replace,
                ),
                RuleAdd::BuildkiteJob(a) => (
                    a.target,
                    buildkite(a.context, a.organization, a.pipeline, a.job, a.notify_on)?,
                    a.replace,
                ),
            };
            let target_preset = target.preset;
            let target_watch = target.watch;
            if let Some(reference) = target_preset {
                let preset_id = resolve_preset_reference(store, &reference).await?;
                let existing = store
                    .list_preset_rules(preset_id.clone())
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .find(|rule| rule.config.kind() == config.kind());
                if existing.is_some() && !replace {
                    return Err(Error::input(format!(
                        "this preset already has a {} rule; pass --replace to replace it",
                        rule_kind_name(config.kind()),
                    )));
                }
                let result = if let Some(existing) = existing {
                    store
                        .update_preset_rule(PresetRuleChange {
                            id: existing.id,
                            config,
                            at: now(),
                        })
                        .await
                } else {
                    store.add_preset_rule(preset_id, config, now()).await
                }
                .map_err(store_err)?;
                return out.emit("rule.add", json!(result));
            }
            let watch_id = resolve_watch_reference(
                store,
                &target_watch.expect("clap requires --watch or --preset"),
            )?;
            if let Some(existing) = store
                .list_rules(Some(&watch_id))
                .map_err(|e| Error::fail(e.to_string()))?
                .into_iter()
                .find(|rule| rule.kind == config.kind() && rule.archived_at.is_none())
            {
                if !replace {
                    return Err(Error::input(format!(
                        "this watch already has a {} rule ({}){}; pass --replace to replace it",
                        rule_kind_name(config.kind()),
                        existing.id,
                        if existing.enabled {
                            ""
                        } else {
                            ", currently disabled"
                        },
                    )));
                }
            }
            let draft = NewRule {
                watch_id,
                config,
                enabled: true,
                created_at: now(),
            };
            out.emit(
                "rule.add",
                json!(if replace {
                    store.replace_rule(draft).await
                } else {
                    store.add_rule(draft).await
                }
                .map_err(rule_add_err)?),
            )
        }
        RuleSub::Enable { rule_id: id, watch } => {
            set_rule(store, out, id, watch, true, "rule.enable").await
        }
        RuleSub::Disable { rule_id: id, watch } => {
            set_rule(store, out, id, watch, false, "rule.disable").await
        }
        RuleSub::Apply {
            preset,
            watch,
            replace,
        } => {
            let rules = store
                .apply_preset(
                    resolve_preset_reference(store, &preset).await?,
                    resolve_watch_reference(store, &watch)?,
                    replace,
                    now(),
                )
                .await
                .map_err(rule_add_err)?;
            out.emit("rule.apply", json!({"rules": rules}))
        }
        RuleSub::Remove {
            rule_id: id,
            watch,
            preset,
            yes,
        } => {
            if let Some(reference) = preset {
                let rule = resolve_preset_rule(
                    store,
                    resolve_preset_reference(store, &reference).await?,
                    &id,
                )
                .await?;
                confirm(yes, "remove this preset rule")?;
                store
                    .remove_preset_rule(rule.id, now())
                    .await
                    .map_err(store_err)?;
                return out.emit("rule.remove", json!({"removed": true}));
            }
            let id = resolve_rule_reference(store, &id, watch.as_deref())?;
            confirm(yes, "archive this rule")?;
            store
                .archive_rule(&id, &now())
                .map_err(|e| Error::fail(e.to_string()))?;
            out.emit("rule.remove", json!({"archived": true}))
        }
        RuleSub::List { watch, .. } => {
            let watch = watch
                .as_deref()
                .map(|reference| resolve_watch_reference(store, reference))
                .transpose()?;
            out.emit("rule.list", json!({"rules":store.list_rules(watch.as_ref()).map_err(|e|Error::fail(e.to_string()))?}))
        }
        RuleSub::Show {
            rule_id: id,
            watch,
            preset,
        } => {
            if let Some(reference) = preset {
                let rule = resolve_preset_rule(
                    store,
                    resolve_preset_reference(store, &reference).await?,
                    &id,
                )
                .await?;
                return out.emit("rule.show", json!(rule));
            }
            let id = resolve_rule_reference(store, &id, watch.as_deref())?;
            out.emit(
                "rule.show",
                json!(store
                    .get_rule_view(&id)
                    .map_err(|e| Error::fail(e.to_string()))?
                    .ok_or_else(|| Error::fail("rule was not found"))?),
            )
        }
        RuleSub::Update(update) => {
            if update.check_name.is_none()
                && update.context.is_none()
                && update.organization.is_none()
                && update.pipeline.is_none()
                && update.job.is_none()
                && update.notify_on.is_none()
                && !update.alert_on_start
                && !update.no_alert_on_start
                && update.alert_if_missing_after.is_none()
                && !update.no_alert_if_missing_after
            {
                return Err(Error::input(
                    "rule update requires at least one changed option",
                ));
            }
            let preset_rule = if let Some(reference) = &update.preset {
                Some(
                    resolve_preset_rule(
                        store,
                        resolve_preset_reference(store, reference).await?,
                        &update.rule_id,
                    )
                    .await?,
                )
            } else {
                None
            };
            let id = if preset_rule.is_none() {
                Some(resolve_rule_reference(
                    store,
                    &update.rule_id,
                    update.watch.as_deref(),
                )?)
            } else {
                None
            };
            let current_config = match &preset_rule {
                Some(rule) => rule.config.clone(),
                None => {
                    store
                        .get_rule_definition(id.as_ref().expect("runtime rule ID"))
                        .map_err(|e| Error::fail(e.to_string()))?
                        .ok_or_else(|| Error::fail("rule was not found"))?
                        .config
                }
            };
            let config = match current_config {
                RuleConfig::GitHubCheckCompletes {
                    check_name,
                    alert_on_start,
                    alert_if_missing_after_seconds,
                } => {
                    if update.context.is_some()
                        || update.organization.is_some()
                        || update.pipeline.is_some()
                        || update.job.is_some()
                        || update.notify_on.is_some()
                    {
                        return Err(Error::input(
                            "buildkite options cannot update a bugbot rule",
                        ));
                    }
                    RuleConfig::GitHubCheckCompletes {
                        check_name: update.check_name.unwrap_or(check_name),
                        alert_on_start: if update.alert_on_start {
                            true
                        } else if update.no_alert_on_start {
                            false
                        } else {
                            alert_on_start
                        },
                        alert_if_missing_after_seconds: if update.no_alert_if_missing_after {
                            None
                        } else {
                            update
                                .alert_if_missing_after
                                .map(|duration| duration.as_secs())
                                .or(alert_if_missing_after_seconds)
                        },
                    }
                }
                RuleConfig::BuildkiteJobCompletes {
                    github_status_context,
                    expected_organization,
                    expected_pipeline,
                    job_name,
                    notify_on,
                } => {
                    if update.check_name.is_some() {
                        return Err(Error::input("--check-name cannot update a Buildkite rule"));
                    }
                    if update.alert_on_start
                        || update.no_alert_on_start
                        || update.alert_if_missing_after.is_some()
                        || update.no_alert_if_missing_after
                    {
                        return Err(Error::input(
                            "Bugbot alert options cannot update a Buildkite rule",
                        ));
                    }
                    buildkite(
                        update
                            .context
                            .unwrap_or_else(|| github_status_context.into_inner()),
                        update
                            .organization
                            .unwrap_or_else(|| expected_organization.into_inner()),
                        update
                            .pipeline
                            .unwrap_or_else(|| expected_pipeline.into_inner()),
                        update.job.unwrap_or_else(|| job_name.into_inner()),
                        update.notify_on.unwrap_or(match notify_on {
                            BuildkiteNotifyOn::Terminal => Notify::Terminal,
                            BuildkiteNotifyOn::Passed => Notify::Passed,
                        }),
                    )?
                }
            };
            if let Some(rule) = preset_rule {
                out.emit(
                    "rule.update",
                    json!(store
                        .update_preset_rule(PresetRuleChange {
                            id: rule.id,
                            config,
                            at: now(),
                        })
                        .await
                        .map_err(store_err)?),
                )
            } else {
                out.emit(
                    "rule.update",
                    json!(store
                        .update_rule(RuleChange {
                            id: id.expect("runtime rule ID"),
                            config,
                            at: now()
                        })
                        .await
                        .map_err(store_err)?),
                )
            }
        }
    }
}
fn buildkite(
    context: String,
    organization: String,
    pipeline: String,
    job: String,
    notify: Notify,
) -> Result<RuleConfig, Error> {
    Ok(RuleConfig::BuildkiteJobCompletes {
        github_status_context: GitHubStatusContext::new(context)
            .map_err(|e| Error::input(e.to_string()))?,
        expected_organization: BuildkiteOrganization::new(organization)
            .map_err(|e| Error::input(e.to_string()))?,
        expected_pipeline: BuildkitePipeline::new(pipeline)
            .map_err(|e| Error::input(e.to_string()))?,
        job_name: BuildkiteJobName::new(job).map_err(|e| Error::input(e.to_string()))?,
        notify_on: match notify {
            Notify::Terminal => BuildkiteNotifyOn::Terminal,
            Notify::Passed => BuildkiteNotifyOn::Passed,
        },
    })
}
async fn set_rule(
    store: &Arc<SqliteStore>,
    out: &Out,
    id: String,
    watch: Option<String>,
    on: bool,
    command: &str,
) -> Result<(), Error> {
    let id = resolve_rule_reference(store, &id, watch.as_deref())?;
    out.emit(
        command,
        json!(store
            .change_rule_state(id, on, now())
            .await
            .map_err(store_err)?),
    )
}

async fn make_runtime(store: &Arc<SqliteStore>) -> Result<Runtime, Error> {
    let targets = store
        .load_refresh_targets(RefreshScope::AllActive)
        .await
        .map_err(store_err)?;
    let github = if targets.is_empty() {
        SecretString::from("unused")
    } else {
        need(ProviderCredential::GitHub)?
    };
    let uses_buildkite = targets.iter().flat_map(|target| &target.rules).any(|rule| {
        matches!(
            rule.definition.config,
            RuleConfig::BuildkiteJobCompletes { .. }
        )
    });
    let buildkite: Arc<dyn BuildkiteApi> = if uses_buildkite {
        match need(ProviderCredential::Buildkite) {
            Ok(token) => Arc::new(
                ReqwestBuildkiteClient::new(token.expose_secret().to_owned())
                    .map_err(|e| Error::fail(e.to_string()))?,
            ),
            Err(error) if error.code == CREDENTIAL => Arc::new(MissingBuildkite),
            Err(error) => return Err(error),
        }
    } else {
        Arc::new(MissingBuildkite)
    };
    let github = Arc::new(
        ReqwestGitHubApi::new(github.expose_secret().to_owned())
            .map_err(|e| Error::fail(e.to_string()))?,
    );
    Ok(Runtime::new(
        store.clone(),
        store.clone(),
        Arc::new(SystemClock),
        vec![Arc::new(GitHubPullRequestMonitor::new(github, buildkite))],
    ))
}
async fn refresh(args: Refresh, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    let scope = args
        .watch_id
        .map(watch_id)
        .transpose()?
        .map_or(RefreshScope::AllActive, RefreshScope::Watch);
    let report = make_runtime(store)
        .await?
        .refresh(scope, args.wait, &NeverCancelled)
        .await
        .map_err(runtime_err)?;
    notify_alerts(&report);
    show_refresh("refresh", &report, out)
}
async fn run(args: Run, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    let interval = match args.interval {
        Some(interval) => interval,
        None => store
            .setting("poll-interval")
            .map_err(|e| Error::fail(e.to_string()))?
            .map(|value| parse_duration(&value).map_err(Error::input))
            .transpose()?
            .unwrap_or(Duration::from_secs(60)),
    };
    if !(Duration::from_secs(30)..=Duration::from_secs(86400)).contains(&interval) {
        return Err(Error::input(
            "--interval must be between 30 seconds and 24 hours",
        ));
    }
    let cancel = Arc::new(Cancel::new());
    let c = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        let signal = async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM listener");
            tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
        };
        #[cfg(not(unix))]
        let signal = tokio::signal::ctrl_c();
        let _ = signal.await;
        c.cancel();
    });
    let mut sequence = 0_u64;
    let mut handler = |r: &RefreshReport| {
        sequence += 1;
        notify_alerts(r);
        if out.json {
            let outcome = match r.outcome {
                airborne_core::PollOutcome::Success => "success",
                airborne_core::PollOutcome::Partial => "partial",
                _ => "failure",
            };
            println!(
                "{}",
                json!({"schema_version":1,"command":"run","event":"refresh","sequence":sequence,"outcome":outcome,"data":refresh_data(r)})
            );
        } else {
            let _ = show_refresh("run", r, out);
        }
    };
    make_runtime(store)
        .await?
        .run(interval, &TokioSleep, cancel.as_ref(), &mut handler)
        .await
        .map_err(runtime_err)
}
fn notify_alerts(report: &RefreshReport) {
    for alert in report.new_alerts() {
        if let Err(error) = send_notification(&alert.title, &alert.body) {
            eprintln!(
                "airborne: could not send system notification for alert {}: {error}",
                alert.id
            );
        }
    }
}
fn send_notification(summary: &str, body: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    prepare_macos_notifications()?;
    Notification::new()
        .appname("Airborne")
        .summary(summary)
        .body(body)
        .show()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
#[cfg(target_os = "macos")]
fn prepare_macos_notifications() -> Result<(), String> {
    use std::sync::OnceLock;

    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            notify_rust::set_application("com.apple.Terminal").map_err(|error| error.to_string())
        })
        .clone()
}
fn show_refresh(command: &str, r: &RefreshReport, out: &Out) -> Result<(), Error> {
    let data = refresh_data(r);
    if out.json {
        println!("{}", refresh_json(command, r));
    } else {
        out.emit(command, data)?;
    }
    match refresh_exit(r.outcome) {
        0 => Ok(()),
        PARTIAL => Err(Error {
            code: PARTIAL,
            message: "refresh completed with source failures".into(),
        }),
        _ => Err(Error::fail("refresh failed")),
    }
}
fn refresh_exit(outcome: airborne_core::PollOutcome) -> u8 {
    match outcome {
        airborne_core::PollOutcome::Success => 0,
        airborne_core::PollOutcome::Partial => PARTIAL,
        airborne_core::PollOutcome::Failure | airborne_core::PollOutcome::Canceled => FAILURE,
    }
}
fn refresh_json(command: &str, report: &RefreshReport) -> String {
    let outcome = match report.outcome {
        airborne_core::PollOutcome::Success => "success",
        airborne_core::PollOutcome::Partial => "partial",
        _ => "failure",
    };
    json!({"schema_version":1,"command":command,"outcome":outcome,"data":refresh_data(report)})
        .to_string()
}
fn refresh_data(r: &RefreshReport) -> Value {
    json!({"refresh_id":r.refresh_id,"started_at":r.started_at,"finished_at":r.finished_at,"outcome":r.outcome,"subjects":r.subjects.iter().map(|x|json!({"watch_id":x.watch_id,"subject_key":x.subject_key,"outcome":x.outcome,"observations":x.observations,"issues":x.issues,"new_alerts":x.new_alerts})).collect::<Vec<_>>()})
}
fn status(args: Status, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    let watch = args.watch.map(watch_id).transpose()?;
    let status = store
        .status(watch.as_ref())
        .map_err(|e| Error::fail(e.to_string()))?;
    if watch.is_some() && status.watches.is_empty() {
        return Err(Error::fail("watch was not found"));
    }
    let mut data = json!(status);
    for watch in data["watches"].as_array_mut().into_iter().flatten() {
        for rule in watch["rules"].as_array_mut().into_iter().flatten() {
            let config = &rule["definition"]["config"];
            if text(config, "kind") == "git_hub_check_completes" {
                rule["alert_policy"] = json!({
                    "alert_on_start": config["alert_on_start"].as_bool().unwrap_or(false),
                    "alert_if_missing_after_seconds": config["alert_if_missing_after_seconds"],
                });
            }
        }
    }
    out.emit("status", data)
}
async fn alerts(cmd: AlertsSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let AlertsSub::Show { alert_id } = &cmd {
        let id = airborne_core::AlertId::new(alert_id.clone())
            .map_err(|e| Error::input(e.to_string()))?;
        let alert = store
            .get_alert_view(&id)
            .map_err(|e| Error::fail(e.to_string()))?
            .ok_or_else(|| Error::fail("alert was not found"))?;
        return out.emit("alerts.show", json!(alert));
    }
    match cmd {
        AlertsSub::List { all, watch, .. } => {
            let watch = watch.map(watch_id).transpose()?;
            out.emit("alerts.list",json!({"alerts":store.list_alert_views(!all,watch.as_ref()).map_err(|e|Error::fail(e.to_string()))?,"mode":if all { "all" } else { "pending" }}))
        }
        AlertsSub::Acknowledge { alert_id, all, yes } => {
            if all {
                confirm(yes, "acknowledge all alerts")?;
                let ids = store
                    .list_alerts(true, None)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>();
                let n = store.acknowledge(&ids, now()).await.map_err(store_err)?;
                return out.emit("alerts.acknowledge", json!({"acknowledged":n}));
            }
            let id = alert_id.ok_or_else(|| Error::input("provide an alert ID or --all"))?;
            let n = store
                .acknowledge(
                    &[airborne_core::AlertId::new(id).map_err(|e| Error::input(e.to_string()))?],
                    now(),
                )
                .await
                .map_err(store_err)?;
            out.emit("alerts.acknowledge", json!({"acknowledged":n}))
        }
        AlertsSub::Show { .. } => Err(Error::fail("alerts show requires alert lookup")),
    }
}
fn auth(cmd: AuthSub, out: &Out) -> Result<(), Error> {
    match cmd{AuthSub::Status=>out.emit("auth.status",json!({"github":source(ProviderCredential::GitHub)?,"buildkite":source(ProviderCredential::Buildkite)?})),AuthSub::Set{provider}=>{let p=provider_to(provider);let token=read_secret()?;MacosCredentialStore.set(p,token).map_err(credential_error)?;out.emit("auth.set",json!({"provider":p.account_name(),"source":"keychain"}))},AuthSub::Remove{provider,yes}=>{confirm(yes,"remove this credential")?;let p=provider_to(provider);MacosCredentialStore.remove(p).map_err(credential_error)?;out.emit("auth.remove",json!({"provider":p.account_name()}))}}
}
fn provider_to(value: ProviderArg) -> ProviderCredential {
    match value {
        ProviderArg::Github => ProviderCredential::GitHub,
        ProviderArg::Buildkite => ProviderCredential::Buildkite,
    }
}
fn read_secret() -> Result<SecretString, Error> {
    if !io::stdin().is_terminal() {
        return Err(Error::input("auth set requires an interactive terminal"));
    }
    eprint!("Token: ");
    io::stderr()
        .flush()
        .map_err(|e| Error::fail(e.to_string()))?;
    let echo = EchoGuard::disable()?;
    let mut s = String::new();
    io::stdin()
        .read_line(&mut s)
        .map_err(|e| Error::fail(e.to_string()))?;
    drop(echo);
    eprintln!();
    let s = s.trim();
    if s.is_empty() {
        Err(Error::input("token must not be empty"))
    } else {
        Ok(SecretString::from(s.to_owned()))
    }
}
fn confirm(yes: bool, what: &str) -> Result<(), Error> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(Error::input(format!(
            "--yes is required to {what} outside an interactive terminal"
        )));
    }
    eprint!("Really {what}? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|e| Error::fail(e.to_string()))?;
    let mut s = String::new();
    io::stdin()
        .read_line(&mut s)
        .map_err(|e| Error::fail(e.to_string()))?;
    if matches!(s.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(Error::fail("operation canceled"))
    }
}
fn config(cmd: ConfigSub, dir: &Path, out: &Out) -> Result<(), Error> {
    let store =
        SqliteStore::open(dir.join("airborne.sqlite3")).map_err(|e| Error::fail(e.to_string()))?;
    match cmd {
        ConfigSub::Path => out.emit(
            "config.path",
            json!({"data_dir":dir,"database":dir.join("airborne.sqlite3")}),
        ),
        ConfigSub::Get { key } => {
            let key = key.unwrap_or_else(|| "poll-interval".into());
            out.emit("config.get", json!({"key": key, "value": store.setting(&key).map_err(|e| Error::fail(e.to_string()))?}))
        }
        ConfigSub::Set {
            key: ConfigKey::PollInterval,
            value,
        } => {
            if !(Duration::from_secs(30)..=Duration::from_secs(86_400)).contains(&value) {
                return Err(Error::input(
                    "poll-interval must be between 30 seconds and 24 hours",
                ));
            }
            let value = format!("{}s", value.as_secs());
            store
                .set_setting("poll-interval", &value, &now())
                .map_err(|e| Error::fail(e.to_string()))?;
            out.emit("config.set", json!({"key":"poll-interval", "value":value}))
        }
    }
}
async fn doctor(
    args: Doctor,
    dir: &Path,
    store: &Arc<SqliteStore>,
    out: &Out,
) -> Result<(), Error> {
    store
        .integrity_check()
        .map_err(|e| Error::fail(e.to_string()))?;
    let leases = store
        .lease_status()
        .map_err(|e| Error::fail(e.to_string()))?;
    let live = if args.live {
        let github = need(ProviderCredential::GitHub)?;
        ReqwestGitHubApi::new(github.expose_secret().to_owned())
            .map_err(|e| Error::fail(e.to_string()))?
            .check_access()
            .await
            .map_err(|error| Error {
                code: if matches!(error, airborne_github::GitHubError::AuthenticationRejected) {
                    CREDENTIAL
                } else {
                    FAILURE
                },
                message: format!("GitHub access check failed: {error}"),
            })?;
        let buildkite = need(ProviderCredential::Buildkite)?;
        ReqwestBuildkiteClient::new(buildkite.expose_secret().to_owned())
            .map_err(|e| Error::fail(e.to_string()))?
            .check_access()
            .await
            .map_err(|error| Error {
                code: if matches!(error, BuildkiteError::CredentialRejected) {
                    CREDENTIAL
                } else {
                    FAILURE
                },
                message: format!("Buildkite access check failed: {error}"),
            })?;
        let github = "ok";
        let buildkite = "ok";
        json!({"github":github,"buildkite":buildkite})
    } else {
        json!(null)
    };
    let notification_test = if args.test_notifications {
        send_notification(
            "Airborne notifications work",
            "You will get a notification when Airborne creates an alert.",
        )
        .map_err(|error| Error::fail(format!("system notification test failed: {error}")))?;
        json!("sent")
    } else {
        json!(null)
    };
    out.emit("doctor",json!({"data_dir":dir,"schema_version":store.schema_version().map_err(|e|Error::fail(e.to_string()))?,"storage":"ok","github":source(ProviderCredential::GitHub)?,"buildkite":source(ProviderCredential::Buildkite)?,"leases":leases,"live":live,"notification_test":notification_test}))
}
fn migrate(cmd: MigrateSub, dir: &Path, out: &Out) -> Result<(), Error> {
    match cmd {
        MigrateSub::Prototype {
            from,
            credentials,
            dry_run,
        } => {
            if credentials && !dry_run {
                confirm(false, "copy prototype credentials into Airborne Keychain")?;
            }
            let source = from.unwrap_or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("Library/Application Support/com.prwatcher.v0/pr-watcher.sqlite3")
            });
            if !dry_run {
                confirm(false, "import the prototype database")?;
            }
            let store = SqliteStore::open(dir.join("airborne.sqlite3"))
                .map_err(|e| Error::fail(e.to_string()))?;
            let report = store
                .import_prototype(source, dry_run)
                .map_err(|e| Error::fail(e.to_string()))?;
            let copied = if credentials && !dry_run {
                [ProviderCredential::GitHub, ProviderCredential::Buildkite]
                    .into_iter()
                    .map(|provider| {
                        MacosCredentialStore
                            .copy_legacy(provider)
                            .map_err(credential_error)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                Vec::new()
            };
            out.emit("migrate.prototype", json!({"watches":report.watches,"rules":report.rules,"observations":report.observations,"alerts":report.alerts,"settings":report.settings,"skipped":report.skipped,"repaired":report.repaired,"already_imported":report.already_imported,"credentials_copied":copied.into_iter().filter(|copied| *copied).count(),"credentials_planned":credentials && dry_run}))
        }
    }
}
fn parse_duration(s: &str) -> Result<Duration, String> {
    let i = s
        .find(|x: char| !x.is_ascii_digit())
        .ok_or_else(|| "duration needs a unit such as 30s".to_owned())?;
    let (n, u) = s.split_at(i);
    let n: u64 = n
        .parse()
        .map_err(|_| "duration must start with a whole number".to_owned())?;
    match u {
        "s" => Ok(Duration::from_secs(n)),
        "m" => n
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| "duration is too large".into()),
        "h" => n
            .checked_mul(3600)
            .map(Duration::from_secs)
            .ok_or_else(|| "duration is too large".into()),
        _ => Err("duration unit must be s, m, or h".into()),
    }
}
fn parse_positive_duration(s: &str) -> Result<Duration, String> {
    let duration = parse_duration(s)?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".into());
    }
    Ok(duration)
}
fn human_candidate_state(state: &str) -> &str {
    match state {
        "not_detected" => "not detected",
        "in_progress" => "in progress",
        other => other,
    }
}
fn store_err(e: airborne_runtime::StoreError) -> Error {
    Error::fail(e.to_string())
}
fn rule_add_err(e: airborne_runtime::StoreError) -> Error {
    match e {
        airborne_runtime::StoreError::RuleKindConflict {
            kind,
            existing_rule_id,
            ..
        } => Error::input(format!(
            "this watch already has a {} rule ({existing_rule_id}); archive it before adding a replacement",
            rule_kind_name(kind),
        )),
        error @ airborne_runtime::StoreError::Failed { .. } => Error::fail(error.to_string()),
    }
}
fn rule_kind_name(kind: airborne_core::RuleKind) -> &'static str {
    match kind {
        airborne_core::RuleKind::GitHubCheckCompletes => "Bugbot",
        airborne_core::RuleKind::BuildkiteJobCompletes => "Buildkite job",
    }
}
fn runtime_err(e: airborne_runtime::RuntimeError) -> Error {
    Error {
        code: if matches!(
            e,
            airborne_runtime::RuntimeError::Lease(airborne_runtime::LeaseError::Busy { .. })
        ) {
            LEASE
        } else {
            FAILURE
        },
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_exit_codes_follow_the_public_contract() {
        assert_eq!(refresh_exit(airborne_core::PollOutcome::Success), 0);
        assert_eq!(refresh_exit(airborne_core::PollOutcome::Partial), PARTIAL);
        assert_eq!(refresh_exit(airborne_core::PollOutcome::Failure), FAILURE);
    }

    #[test]
    fn partial_refresh_json_is_one_complete_document() {
        let report = RefreshReport {
            refresh_id: airborne_core::RefreshId::new("refresh-test").unwrap(),
            started_at: now(),
            finished_at: now(),
            outcome: airborne_core::PollOutcome::Partial,
            subjects: Vec::new(),
        };
        let output = refresh_json("refresh", &report);
        assert_eq!(output.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["outcome"],
            "partial"
        );
    }

    #[test]
    fn status_renders_watch_issues_and_one_poll_summary() {
        let data = json!({"watches":[{
            "watch":{"id":"watch-1","state":"active"},
            "subject":{"key":"github.com/owner/repo/pull/7","display_title":"A title","canonical_url":"https://github.com/owner/repo/pull/7","current_revision":"abc"},
            "pending_alert_count":0,
            "latest_issue":{"safe_message":"GitHub is unavailable"},
            "latest_poll_outcome":"partial",
            "latest_poll_finished_at":"2026-09-10T18:41:00Z",
            "rules":[{"rule":{"enabled":true,"archived_at":null,"kind":"git_hub_check_completes"},"definition":{"config":{"kind":"git_hub_check_completes","check_name":"Cursor Bugbot"}},"latest_observation":{"state":"in_progress","observed_at":"2026-09-10T18:40:00Z"}}]
        }]});
        let output = render_status(&data, false);
        assert!(output.contains("Issue: GitHub is unavailable"));
        assert!(output.contains("Checked Sep 10, 2026 at"));
        assert!(output.contains("last poll partial"));
        assert!(output.contains("observed Sep 10, 2026 at"));
        assert_eq!(output.matches("Checked ").count(), 1);
    }

    #[test]
    fn archived_rule_never_inherits_missing_duration() {
        let rule = json!({
            "rule":{"archived_at":"2026-09-10T00:00:00Z","enabled":false},
            "latest_observation":{"state":"not_detected","first_observed_at":"2020-01-01T00:00:00Z"}
        });
        assert_eq!(rule_display_state(&rule), "archived");
    }

    #[test]
    fn buildkite_rule_show_includes_organization() {
        let rule = json!({
            "id":"rule-1", "enabled":true, "archived_at":null, "current_version":1,
            "subject":{"key":"github.com/owner/repo/pull/7","display_title":"A title"},
            "definition":{"config":{"kind":"buildkite_job_completes","job_name":"test","github_status_context":"buildkite/test","expected_organization":"acme","expected_pipeline":"main"}},
            "latest_observation":null
        });
        assert!(render_rule_show(&rule, false).contains("Organization     acme"));
    }

    #[test]
    fn preset_views_render_description_archival_state_and_rule_id() {
        let preset = json!({
            "id":"preset-1", "name":"CI", "description":"Default checks",
            "archived_at":"2026-09-11T00:00:00Z"
        });
        let rule = json!({
            "id":"preset_rule-1", "config":{"kind":"git_hub_check_completes", "check_name":"Cursor Bugbot"}
        });
        assert!(
            render_preset_list(&json!({"presets":[preset.clone()]}), false).contains("archived")
        );
        let output = render_preset_show(&json!({"preset":preset, "rules":[rule]}), false);
        assert!(output.contains("Default checks"));
        assert!(output.contains("archived"));
        assert!(output.contains("preset_rule"));
    }
}
