//! Black-box coverage for the public CLI contract. These tests only talk to
//! the compiled executable and temporary files; they never call a provider.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command as ProcessCommand, Output, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

static PROCESS_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn process_test_guard() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("process test mutex")
}

fn airborne() -> Command {
    Command::cargo_bin("airborne").expect("airborne binary is built")
}

fn data_dir() -> TempDir {
    TempDir::new().expect("temporary data directory")
}

fn json(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("valid JSON output")
}

fn interactive_json(output: &[u8]) -> Value {
    let output = String::from_utf8_lossy(output);
    let start = output
        .find('{')
        .unwrap_or_else(|| panic!("JSON result from pseudo-terminal; output: {output:?}"));
    serde_json::from_str(output[start..].trim()).expect("valid pseudo-terminal JSON output")
}

fn assert_envelope(value: &Value, command: &str) {
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], command);
    assert!(value["outcome"].is_string());
    assert!(value["data"].is_object() || value["data"].is_null());
}

fn normalize_dynamic(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_dynamic),
        Value::Object(values) => {
            for (key, value) in values {
                if matches!(key.as_str(), "refresh_id" | "started_at" | "finished_at") {
                    *value = Value::String("<dynamic>".into());
                } else {
                    normalize_dynamic(value);
                }
            }
        }
        _ => {}
    }
}

fn command_help(args: &[&str]) -> String {
    let output = airborne().args(args).output().expect("run help command");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    help.split_once("\nOptions:\n")
        .expect("global options section")
        .0
        .trim_end()
        .to_owned()
}

fn legacy_database(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("prototype.sqlite3");
    let schema = r#"
CREATE TABLE watch (id INTEGER PRIMARY KEY,github_owner TEXT,github_repo TEXT,github_pr_number INTEGER,title TEXT,active INTEGER,created_at TEXT,head_sha TEXT);
CREATE TABLE rule (id INTEGER PRIMARY KEY,watch_id INTEGER,kind TEXT,config_json TEXT,enabled INTEGER,created_at TEXT);
CREATE TABLE rule_observation (rule_id INTEGER,head_sha TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT);
CREATE TABLE alert (rule_id INTEGER,head_sha TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,created_at TEXT);
CREATE TABLE setting (key TEXT PRIMARY KEY,value TEXT);
INSERT INTO watch VALUES(1,'owner','repo',7,'A title',1,'2026-09-08T00:00:00Z','abc');
INSERT INTO rule VALUES(2,1,'cursor_bugbot_completed','{"check_name":"Cursor Bugbot"}',1,'2026-09-08T00:00:00Z');
INSERT INTO rule_observation VALUES(2,'abc','completed','99','https://github.com','done','2026-09-08T00:01:00Z');
INSERT INTO alert VALUES(2,'abc','99','Done','body','unread','2026-09-08T00:01:00Z');
INSERT INTO setting VALUES('settings','{"poll_interval_seconds":60}');
"#;
    let mut sqlite = ProcessCommand::new("/usr/bin/sqlite3")
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("sqlite3 is available on supported macOS");
    sqlite
        .stdin
        .take()
        .expect("sqlite stdin")
        .write_all(schema.as_bytes())
        .expect("write legacy fixture");
    assert!(sqlite.wait().expect("wait for sqlite").success());
    path
}

fn sqlite_execute(database: &PathBuf, sql: &str) {
    let mut sqlite = ProcessCommand::new("/usr/bin/sqlite3")
        .arg(database)
        .stdin(Stdio::piped())
        .spawn()
        .expect("sqlite3 is available on supported macOS");
    sqlite
        .stdin
        .take()
        .expect("sqlite stdin")
        .write_all(sql.as_bytes())
        .expect("write SQLite fixture");
    assert!(sqlite.wait().expect("wait for sqlite").success());
}

