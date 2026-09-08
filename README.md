# PR Watcher

> This repository currently contains the desktop prototype. Airborne is moving
> to a CLI; [`docs/cli/`](docs/cli/README.md) is the authoritative specification
> for new work.

PR Watcher is a local macOS menubar app for GitHub pull requests. It watches
the current head SHA and alerts when Cursor Bugbot or selected Buildkite jobs
finish.

## Run it

```sh
npm install
npm run build
cd src-tauri
cargo run
```

Open **Settings** to enter a GitHub personal access token and a Buildkite token
with `read_builds`. Tokens go to macOS Keychain; the local SQLite database only
stores watches, rules, observations, alerts, and non-secret settings.

Add a Buildkite pipeline mapping before adding a Buildkite job rule. Job names
match exactly, including punctuation and emoji.

Closing the dashboard keeps the menubar app running. Use the tray menu to
refresh, reopen the dashboard, or quit.

## Checks

```sh
cd src-tauri
cargo fmt --check
cargo test --lib --tests
cargo clippy --lib --tests -- -D warnings
cargo check
```

```sh
npm run build
```
