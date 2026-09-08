use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn airborne() -> Command {
    Command::cargo_bin("airborne").expect("binary is built")
}

#[test]
fn root_help_has_the_production_command_groups() {
    airborne()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Watch pull requests and report when important checks finish",
        ))
        .stdout(predicate::str::contains("watch"))
        .stdout(predicate::str::contains("refresh"))
        .stdout(predicate::str::contains("migrate"));
}

#[test]
fn config_path_is_machine_readable_and_uses_selected_data_dir() {
    let dir = TempDir::new().expect("temp directory");
    airborne()
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"schema_version\":1"))
        .stdout(predicate::str::contains(
            dir.path().to_string_lossy().as_ref(),
        ));
}

#[test]
fn destructive_command_needs_yes_when_not_a_terminal() {
    let dir = TempDir::new().expect("temp directory");
    airborne()
        .args(["--data-dir"])
        .arg(dir.path())
        .args(["watch", "remove", "watch-1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--yes is required"));
}

#[test]
fn invalid_duration_is_a_syntax_error() {
    airborne()
        .args(["run", "--interval", "ten"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("duration"));
}

#[test]
fn auth_set_never_echoes_an_accidental_token_argument() {
    let canary = "secret-canary-value";
    airborne()
        .args(["auth", "set", "github", canary])
        .assert()
        .code(2)
        .stdout(predicate::str::contains(canary).not())
        .stderr(predicate::str::contains(canary).not())
        .stderr(predicate::str::contains("invalid auth set invocation"));
}

#[test]
fn json_errors_use_the_stable_envelope() {
    let dir = TempDir::new().expect("temp directory");
    airborne()
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["run", "--interval", "1s"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"schema_version\":1"))
        .stdout(predicate::str::contains("\"outcome\":\"failure\""));
}

#[test]
fn empty_environment_token_is_not_reported_as_a_credential() {
    let dir = TempDir::new().expect("temp directory");
    airborne()
        .env("AIRBORNE_GITHUB_TOKEN", "")
        .args(["--data-dir"])
        .arg(dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("environment").not());
}

#[test]
fn leaf_help_explains_watch_add() {
    airborne()
        .args(["watch", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GITHUB_PR_URL"));
}

#[test]
fn live_doctor_requires_provider_credentials() {
    let dir = TempDir::new().expect("temp directory");
    airborne()
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .env_remove("AIRBORNE_BUILDKITE_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["doctor", "--live"])
        .assert()
        .code(5)
        .stdout(predicate::str::contains("\"command\":\"doctor\""));
}