fn interactive(args: &[String], input: &[u8]) -> Output {
    let _guard = process_test_guard();
    // `script` gives the child a controlling terminal while retaining a
    // process-level test. macOS ships it as part of the base system.
    let binary = assert_cmd::cargo::cargo_bin("airborne");
    let mut child = ProcessCommand::new("/usr/bin/script")
        .args(["-q", "/dev/null"])
        .arg(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start pseudo-terminal");
    let mut stdin = child.stdin.take().expect("script stdin");
    let input = input.to_vec();
    let writer = thread::spawn(move || {
        // Keep the pipe open after the answer. Closing it immediately makes
        // BSD `script` send EOT to the terminal before it forwards the input.
        thread::sleep(Duration::from_millis(200));
        stdin.write_all(&input).expect("answer prompt");
        stdin.flush().expect("flush prompt answer");
        thread::sleep(Duration::from_secs(1));
    });
    let output = child.wait_with_output().expect("wait for pseudo-terminal");
    writer.join().expect("prompt writer");
    output
}

fn wait_for_exit(child: &mut std::process::Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI did not stop within {} seconds", within.as_secs());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn imported_fixture(data: &TempDir) -> (TempDir, Value) {
    let source_dir = TempDir::new().expect("legacy fixture directory");
    let source = legacy_database(&source_dir);
    let args = vec![
        "--json".into(),
        "--data-dir".into(),
        data.path().display().to_string(),
        "migrate".into(),
        "prototype".into(),
        "--from".into(),
        source.display().to_string(),
    ];
    let output = interactive(&args, b"y\n");
    assert!(output.status.success(), "migration failed: {output:?}");
    let value = interactive_json(&output.stdout);
    assert_envelope(&value, "migrate.prototype");
    (source_dir, value)
}

fn offline_json(data: &TempDir, args: &[&str]) -> Value {
    let output = airborne()
        .arg("--json")
        .arg("--data-dir")
        .arg(data.path())
        .args(args)
        .output()
        .expect("run offline CLI command");
    assert!(
        output.status.success(),
        "offline command failed: {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    json(&output.stdout)
}

#[test]
fn every_public_command_group_and_nested_command_has_useful_help() {
    for args in [
        &["--help"][..],
        &["watch", "--help"],
        &["rule", "--help"],
        &["alerts", "--help"],
        &["auth", "--help"],
        &["config", "--help"],
        &["migrate", "--help"],
        &["watch", "add", "--help"],
        &["rule", "add", "buildkite-job", "--help"],
        &["alerts", "acknowledge", "--help"],
        &["migrate", "prototype", "--help"],
    ] {
        airborne()
            .args(args)
            .assert()
            .success()
            .stdout(predicates::str::contains("Usage: airborne"));
    }
}

#[test]
fn root_and_group_help_command_snapshots_are_stable() {
    for (args, expected) in [
        (
            vec!["--help"],
            "Watch pull requests and report when important checks finish\n\nUsage: airborne [OPTIONS] <COMMAND>\n\nCommands:\n  watch    Manage watched pull requests\n  rule     Manage rules for watched pull requests\n  refresh  Check active watches once, then exit\n  run      Poll active watches in the foreground\n  status   Show watches, rules, and their latest state\n  alerts   List and acknowledge alerts\n  auth     Configure GitHub and Buildkite access\n  config   View and change local settings\n  doctor   Check credentials, storage, and provider access\n  migrate  Import data from the desktop prototype\n  help     Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["watch", "--help"],
            "Manage watched pull requests\n\nUsage: airborne watch [OPTIONS] <COMMAND>\n\nCommands:\n  add     Add a pull request watch\n  list    List watches\n  show    Show one watch\n  pause   Pause a watch\n  resume  Resume a watch and reset its rule baselines\n  remove  Archive a watch\n  help    Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["rule", "--help"],
            "Manage rules for watched pull requests\n\nUsage: airborne rule [OPTIONS] <COMMAND>\n\nCommands:\n  add      Add a rule\n  list     List rules, optionally for one watch\n  show     Show a rule and its current version\n  enable   Enable a rule and reset its baseline\n  disable  Disable a rule without deleting its history\n  update   Update a rule's matching policy\n  remove   Archive a rule\n  help     Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["alerts", "--help"],
            "List and acknowledge alerts\n\nUsage: airborne alerts [OPTIONS] <COMMAND>\n\nCommands:\n  list         List alerts\n  show         Show one alert without acknowledging it\n  acknowledge  Acknowledge one alert or all pending alerts\n  help         Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["auth", "--help"],
            "Configure GitHub and Buildkite access\n\nUsage: airborne auth [OPTIONS] <COMMAND>\n\nCommands:\n  set     Set a credential from hidden terminal input\n  status  Show credential sources without exposing tokens\n  remove  Remove a credential\n  help    Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["config", "--help"],
            "View and change local settings\n\nUsage: airborne config [OPTIONS] <COMMAND>\n\nCommands:\n  get   Read one setting or the default settings\n  set   Set a local configuration value\n  path  Print the data directory and database path\n  help  Print this message or the help of the given subcommand(s)",
        ),
        (
            vec!["migrate", "--help"],
            "Import data from the desktop prototype\n\nUsage: airborne migrate [OPTIONS] <COMMAND>\n\nCommands:\n  prototype  Import supported data from the desktop prototype\n  help       Print this message or the help of the given subcommand(s)",
        ),
    ] {
        assert_eq!(command_help(&args), expected, "help snapshot for {args:?}");
    }
}

#[test]
fn global_flags_validate_and_data_directory_flag_wins_over_environment() {
    let flag = data_dir();
    let environment = data_dir();
    let output = airborne()
        .env("AIRBORNE_DATA_DIR", environment.path())
        .args(["--json", "--data-dir"])
        .arg(flag.path())
        .args(["config", "path"])
        .output()
        .expect("run config path");
    assert!(output.status.success());
    let value = json(&output.stdout);
    assert_envelope(&value, "config.path");
    assert!(value["data"]["data_dir"]
        .as_str()
        .expect("data directory string")
        .contains(flag.path().to_string_lossy().as_ref()));
    assert!(!value["data"]["data_dir"]
        .as_str()
        .expect("data directory string")
        .contains(environment.path().to_string_lossy().as_ref()));
    airborne()
        .args(["--quiet", "--verbose", "config", "path"])
        .assert()
        .code(2);
}

#[test]
fn data_directory_and_database_are_private_to_the_owner() {
    let dir = data_dir();
    airborne()
        .args(["--data-dir"])
        .arg(dir.path())
        .args(["doctor"])
        .assert()
        .success();
    assert_eq!(
        fs::metadata(dir.path())
            .expect("data dir metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let db = dir.path().join("airborne.sqlite3");
    assert_eq!(
        fs::metadata(db)
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[test]
fn config_crud_and_doctor_work_without_provider_access() {
    let dir = data_dir();
    let output = airborne()
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["config", "set", "poll-interval", "30s"])
        .output()
        .expect("set config");
    assert!(output.status.success());
    assert_eq!(
        json(&output.stdout),
        serde_json::json!({
            "schema_version": 1,
            "command": "config.set",
            "outcome": "success",
            "data": {"key": "poll-interval", "value": "30s"}
        })
    );
    let output = airborne()
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["config", "get", "poll-interval"])
        .output()
        .expect("get config");
    assert!(output.status.success());
    let value = json(&output.stdout);
    assert_envelope(&value, "config.get");
    assert_eq!(value["data"]["value"], "30s");
    airborne()
        .args(["--data-dir"])
        .arg(dir.path())
        .args(["config", "set", "poll-interval", "29s"])
        .assert()
        .code(2);
    let output = airborne()
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["doctor"])
        .output()
        .expect("run doctor");
    assert!(output.status.success());
    assert_envelope(&json(&output.stdout), "doctor");
}

#[test]
fn no_color_human_output_has_no_ansi_and_matches_default_non_terminal_output() {
    let data = data_dir();
    airborne()
        .arg("--data-dir")
        .arg(data.path())
        .args(["config", "set", "poll-interval", "30s"])
        .assert()
        .success();
    let plain = airborne()
        .arg("--data-dir")
        .arg(data.path())
        .args(["config", "get", "poll-interval"])
        .output()
        .expect("default human output");
    let no_color = airborne()
        .arg("--no-color")
        .arg("--data-dir")
        .arg(data.path())
        .args(["config", "get", "poll-interval"])
        .output()
        .expect("no-color human output");
    assert!(plain.status.success() && no_color.status.success());
    assert_eq!(plain.stdout, no_color.stdout);
    assert!(!no_color.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
}

#[test]
fn imported_data_supports_offline_watch_rule_alert_and_status_crud() {
    let data = data_dir();
    let (_source, report) = imported_fixture(&data);
    assert_eq!(report["data"]["watches"], 1);
    assert_eq!(report["data"]["rules"], 1);
    assert_eq!(report["data"]["alerts"], 1);

    let list = airborne()
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["watch", "list", "--all"])
        .output()
        .expect("list watches");
    assert!(list.status.success());
    let watches = json(&list.stdout);
    assert_envelope(&watches, "watch.list");
    let watch_id = watches["data"]["watches"][0]["id"]
        .as_str()
        .expect("watch id")
        .to_owned();

    let rules = airborne()
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["rule", "list", "--all"])
        .output()
        .expect("list rules");
    assert!(rules.status.success());
    let rules = json(&rules.stdout);
    assert_envelope(&rules, "rule.list");
    let rule_id = rules["data"]["rules"][0]["id"]
        .as_str()
        .expect("rule id")
        .to_owned();
    for args in [
        vec!["watch", "pause", &watch_id],
        vec!["watch", "resume", &watch_id],
        vec!["rule", "disable", &rule_id],
        vec!["rule", "enable", &rule_id],
        vec!["status", "--watch", &watch_id],
        vec!["alerts", "list", "--all", "--watch", &watch_id],
    ] {
        airborne()
            .arg("--json")
            .arg("--data-dir")
            .arg(data.path())
            .args(args)
            .assert()
            .success();
    }
}

#[test]
fn offline_command_result_envelopes_have_stable_command_specific_shapes() {
    let data = data_dir();
    let (_source, _) = imported_fixture(&data);
    let watches = offline_json(&data, &["watch", "list", "--all"]);
    assert_envelope(&watches, "watch.list");
    assert!(watches["data"]["watches"].is_array());
    let watch_id = watches["data"]["watches"][0]["id"]
        .as_str()
        .expect("watch id")
        .to_owned();
    for (args, command) in [
        (vec!["watch", "show", &watch_id], "watch.show"),
        (vec!["watch", "pause", &watch_id], "watch.pause"),
        (vec!["watch", "resume", &watch_id], "watch.resume"),
        (vec!["status", "--watch", &watch_id], "status"),
        (
            vec!["alerts", "list", "--all", "--watch", &watch_id],
            "alerts.list",
        ),
    ] {
        assert_envelope(&offline_json(&data, &args), command);
    }
    let rules = offline_json(&data, &["rule", "list", "--all"]);
    assert_envelope(&rules, "rule.list");
    let rule_id = rules["data"]["rules"][0]["id"]
        .as_str()
        .expect("rule id")
        .to_owned();
    for (args, command) in [
        (vec!["rule", "show", &rule_id], "rule.show"),
        (vec!["rule", "disable", &rule_id], "rule.disable"),
        (vec!["rule", "enable", &rule_id], "rule.enable"),
        (
            vec!["rule", "update", &rule_id, "--check-name", "Bugbot 2"],
            "rule.update",
        ),
    ] {
        assert_envelope(&offline_json(&data, &args), command);
    }
    let added = offline_json(&data, &["rule", "add", "bugbot", &watch_id]);
    assert_envelope(&added, "rule.add");
    let added_id = added["data"]["id"]
        .as_str()
        .expect("added rule id")
        .to_owned();
    assert_envelope(
        &offline_json(&data, &["rule", "remove", &added_id, "--yes"]),
        "rule.remove",
    );
    let alerts = offline_json(&data, &["alerts", "list", "--all"]);
    let alert_id = alerts["data"]["alerts"][0]["id"]
        .as_str()
        .expect("alert id")
        .to_owned();
    assert_envelope(
        &offline_json(&data, &["alerts", "show", &alert_id]),
        "alerts.show",
    );
    assert_envelope(
        &offline_json(&data, &["alerts", "acknowledge", &alert_id]),
        "alerts.acknowledge",
    );
    assert_envelope(
        &offline_json(&data, &["watch", "remove", &watch_id, "--yes"]),
        "watch.remove",
    );
}

#[test]
fn offline_operational_json_envelopes_have_exact_snapshot_shapes() {
    let data = data_dir();
    let mut refresh = offline_json(&data, &["refresh"]);
    normalize_dynamic(&mut refresh);
    assert_eq!(
        refresh,
        serde_json::json!({
            "schema_version": 1,
            "command": "refresh",
            "outcome": "success",
            "data": {
                "refresh_id": "<dynamic>",
                "started_at": "<dynamic>",
                "finished_at": "<dynamic>",
                "outcome": "success",
                "subjects": []
            }
        })
    );
    assert_eq!(
        offline_json(&data, &["auth", "status"]),
        serde_json::json!({
            "schema_version": 1,
            "command": "auth.status",
            "outcome": "success",
            "data": {"github": "missing", "buildkite": "missing"}
        })
    );
}

#[test]
fn destructive_non_terminal_commands_require_yes_and_auth_never_leaks_a_canary() {
    let data = data_dir();
    let (_source, _) = imported_fixture(&data);
    let watches = airborne()
        .arg("--json")
        .arg("--data-dir")
        .arg(data.path())
        .args(["watch", "list"])
        .output()
        .expect("list watches");
    let id = json(&watches.stdout)["data"]["watches"][0]["id"]
        .as_str()
        .expect("watch id")
        .to_owned();
    airborne()
        .arg("--data-dir")
        .arg(data.path())
        .args(["watch", "remove", &id])
        .assert()
        .code(2);
    let canary = "secret-canary-never-print";
    let output = airborne()
        .args(["auth", "set", "github", canary])
        .output()
        .expect("reject token argument");
    assert_eq!(output.status.code(), Some(2));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains(canary));
    assert!(!fs::read(data.path().join("airborne.sqlite3"))
        .expect("read database")
        .windows(canary.len())
        .any(|bytes| bytes == canary.as_bytes()));
}

#[test]
fn failures_use_json_envelopes_and_the_documented_public_exit_codes() {
    let data = data_dir();
    let output = airborne()
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["watch", "show", "does-not-exist"])
        .output()
        .expect("missing watch");
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output.stdout);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "failure");
    assert!(value["errors"].is_array());
    airborne()
        .args(["run", "--interval", "ten"])
        .assert()
        .code(2);
    airborne()
        .args(["--data-dir"])
        .arg(data.path())
        .args(["watch", "add", "https://github.com/o/r/pull/1"])
        .assert()
        .code(5);
}

