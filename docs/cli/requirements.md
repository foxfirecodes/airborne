# Production CLI requirements

## 1. Goal

Ship a reliable local CLI that watches the current revision of a GitHub pull
request and records an alert when either of these events occurs:

- a named GitHub check, initially `Cursor Bugbot`, completes;
- a selected job in the Buildkite build linked from GitHub reaches the chosen
  terminal state.

Airborne must work well for people and for scripts. Every important workflow
must be testable without a window or desktop automation.

## 2. Scope

### 2.1 Required for version 1

- A single `airborne` executable for macOS.
- Local SQLite storage with automatic, forward-only migrations.
- GitHub and Buildkite credentials in macOS Keychain.
- Environment variable credentials for automation and tests.
- Commands to manage watches, rules, alerts, credentials, and settings.
- A one-shot refresh command.
- A foreground polling command with clean shutdown.
- Human-readable output and versioned JSON output.
- Exact rule behavior and alert lifecycle from the domain model.
- Stable exit codes.
- Captured provider response tests and opt-in live tests.
- Upgrade or import support for useful data from the desktop prototype.

### 2.2 Not required for version 1

- A desktop app, tray icon, web view, or native notification.
- A background launch agent or installer.
- A hosted service, account, sync, webhook, or OAuth flow.
- GitLab, Bitbucket, issues, deploys, or non-PR subjects.
- CI log reading, merge actions, or job control.
- A general rule language or plugin system.
- Windows or Linux support.

The architecture must not block other hosts or operating systems, but version 1
does not claim support for them.

## 3. Command-line contract

### 3.1 Root command

The root help must follow this shape:

```text
Watch pull requests and report when important checks finish

Usage: airborne [OPTIONS] <COMMAND>

Commands:
  watch     Manage watched pull requests
  rule      Manage rules for watched pull requests
  refresh   Check active watches once, then exit
  run       Poll active watches in the foreground
  status    Show watches, rules, and their latest state
  alerts    List and acknowledge alerts
  auth      Configure GitHub and Buildkite access
  config    View and change local settings
  doctor    Check credentials, storage, and provider access
  migrate   Import data from the desktop prototype
  help      Print help for a command

Options:
      --json             Print machine-readable output
      --data-dir <PATH>  Use a different data directory
  -v, --verbose...       Show more detail; repeat for debug output
  -q, --quiet            Print only errors and requested output
      --no-color         Disable colored output
  -h, --help             Print help
  -V, --version          Print version
```

The executable must use `airborne` as its binary and help name. Help, docs, and
errors must not call it `PR Watcher`.

### 3.2 Global behavior

- `--data-dir` must isolate the database, locks, and other local state. It must
  not change Keychain service names.
- `AIRBORNE_DATA_DIR` may set the same value. The flag wins.
- The default data directory on macOS is
  `~/Library/Application Support/Airborne`; the database is
  `airborne.sqlite3` within it.
- Airborne must create its data directory with owner-only access and keep its
  database and local diagnostic files private to the user.
- `--json` must apply to every command that returns data.
- `--quiet` and `--verbose` are mutually exclusive.
- Color must appear only on an interactive terminal. `NO_COLOR` and
  `--no-color` must disable it.
- Finite commands must write one result to standard output and diagnostics to
  standard error.
- `run --json` must write newline-delimited JSON, one complete event per line.
- Commands must never print a credential or HTTP authorization header.
- Timestamps must use UTC RFC 3339 form in JSON.
- IDs must print as strings in JSON even if SQLite stores them as integers.

### 3.3 Watch commands

```text
airborne watch add <GITHUB_PR_URL> [--paused]
airborne watch list [--active|--all]
airborne watch show <WATCH_ID>
airborne watch pause <WATCH_ID>
airborne watch resume <WATCH_ID>
airborne watch remove <WATCH_ID> [--yes]
```

- `watch add` must accept only a canonical HTTPS GitHub pull request URL.
- It must fetch and store the PR title and current head revision before it
  commits the watch.
- Adding the same active or archived subject must fail with a useful message.
  The message must point to the existing watch and, when needed, `resume`.
- `watch remove` must archive the watch and its active rules. It must retain
  alerts and past observations.
- Resuming a paused watch must create a new version of each enabled rule in the
  same transaction. The next successful results become baselines, so work that
  finished while the watch was paused cannot cause late alerts.
- Destructive prompts must work only on an interactive terminal. Scripts must
  pass `--yes`.

### 3.4 Rule commands

