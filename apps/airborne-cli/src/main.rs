#![allow(
    clippy::ignored_unit_patterns,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]

use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode},
    sync::Arc,
    time::Duration,
};

use airborne_buildkite::{BuildSnapshot, BuildkiteApi, BuildkiteError, ReqwestBuildkiteClient};
use airborne_core::{
    BuildkiteJobName, BuildkiteNotifyOn, BuildkiteOrganization, BuildkitePipeline,
    GitHubStatusContext, RuleConfig, RuleId, Subject, SubjectKey, SubjectKind, Timestamp, WatchId,
    WatchState,
};
use airborne_credentials_macos::{
    CredentialError, CredentialPresence, CredentialStore, ExposeSecret, MacosCredentialStore,
    ProviderCredential, SecretString,
};
use airborne_github::{parse_pull_request_url, GitHubApi, ReqwestGitHubApi};
use airborne_pr::GitHubPullRequestMonitor;
use airborne_runtime::{
    Cancellation, Clock, NeverCancelled, RefreshReport, RefreshScope, Runtime, RuntimeStore,
    Sleeper,
};
use airborne_store_sqlite::{
    AlertRepository, CatalogRepository, NewRule, NewWatch, RuleChange, SqliteStore,
};
use chrono::Utc;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
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
        github_pr_url: String,
        #[arg(long)]
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
    Show { watch_id: String },
    /// Pause a watch.
    Pause { watch_id: String },
    /// Resume a watch and reset its rule baselines.
    Resume { watch_id: String },
    /// Archive a watch.
    Remove {
        watch_id: String,
        #[arg(long)]
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
    /// List rules, optionally for one watch.
    List {
        #[arg(long, help = "Limit rules to one watch")]
        watch: Option<String>,
        #[arg(long, conflicts_with = "all", help = "Show enabled rules")]
        enabled: bool,
        #[arg(long, help = "Include disabled and archived rules")]
        all: bool,
    },
    /// Show a rule and its current version.
    Show { rule_id: String },
    /// Enable a rule and reset its baseline.
    Enable { rule_id: String },
    /// Disable a rule without deleting its history.
    Disable { rule_id: String },
    /// Update a rule's matching policy.
    Update(RuleUpdate),
    /// Archive a rule.
    Remove {
        rule_id: String,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Subcommand)]