#[test]
fn refresh_lease_contention_and_live_doctor_missing_credential_use_public_exit_codes() {
    let data = data_dir();
    // Initialize the current schema before adding a lease owned by another
    // process. Its expiry is deliberately far in the future.
    airborne()
        .arg("--data-dir")
        .arg(data.path())
        .args(["config", "path"])
        .assert()
        .success();
    sqlite_execute(
        &data.path().join("airborne.sqlite3"),
        "INSERT INTO lease(name,owner,expires_at,updated_at) VALUES ('refresh','test-owner',4102444800000,0);",
    );
    let busy = airborne()
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["refresh", "--wait", "0s"])
        .output()
        .expect("contending refresh");
    assert_eq!(busy.status.code(), Some(4));
    let busy = json(&busy.stdout);
    assert_eq!(busy["schema_version"], 1);
    assert_eq!(busy["command"], "refresh");
    assert_eq!(busy["outcome"], "failure");
    assert!(busy["errors"].is_array());

    let live_data = data_dir();
    let (_source, _) = imported_fixture(&live_data);
    let doctor = airborne()
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .env_remove("AIRBORNE_BUILDKITE_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(live_data.path())
        .args(["doctor", "--live"])
        .output()
        .expect("live doctor without credentials");
    assert_eq!(doctor.status.code(), Some(5));
    let doctor = json(&doctor.stdout);
    assert_eq!(doctor["schema_version"], 1);
    assert_eq!(doctor["command"], "doctor");
    assert_eq!(doctor["outcome"], "failure");
    assert!(doctor["errors"].is_array());
}