```text
airborne rule add bugbot <WATCH_ID> [--check-name <NAME>]

airborne rule add buildkite-job <WATCH_ID> \
  --context <GITHUB_STATUS_CONTEXT> \
  --organization <ORGANIZATION> \
  --pipeline <PIPELINE> \
  --job <EXACT_JOB_NAME> \
  [--notify-on terminal|passed]

airborne rule list [--watch <WATCH_ID>] [--enabled|--all]
airborne rule show <RULE_ID>
airborne rule enable <RULE_ID>
airborne rule disable <RULE_ID>
airborne rule update <RULE_ID> [KIND-SPECIFIC OPTIONS]
airborne rule remove <RULE_ID> [--yes]
```

- The default Bugbot check name must be `Cursor Bugbot`.
- Buildkite context, organization, pipeline, and job name must use exact,
  case-sensitive matching.
- `--notify-on` must default to `terminal`.
- `rule update` must keep the rule kind fixed, accept the same options as that
  kind's `rule add`, and require at least one changed option.
- A change to rule matching or alert policy must create a new rule version.
- Enabling a disabled rule must create a new rule version. Its first successful
  result becomes a baseline, so time spent disabled cannot cause a late alert.
- Disabling a rule must retain its history and current version.
- `rule remove` must archive the rule and retain its alerts and observations.

The CLI does not require a separate pipeline-mapping record. A Buildkite rule
contains the full expected context and pipeline identity. This removes a UI
picker concern from the domain model.

### 3.5 Refresh and run

```text
airborne refresh [WATCH_ID] [--wait <DURATION>]
airborne run [--interval <DURATION>]
```

- `refresh` must poll all active watches, or one named watch, exactly once.
- One failed watch or source must not stop unrelated work.
- The command must commit valid results even when it ends with a partial-failure
  exit code.
- Its result must list attempted watches, successful rule results, source
  issues, new alerts, elapsed time, and overall outcome.
- `--wait` controls how long to wait for another refresh to release its lease.
  The default is 5 seconds.
- `run` must poll immediately, then poll at the configured interval.
- `run --interval` overrides the stored interval for that process and must not
  persist the override.
- The supported interval is 30 seconds through 24 hours.
- Only one `run` process may own a data directory. A second must fail at once
  and identify the active runner when possible.
- `refresh` may run while `run` sleeps. Refreshes against the same data
  directory must never overlap.
- `run` must handle `SIGINT` and `SIGTERM`, finish or cancel safely, release its
  lease, and exit within 10 seconds.
- A canceled refresh must not commit a half-applied subject result.
- Each cycle must emit its report before sleeping.

### 3.6 Status and alerts

```text
airborne status [--watch <WATCH_ID>]
airborne alerts list [--pending|--all] [--watch <WATCH_ID>]
airborne alerts show <ALERT_ID>
airborne alerts acknowledge <ALERT_ID>
airborne alerts acknowledge --all [--yes]
```

- `status` must show the current revision, last poll outcome, enabled rules,
  latest successful evaluation, and current source issue.
- A source issue must not replace the last successful evaluation.
- Alerts are immutable event records. Acknowledgement is the only mutable alert
  field exposed by version 1.
- Opening or listing an alert must not acknowledge it.

### 3.7 Authentication and configuration

```text
airborne auth set github
airborne auth set buildkite
airborne auth status
airborne auth remove github|buildkite [--yes]

airborne config get [KEY]
airborne config set poll-interval <DURATION>
airborne config path

airborne doctor [--live]

airborne migrate prototype [--from <DATABASE>] [--credentials] [--dry-run]
```

- `auth set` must read a token without terminal echo and store it in Keychain.
- The Keychain service name is `airborne`. Account names are `github_token` and
  `buildkite_token`.
- It must refuse token text passed as a command argument.
- `AIRBORNE_GITHUB_TOKEN` and `AIRBORNE_BUILDKITE_TOKEN` must override Keychain
  for the current process and must never be persisted.
- `auth status` may report the active source as `environment`, `keychain`, or
  `missing`; it must not reveal any part of the token.
- `doctor` must check data paths, migrations, locks, and credential presence
  without making network requests.
- `doctor --live` must also make the least costly authenticated request to each
  configured provider. It must not mutate remote state.
- `migrate prototype` must discover the standard prototype database when
  `--from` is absent, print a plan with `--dry-run`, and require confirmation
  before the first real import.
- `--credentials` must separately confirm before copying prototype Keychain
  items. A database import must not imply credential access.

## 4. Output and exit status

### 4.1 Human output

Human output must favor short tables and clear summaries. It must remain useful
without color. Errors must state the failed operation, the affected watch or
rule when known, and the next action when Airborne can name one.

Routine HTTP requests must not print. `-v` may show request method, provider,
and path, but never query secrets, headers, or response bodies that may contain
private data. `-vv` may add timing and decoded counts.

### 4.2 JSON envelope