enum RuleAdd {
    /// Alert when Cursor Bugbot completes.
    Bugbot {
        watch_id: String,
        #[arg(long, default_value=CHECK)]
        check_name: String,
    },
    #[command(name = "buildkite-job")]
    /// Alert when a Buildkite job completes.
    BuildkiteJob(BuildkiteArgs),
}
#[derive(Args)]
struct BuildkiteArgs {
    watch_id: String,
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
#[derive(Clone, Copy, ValueEnum)]
enum Notify {
    Terminal,
    Passed,
}
#[derive(Args)]
struct RuleUpdate {
    rule_id: String,
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
            println!("{}", human(command, &data));
        }
        Ok(())
    }
}
fn human(command: &str, data: &Value) -> String {
    match command {
        "watch.list" => rows(data, "watches", |item| {
            format!("{}\t{}\t{}", item["id"], item["state"], item["subject_key"])
        }),
        "rule.list" => rows(data, "rules", |item| {
            format!(
                "{}\t{}\t{}",
                item["id"],
                item["kind"],
                if item["enabled"].as_bool().unwrap_or(false) {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        }),
        "alerts.list" => rows(data, "alerts", |item| {
            format!(
                "{}\t{}\t{}",
                item["id"],
                item["title"].as_str().unwrap_or(""),
                if item["acknowledged_at"].is_null() {
                    "pending"
                } else {
                    "acknowledged"
                }
            )
        }),
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
        "status" => rows(data, "watches", |item| {
            format!(
                "{}\t{}\t{} rule(s)",
                item["watch"]["id"],
                item["subject"]["current_revision"]
                    .as_str()
                    .unwrap_or("unknown"),
                item["rules"].as_array().map_or(0, Vec::len)
            )
        }),
        _ => serde_json::to_string_pretty(data).unwrap_or_else(|_| "completed".into()),
    }
}
fn rows(data: &Value, key: &str, render: impl Fn(&Value) -> String) -> String {
    data[key]
        .as_array()
        .map(|items| {
            if items.is_empty() {
                "No results.".into()
            } else {
                items.iter().map(render).collect::<Vec<_>>().join("\n")
            }
        })
        .unwrap_or_else(|| "completed".into())
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
    };
    match cli.command {
        Command::Watch(x) => watch(x.command, &store, &out).await,
        Command::Rule(x) => rule(x.command, &store, &out).await,
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
fn rule_id(value: String) -> Result<RuleId, Error> {
    RuleId::new(value).map_err(|e| Error::input(e.to_string()))
}
fn need(provider: ProviderCredential) -> Result<SecretString, Error> {
    let name = match provider {
        ProviderCredential::GitHub => "AIRBORNE_GITHUB_TOKEN",
        ProviderCredential::Buildkite => "AIRBORNE_BUILDKITE_TOKEN",
    };
    if let Some(value) = env::var_os(name).filter(|x| !x.is_empty()) {
        return Ok(SecretString::from(value.to_string_lossy().into_owned()));
    }
    MacosCredentialStore.get(provider).map_err(credential_error)
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
fn source(provider: ProviderCredential) -> &'static str {
    let env_name = match provider {
        ProviderCredential::GitHub => "AIRBORNE_GITHUB_TOKEN",
        ProviderCredential::Buildkite => "AIRBORNE_BUILDKITE_TOKEN",
    };
    if env::var_os(env_name).is_some_and(|value| !value.is_empty()) {
        "environment"
    } else if matches!(
        MacosCredentialStore.presence(provider),
        Ok(CredentialPresence::Present)
    ) {
        "keychain"
    } else {
        "missing"
    }
}

async fn watch(cmd: WatchSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let WatchSub::List { active: _, all } = &cmd {
        let state = if *all { None } else { Some(WatchState::Active) };
        let watches = store.list_watches(state).await.map_err(store_err)?;
        return out.emit("watch.list", json!({"watches": watches}));
    }
    if let WatchSub::Show { watch_id: id } = &cmd {
        let watch = store
            .get_watch(&watch_id(id.clone())?)
            .map_err(|e| Error::fail(e.to_string()))?
            .ok_or_else(|| Error::fail("watch was not found"))?;
        return out.emit("watch.show", json!(watch));
    }
    if let WatchSub::Remove { watch_id: id, yes } = &cmd {
        confirm(*yes, "archive this watch")?;
        store
            .archive_watch(&watch_id(id.clone())?, &now())
            .map_err(|e| Error::fail(e.to_string()))?;
        return out.emit("watch.remove", json!({"archived": true}));
    }
    match cmd { WatchSub::Add{github_pr_url,paused}=>{let key=parse_pull_request_url(&github_pr_url).map_err(|_|Error::input("expected a canonical HTTPS GitHub pull request URL"))?;let token=need(ProviderCredential::GitHub)?;let api=ReqwestGitHubApi::new(token.expose_secret().to_owned()).map_err(|e|Error::fail(e.to_string()))?;let pr=api.pull_request(&key).await.map_err(|e|Error{code:if matches!(e,airborne_github::GitHubError::AuthenticationRejected){CREDENTIAL}else{FAILURE},message:e.to_string()})?;let subject=Subject{key:SubjectKey::new(format!("github.com/{}/{}/pull/{}",key.repository.owner,key.repository.repository,key.number)).map_err(|e|Error::input(e.to_string()))?,kind:SubjectKind::GitHubPullRequest,canonical_url:github_pr_url,display_title:pr.title,current_revision:Some(pr.head_revision),metadata_refreshed_at:Some(now()),created_at:now()};let value=store.add_watch(NewWatch{subject,state:if paused{WatchState::Paused}else{WatchState::Active}}).await.map_err(store_err)?;out.emit("watch.add",json!(value))},WatchSub::List{active,..}=>out.emit("watch.list",json!({"watches":store.list_watches(active.then_some(WatchState::Active)).await.map_err(store_err)?})),WatchSub::Pause{watch_id:id}=>set_watch(store,out,id,WatchState::Paused,"watch.pause").await,WatchSub::Resume{watch_id:id}=>set_watch(store,out,id,WatchState::Active,"watch.resume").await,WatchSub::Remove{watch_id:id,yes}=>{confirm(yes,"archive this watch")?;set_watch(store,out,id,WatchState::Archived,"watch.remove").await},WatchSub::Show{..}=>Err(Error::fail("watch show requires the status repository"))}
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
async fn rule(cmd: RuleSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let RuleSub::List {
        watch,
        enabled,
        all,
    } = &cmd
    {
        let watch = watch.clone().map(watch_id).transpose()?;
        let mut rules = store
            .list_rules(watch.as_ref())
            .map_err(|e| Error::fail(e.to_string()))?;
        if *enabled || !*all {
            rules.retain(|rule| rule.enabled);
        }
        return out.emit("rule.list", json!({"rules":rules}));
    }
    match cmd {
        RuleSub::Add { kind } => {
            let (watch_id, config) = match kind {
                RuleAdd::Bugbot {
                    watch_id: id,
                    check_name,
                } => (
                    watch_id(id)?,
                    RuleConfig::GitHubCheckCompletes { check_name },
                ),
                RuleAdd::BuildkiteJob(a) => (
                    watch_id(a.watch_id)?,
                    buildkite(a.context, a.organization, a.pipeline, a.job, a.notify_on)?,
                ),
            };
            out.emit(
                "rule.add",
                json!(store
                    .add_rule(NewRule {
                        watch_id,
                        config,
                        enabled: true,
                        created_at: now()
                    })
                    .await
                    .map_err(store_err)?),
            )
        }
        RuleSub::Enable { rule_id: id } => set_rule(store, out, id, true, "rule.enable").await,
        RuleSub::Disable { rule_id: id } => set_rule(store, out, id, false, "rule.disable").await,
        RuleSub::Remove { rule_id: id, yes } => {
            confirm(yes, "archive this rule")?;
            store
                .archive_rule(&rule_id(id)?, &now())
                .map_err(|e| Error::fail(e.to_string()))?;
            out.emit("rule.remove", json!({"archived": true}))
        }
        RuleSub::List { watch, .. } => {
            let watch = watch.map(watch_id).transpose()?;
            out.emit("rule.list", json!({"rules":store.list_rules(watch.as_ref()).map_err(|e|Error::fail(e.to_string()))?}))
        }
        RuleSub::Show { rule_id: id } => out.emit(
            "rule.show",
            json!(store
                .get_rule(&rule_id(id)?)
                .map_err(|e| Error::fail(e.to_string()))?
                .ok_or_else(|| Error::fail("rule was not found"))?),
        ),
        RuleSub::Update(update) => {
            if update.check_name.is_none()
                && update.context.is_none()
                && update.organization.is_none()
                && update.pipeline.is_none()
                && update.job.is_none()
                && update.notify_on.is_none()
            {
                return Err(Error::input(
                    "rule update requires at least one changed option",
                ));
            }
            let id = rule_id(update.rule_id)?;
            let definition = store
                .get_rule_definition(&id)
                .map_err(|e| Error::fail(e.to_string()))?
                .ok_or_else(|| Error::fail("rule was not found"))?;
            let config = match definition.config {
                RuleConfig::GitHubCheckCompletes { check_name } => {
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
            out.emit(
                "rule.update",
                json!(store
                    .update_rule(RuleChange {
                        id,
                        config,
                        at: now()
                    })
                    .await
                    .map_err(store_err)?),
            )
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
    on: bool,
    command: &str,
) -> Result<(), Error> {
    out.emit(
        command,
        json!(store
            .change_rule_state(rule_id(id)?, on, now())
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
    out.emit("status", json!(status))
}
async fn alerts(cmd: AlertsSub, store: &Arc<SqliteStore>, out: &Out) -> Result<(), Error> {
    if let AlertsSub::Show { alert_id } = &cmd {
        let id = airborne_core::AlertId::new(alert_id.clone())
            .map_err(|e| Error::input(e.to_string()))?;
        let alert = store
            .get_alert(&id)
            .map_err(|e| Error::fail(e.to_string()))?
            .ok_or_else(|| Error::fail("alert was not found"))?;
        return out.emit("alerts.show", json!(alert));
    }
    match cmd{AlertsSub::List{all,watch,..}=>out.emit("alerts.list",json!({"alerts":store.list_alerts(!all,watch.map(watch_id).transpose()?).await.map_err(store_err)?})),AlertsSub::Acknowledge{alert_id,all,yes}=>{if all{confirm(yes,"acknowledge all alerts")?;let ids=store.list_alerts(true,None).await.map_err(store_err)?.into_iter().map(|a|a.id).collect::<Vec<_>>();let n=store.acknowledge(&ids,now()).await.map_err(store_err)?;return out.emit("alerts.acknowledge",json!({"acknowledged":n}));}let id=alert_id.ok_or_else(||Error::input("provide an alert ID or --all"))?;let n=store.acknowledge(&[airborne_core::AlertId::new(id).map_err(|e|Error::input(e.to_string()))?],now()).await.map_err(store_err)?;out.emit("alerts.acknowledge",json!({"acknowledged":n}))},AlertsSub::Show{..}=>Err(Error::fail("alerts show requires alert lookup"))}
}
fn auth(cmd: AuthSub, out: &Out) -> Result<(), Error> {
    match cmd{AuthSub::Status=>out.emit("auth.status",json!({"github":source(ProviderCredential::GitHub),"buildkite":source(ProviderCredential::Buildkite)})),AuthSub::Set{provider}=>{let p=provider_to(provider);let token=read_secret()?;MacosCredentialStore.set(p,token).map_err(credential_error)?;out.emit("auth.set",json!({"provider":p.account_name(),"source":"keychain"}))},AuthSub::Remove{provider,yes}=>{confirm(yes,"remove this credential")?;let p=provider_to(provider);MacosCredentialStore.remove(p).map_err(credential_error)?;out.emit("auth.remove",json!({"provider":p.account_name()}))}}
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
    out.emit("doctor",json!({"data_dir":dir,"schema_version":store.schema_version().map_err(|e|Error::fail(e.to_string()))?,"storage":"ok","github":source(ProviderCredential::GitHub),"buildkite":source(ProviderCredential::Buildkite),"leases":leases,"live":live}))
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
fn store_err(e: airborne_runtime::StoreError) -> Error {
    Error::fail(e.to_string())
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
}
