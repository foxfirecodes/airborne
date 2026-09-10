# Airborne

Airborne is a macOS command-line tool that watches GitHub pull requests and reports when selected checks or Buildkite jobs finish. It stores its state locally and does not need a desktop app or hosted service.

The CLI specification in [docs/cli](docs/cli/README.md) defines the product contract. The included commands may still be under active development; use a release artifact for production work.

## Install

Download the signed Airborne archive for your Mac and CPU type from the release page, then install its `airborne` binary on your `PATH`:

```sh
mkdir -p "$HOME/.local/bin"
unzip airborne-<version>-macos-<arch>.zip -d /tmp/airborne-install
install -m 755 /tmp/airborne-install/airborne "$HOME/.local/bin/airborne"
airborne --version
```

Add `~/.local/bin` to your shell `PATH` if it is not already present. Verify a downloaded archive or binary with the checksums published with that release.

## Quick start

Set credentials interactively. Airborne stores them in your macOS Keychain; never put a token on a command line.

```sh
airborne auth set github
airborne auth set buildkite
airborne doctor --live

airborne watch add https://github.com/OWNER/REPO/pull/123
airborne rule add bugbot watch-1
airborne refresh
airborne alerts list --pending
```

Replace `watch-1` with the ID printed by `watch add`. Add a Buildkite job rule when the pull request publishes a matching GitHub status:

```sh
airborne rule add buildkite-job watch-1 \
  --context 'buildkite/your-pipeline' \
  --organization your-organization \
  --pipeline your-pipeline \
  --job 'Exact job name'
```

Use `airborne run` to poll in the foreground. Press Ctrl-C to stop it safely. Use `--json` for machine-readable output and `--data-dir PATH` to keep test or automation state separate from your normal data.

## Authentication

`airborne auth set github` and `airborne auth set buildkite` read tokens without echo and save them in Keychain. For CI or short-lived automation, set `AIRBORNE_GITHUB_TOKEN` and `AIRBORNE_BUILDKITE_TOKEN`; environment credentials override Keychain only for that process and are never saved.

`airborne auth status` reports only whether each credential comes from the environment, Keychain, or is missing. `airborne doctor` checks local state without a network call; add `--live` for read-only provider checks.

### Local debug credentials

Debug builds use a nonempty process token first, then a nonempty `./.env` value
in the process working directory. Copy [`.env.example`](.env.example) to
`.env`, then add the tokens you need. They never read Keychain for credentials
or `auth status` or `doctor`; those commands report whether a value came from
the environment, `.env`, or neither. An empty process value may be filled by a
nonempty `.env` value; otherwise it is missing and does not fall back to
Keychain. A nonempty process token for one provider skips `.env` lookup for that
provider. A missing `.env` is fine. Duplicate `AIRBORNE_GITHUB_TOKEN` or
`AIRBORNE_BUILDKITE_TOKEN` keys, or an existing malformed or unreadable `.env`,
are errors only when Airborne must resolve a credential from that file.

Release builds do not load `.env`: nonempty process environment values take
precedence over Keychain. `airborne auth set` and `airborne auth remove` always
write to or remove from Keychain, even when a debug build ignores it for reads.
`airborne migrate prototype --credentials` is likewise an explicit Keychain
operation. If a release Keychain read fails, Airborne reports a safe credential
error; it does not report the credential as missing.

## Troubleshooting

- Run `airborne doctor` first. It checks the data directory, migrations, locks, and credential presence without contacting providers.
- Run `airborne doctor --live` when local checks pass but refreshes fail. It makes the smallest authenticated read-only request to each configured provider.
- A second `run` process for the same data directory should exit with code 4. Use `airborne config path` to confirm which directory you are using.
- `refresh` exit code 3 means some valid results were saved but one or more sources failed. Exit code 5 means required credentials are missing or rejected.
- For a clean repro, pass `--data-dir` before the command. This does not change Keychain service names.

See [known limits](docs/cli/known-limits.md) and the full [command contract](docs/cli/requirements.md).

## Uninstall

Stop any foreground `airborne run` process, then remove the installed binary. The following command preserves local data and Keychain credentials:

```sh
rm -f "$HOME/.local/bin/airborne"
```

To remove local state as well, first confirm the exact path with `airborne config path`, then delete that specific directory. Remove credentials separately with `airborne auth remove github --yes` and `airborne auth remove buildkite --yes`.