#[test]
fn migration_dry_run_is_non_mutating_and_real_import_is_idempotent() {
    let data = data_dir();
    let source_dir = TempDir::new().expect("source directory");
    let source = legacy_database(&source_dir);
    let before = fs::read(&source).expect("source bytes before dry run");
    let dry = airborne()
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["migrate", "prototype", "--from"])
        .arg(&source)
        .arg("--dry-run")
        .output()
        .expect("dry run migration");
    assert!(dry.status.success());
    assert_eq!(json(&dry.stdout)["data"]["watches"], 1);
    assert_eq!(
        before,
        fs::read(&source).expect("source bytes after dry run")
    );
    let args = vec![
        "--json".into(),
        "--data-dir".into(),
        data.path().display().to_string(),
        "migrate".into(),
        "prototype".into(),
        "--from".into(),
        source.display().to_string(),
    ];
    let imported = interactive(&args, b"y\n");
    assert!(imported.status.success());
    let first = interactive_json(&imported.stdout);
    assert_eq!(first["data"]["already_imported"], false);
    let second = interactive(&args, b"y\n");
    assert!(second.status.success());
    assert_eq!(
        interactive_json(&second.stdout)["data"]["already_imported"],
        true
    );
}

#[test]
fn run_emits_ndjson_and_signal_shutdown_releases_the_runner_lease() {
    let _guard = process_test_guard();
    let data = data_dir();
    let binary = assert_cmd::cargo::cargo_bin("airborne");
    let mut first = ProcessCommand::new(&binary)
        .args(["--json", "--data-dir"])
        .arg(data.path())
        .args(["run", "--interval", "30s"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start runner");
    let stdout = first.stdout.take().expect("runner stdout");
    let (event_tx, event_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = event_tx.send(result);
    });
    let first_event = event_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("runner emitted its first event")
        .expect("read runner event");
    let event: Value = serde_json::from_str(first_event.trim()).expect("NDJSON event");
    assert_envelope(&event, "run");
    assert_eq!(event["event"], "refresh");
    assert_eq!(event["sequence"], 1);
    let second = ProcessCommand::new(&binary)
        .args(["--data-dir"])
        .arg(data.path())
        .args(["run", "--interval", "30s"])
        .output()
        .expect("start contending runner");
    assert_eq!(second.status.code(), Some(4));
    ProcessCommand::new("/bin/kill")
        .args(["-TERM", &first.id().to_string()])
        .status()
        .expect("send SIGTERM");
    let first = wait_for_exit(&mut first, Duration::from_secs(10));
    assert!(first.success(), "runner did not stop cleanly: {first:?}");
    let mut restarted = ProcessCommand::new(&binary)
        .args(["--data-dir"])
        .arg(data.path())
        .args(["run", "--interval", "30s"])
        .spawn()
        .expect("runner lease released");
    thread::sleep(Duration::from_millis(150));
    ProcessCommand::new("/bin/kill")
        .args(["-INT", &restarted.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(wait_for_exit(&mut restarted, Duration::from_secs(10)).success());
}
