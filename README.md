# Airborne

Airborne watches GitHub pull requests and tells you when chosen checks or
Buildkite jobs finish. It runs on macOS, stores its state on your machine, and
needs no desktop app or hosted service.

## Install

Clone the repository, then install Airborne with Rust and Cargo:

```sh
git clone https://github.com/foxfirecodes/airborne.git
cd airborne
cargo install --path apps/airborne-cli
```

Make sure Cargo's bin directory is on your `PATH`, then check the install:

```sh
airborne --version
```

## Use

Save a GitHub token in your macOS Keychain, check that it works, then add a
pull request:

```sh
airborne auth set github
airborne doctor --live
airborne watch add https://github.com/OWNER/REPO/pull/123
```

`watch add` prints a watch ID. Use it to choose what Airborne should track:

```sh
airborne rule add bugbot WATCH_ID
airborne refresh
airborne alerts list --pending
```

To watch a Buildkite job, save a Buildkite token and add a job rule:

```sh
airborne auth set buildkite
airborne rule add buildkite-job WATCH_ID \
  --context 'buildkite/your-pipeline' \
  --organization your-organization \
  --pipeline your-pipeline \
  --job 'Exact job name'
```

Run `airborne run` to keep polling in the foreground. Press Ctrl-C to stop it.
Run `airborne --help` or `airborne <COMMAND> --help` for all commands and
options.

## Notes

- Tokens entered with `airborne auth set` go to your macOS Keychain and never
  appear in the command itself.
- `airborne doctor` checks local setup. Add `--live` to test provider access.
- `airborne config path` shows where Airborne keeps its database.
- `--json` gives machine-readable output. `--data-dir PATH` keeps test state
  separate from your normal data.

See the [CLI docs](docs/cli/README.md) for the full command contract and known
limits.