Finite JSON output must use this envelope:

```json
{
  "schema_version": 1,
  "command": "refresh",
  "outcome": "success",
  "data": {}
}
```

`outcome` is `success`, `partial`, or `failure`. A failure may add an `errors`
array. The `data` shape is command-specific and must have snapshot tests.

`run --json` uses the same fields per line and adds `event` and `sequence`.
Version 1 may add optional fields, but it must not remove fields, change their
types, or change their meaning. A breaking change requires a new schema version
and an explicit selection mechanism.

### 4.3 Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | The command completed successfully. |
| `1` | The operation failed. |
| `2` | Command syntax or input was invalid. |
| `3` | Refresh completed with one or more source failures; valid work was saved. |
| `4` | A runner or refresh lease could not be acquired in time. |
| `5` | A required credential is missing or rejected. |

All other process failures count as defects. Panic exit codes are not part of
the public contract.

For refresh commands, `3` wins when some rule work commits and other source
work fails, including a credential failure limited to one source. Use `5` when
missing or rejected credentials prevent all requested provider work. Syntax,
lease, and credential checks that fail before a refresh starts use their own
codes rather than `1`.

## 5. Reliability and security

- Storage writes for one subject poll must be atomic: observations, alerts,
  poll outcome, and source issues commit together.
- The runtime must create an alert only after it has durable state for the
  result that caused it.
- A restart, repeated poll, or two sequential CLI calls must not create a
  duplicate alert.
- Provider timeouts and retry policy must be bounded. A cycle must not hang
  forever.
- HTTP connect timeout is 5 seconds and each request timeout is 30 seconds. One
  logical provider operation gets at most 60 seconds and three attempts.
- Retries cover connection failures, timeouts, `429`, `502`, `503`, and `504`
  only. They use capped backoff with jitter and honor `Retry-After` only within
  the 60-second operation deadline. Authentication and validation failures do
  not retry.
- At most four subjects and eight provider requests may run at once. These are
  version 1 implementation limits, not user settings.
- SQLite must enable foreign keys, a busy timeout, and WAL mode where supported.
- Every schema change must have an upgrade test from each supported released
  schema. Migrations must never silently discard user data.
- Logs, errors, JSON output, fixtures, and panic messages must not contain
  credentials.
- URLs supplied by providers must be parsed and checked before use. Airborne
  must call only fixed GitHub and Buildkite API hosts.
- Buildkite status links must match the configured organization, pipeline, and
  canonical public URL before Airborne derives an API request.
- A malformed record or response must produce a bounded error, not a panic.
- Poll attempts and source issues may be pruned after 30 days, but storage must
  retain the latest attempt for each watch. Subjects, watches, rule versions,
  observations, and alerts must not be pruned automatically.

## 6. Migration from the prototype

The new implementation must be built beside the current Tauri code. It must
not refactor the old `Poller` in place.

Before the old app is removed, `airborne migrate prototype` must import the
existing `pr-watcher.sqlite3` data. It must:

- preserve watches, rules, observations, alerts, acknowledgement state, and
  non-secret settings when they can be mapped safely;
- assign rule version `1` to imported rules;
- convert `read` alerts to acknowledged and `unread` alerts to pending;
- report skipped or repaired rows without exposing private data;
- leave the source database unchanged;
- be safe to run more than once.

Existing Keychain items under `com.prwatcher.v0` may be read and copied to the
new service only with `migrate prototype --credentials` and confirmation. The
CLI must not delete the old items.

The Tauri app, React app, and old single-crate backend may be deleted only after
the migration test and real-provider validation gates pass.

## 7. Delivery milestones

Each milestone must leave the workspace green. A milestone is not complete
until its listed checks run in automation.

### Milestone 0: specification and test inputs

- Adopt this document set.
- Record scrubbed GitHub and Buildkite response fixtures from known real
  requests, including provider error responses.
- Fix the expected command help and JSON snapshots.

Complete when reviewers can trace each version 1 behavior to a requirement,
domain rule, component, and planned test.

### Milestone 1: workspace and lifecycle core

- Create the Cargo workspace and shared crate skeletons.
- Implement domain value types and the pure reconciliation function.
- Cover first baseline, same-revision transition, new revision, rule change,
  re-enable, unavailable state, and deduplication.

Complete when core tests need no network, database, async runtime, or CLI.

### Milestone 2: provider contracts and PR monitor

- Implement GitHub and Buildkite clients against captured fixtures.
- Implement the typed PR monitor and its fetch plan.
- Decode Buildkite `state` and `finished_at`; do not expect `finished`.
- Fetch each required source once per subject poll.

Complete when contract tests catch field, paging, URL, state, and source
isolation failures, and monitor tests use fake provider ports.

