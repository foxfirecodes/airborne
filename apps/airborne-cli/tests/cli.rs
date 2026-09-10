use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
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
fn debug_credentials_read_the_current_directory_dotenv_after_empty_process_values() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    fs::write(
        dir.path().join(".env"),
        "AIRBORNE_GITHUB_TOKEN=dotenv-github\nAIRBORNE_BUILDKITE_TOKEN=\nUNRELATED=value\n",
    )
    .expect("write dotenv");
    airborne()
        .current_dir(dir.path())
        .env("AIRBORNE_GITHUB_TOKEN", "")
        .env("AIRBORNE_BUILDKITE_TOKEN", "")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"github\":\".env\""))
        .stdout(predicate::str::contains("\"buildkite\":\"missing\""))
        .stdout(predicate::str::contains("dotenv-github").not())
        .stdout(predicate::str::contains("UNRELATED").not());
}

#[test]
fn debug_credentials_prefer_inherited_environment_over_dotenv() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    fs::write(
        dir.path().join(".env"),
        "AIRBORNE_GITHUB_TOKEN=dotenv-github\nAIRBORNE_BUILDKITE_TOKEN=dotenv-buildkite\n",
    )
    .expect("write dotenv");
    airborne()
        .current_dir(dir.path())
        .env("AIRBORNE_GITHUB_TOKEN", "inherited-github")
        .env("AIRBORNE_BUILDKITE_TOKEN", "inherited-buildkite")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"github\":\"environment\""))
        .stdout(predicate::str::contains("\"buildkite\":\"environment\""))
        .stdout(predicate::str::contains("inherited-github").not());
}

#[test]
fn complete_process_credentials_do_not_read_a_malformed_dotenv() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    fs::write(
        dir.path().join(".env"),
        "AIRBORNE_GITHUB_TOKEN=first\nAIRBORNE_GITHUB_TOKEN=second\nnot valid dotenv\n",
    )
    .expect("write dotenv");
    airborne()
        .current_dir(dir.path())
        .env("AIRBORNE_GITHUB_TOKEN", "inherited-github")
        .env("AIRBORNE_BUILDKITE_TOKEN", "inherited-buildkite")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"github\":\"environment\""))
        .stdout(predicate::str::contains("\"buildkite\":\"environment\""));
}

#[test]
fn debug_credentials_only_read_dotenv_from_the_exact_current_directory() {
    let parent = TempDir::new().expect("parent directory");
    let child = parent.path().join("child");
    let data_dir = TempDir::new().expect("data directory");
    fs::create_dir(&child).expect("create child directory");
    fs::write(
        parent.path().join(".env"),
        "AIRBORNE_GITHUB_TOKEN=parent-token\n",
    )
    .expect("write parent dotenv");
    airborne()
        .current_dir(&child)
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .env_remove("AIRBORNE_BUILDKITE_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"github\":\"missing\""))
        .stdout(predicate::str::contains("parent-token").not());
}

#[test]
fn debug_credentials_are_missing_without_process_values_or_dotenv() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    airborne()
        .current_dir(dir.path())
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .env_remove("AIRBORNE_BUILDKITE_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"github\":\"missing\""))
        .stdout(predicate::str::contains("\"buildkite\":\"missing\""));
}

#[test]
fn malformed_dotenv_fails_without_exposing_or_persisting_tokens() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    let canary = "dotenv-secret-canary";
    fs::write(
        dir.path().join(".env"),
        format!("AIRBORNE_GITHUB_TOKEN={canary}\nthis is not dotenv\n"),
    )
    .expect("write dotenv");
    airborne()
        .current_dir(dir.path())
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("could not parse .env credentials"))
        .stdout(predicate::str::contains(canary).not())
        .stderr(predicate::str::contains(canary).not());
    for entry in fs::read_dir(data_dir.path()).expect("read data directory") {
        let bytes = fs::read(entry.expect("directory entry").path()).expect("read data file");
        assert!(
            !bytes
                .windows(canary.len())
                .any(|window| window == canary.as_bytes()),
            "credential canary was persisted"
        );
    }
}

#[test]
fn unreadable_dotenv_fails_without_exposing_credentials() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    fs::create_dir(dir.path().join(".env")).expect("create dotenv directory");
    airborne()
        .current_dir(dir.path())
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .env_remove("AIRBORNE_BUILDKITE_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("credentials"))
        .stdout(predicate::str::contains("AIRBORNE_GITHUB_TOKEN").not());
}

#[test]
fn duplicate_dotenv_credentials_are_rejected_without_exposing_tokens() {
    let dir = TempDir::new().expect("temp directory");
    let data_dir = TempDir::new().expect("data directory");
    let canary = "dotenv-duplicate-canary";
    fs::write(
        dir.path().join(".env"),
        format!("AIRBORNE_GITHUB_TOKEN={canary}\nAIRBORNE_GITHUB_TOKEN=other-token\n"),
    )
    .expect("write dotenv");
    airborne()
        .current_dir(dir.path())
        .env_remove("AIRBORNE_GITHUB_TOKEN")
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["auth", "status"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "invalid .env credential configuration",
        ))
        .stdout(predicate::str::contains(canary).not())
        .stderr(predicate::str::contains(canary).not());
    for entry in fs::read_dir(data_dir.path()).expect("read data directory") {
        let bytes = fs::read(entry.expect("directory entry").path()).expect("read data file");
        assert!(
            !bytes
                .windows(canary.len())
                .any(|window| window == canary.as_bytes()),
            "credential canary was persisted"
        );
    }
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