### Milestone 3: durable one-shot vertical slice

- Implement SQLite migrations and repositories.
- Implement runtime refresh, atomic apply, reports, and cross-process refresh
  leases.
- Implement credentials and the `watch`, `rule`, `refresh`, `status`, and
  `alerts` commands.
- Complete one real Bugbot and one real Buildkite read-only refresh.

Complete when a fresh data directory can go from `watch add` to a durable alert
and repeat refreshes create no duplicate.

### Milestone 4: long-running and operational UX

- Implement `run`, signal handling, runner leases, `auth`, `config`, and
  `doctor`.
- Freeze human help, JSON schema version 1, and exit codes.
- Add fault, restart, concurrent-process, and credential-redaction tests.

Complete when a 24-hour soak has no stuck lease, runaway memory, duplicate
alert, lost committed result, or leaked secret.

### Milestone 5: migration and release

- Implement and test prototype data import.
- Run release builds and packaging checks on supported macOS versions and CPU
  types.
- Run the full validation matrix.
- Remove the old desktop implementation and rewrite the root README for the
  CLI.

Complete when all production completion criteria below pass from a clean clone
and the release artifact passes a fresh-machine smoke test.

## 8. Production completion criteria

Airborne version 1 is complete only when all of these statements are true:

1. Every command in section 3 exists, has useful `--help`, and follows the
   documented output and exit contract.
2. Every lifecycle rule in the domain model has a pure table test and a
   SQLite-backed restart test.
3. Captured contract tests cover successful and failed GitHub and Buildkite
   responses, paging, missing fields, unknown states, and rate limits.
4. One source failure cannot block valid results from another source or watch.
5. Two Buildkite rules for one build cause one Buildkite request per refresh.
6. Repeated polls and process restarts cannot create duplicate alerts.
7. Credentials never appear in stdout, stderr, logs, snapshots, fixtures, the
   database, or crash output.
8. Database migration, interrupted-write, corrupt-input, and concurrent-process
   tests pass.
9. `SIGINT` and `SIGTERM` stop `run` cleanly and release all leases.
10. The prototype importer preserves supported data and is idempotent.
11. A real read-only GitHub and Buildkite smoke test passes with user-supplied
    local credentials.
12. A 24-hour foreground soak passes.
13. Formatting, linting, unit, integration, contract, release-build, and help
    snapshot checks pass in CI.
14. The root README describes only supported behavior and includes install,
    quick-start, credential, troubleshooting, and uninstall steps.
15. Known limits are written down. No required validation is reported as done
    when its environment was unavailable.

## 9. Validation approach

### 9.1 Test layers

| Layer | Runs against | Proves |
| --- | --- | --- |
| Domain unit | Pure values | Lifecycle and deduplication rules |
| Provider contract | Captured JSON | Real response decoding and paging |
| Monitor unit | Fake provider ports | Fetch plans, matching, and failure isolation |
| Store integration | Temporary SQLite files | Migrations, transactions, restart behavior |
| Runtime integration | Fake monitor plus real store | Atomic apply, reports, and partial success |
| CLI integration | Built binary plus temp data dir | Help, output, exit codes, prompts, and signals |
| Live smoke | Read-only provider APIs | Credentials, URLs, and current integrations |
| Soak | Foreground `run` | Long-lived timing, locking, and resource use |

Tests must not call live services by default. Live tests require explicit flags
and local credentials. Fixtures must be scrubbed, immutable, and tied to the
request path and capture date in a short manifest.

### 9.2 Required automated checks

The final workspace must support one documented check sequence equivalent to:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo build --workspace --release
```

CLI integration tests must run the built binary rather than calling command
handlers in process. Test cases must include:

- clean setup and every CRUD command;
- human and JSON snapshots;
- no-color and non-interactive behavior;
- each public exit code;
- first baseline and later completion;
- a new revision already complete on first poll;
- rule update and re-enable baselines;
- partial provider failure with saved valid work;
- repeated refresh and restart deduplication;
- refresh and runner lease contention;
- shutdown during sleep and during a request;
- migration from a representative prototype database;
- secret-like canary values absent from every captured output and database.

### 9.3 Live validation

Live validation must use a known PR and exact rules chosen by the user. It must
record the command, Airborne version, time, provider request classes, exit
status, and redacted result. It must not record tokens or full private payloads.

The validation must show:

- GitHub PR and head revision lookup;
- Bugbot check evaluation, including a completed neutral result;
- GitHub status to strict Buildkite build resolution;
- Buildkite build decoding from `state` and `finished_at`;
- exact job matching;
- a second refresh with no duplicate alert.

API success alone does not prove the long-running command. The soak and signal
checks remain separate release gates.
