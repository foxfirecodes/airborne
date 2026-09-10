# Component architecture

## 1. Decision

Build Airborne as a Cargo workspace with one production host: the CLI. Keep the
domain, provider decoding, pull request monitoring, lifecycle runtime, storage,
credentials, and rendering behind clear boundaries.

Build the new path beside the desktop prototype. Do not refactor the old
`Poller` one method at a time. Remove the old path only after the CLI passes its
migration and live validation gates.

## 2. Workspace

```text
Cargo.toml
apps/
  airborne-cli/
    src/
crates/
  airborne-core/
  airborne-github/
  airborne-buildkite/
  airborne-pr/
  airborne-runtime/
  airborne-store-sqlite/
  airborne-credentials-macos/
tests/
  fixtures/
    github/
    buildkite/
docs/
  cli/
```

These are ownership boundaries, not packages for publication. Do not create a
crate for each command, rule, table, or output format.

## 3. Dependency direction

```text
airborne-cli
  -> airborne-runtime
  -> airborne-store-sqlite
  -> airborne-credentials-macos
  -> airborne-pr

airborne-runtime -> airborne-core
airborne-pr      -> airborne-core
airborne-pr      -> airborne-runtime
airborne-pr      -> airborne-github
airborne-pr      -> airborne-buildkite
airborne-store-sqlite -> airborne-core
airborne-store-sqlite -> airborne-runtime

airborne-github     -> HTTP and GitHub payloads
airborne-buildkite  -> HTTP and Buildkite payloads
```

No arrow may point to `airborne-cli`. Core and runtime must not depend on Clap,
terminal behavior, Keychain, SQLite, Reqwest, or Tauri. Provider crates must not
depend on storage, lifecycle policy, CLI output, or each other. Runtime owns its
ports; concrete monitor and store crates depend on runtime only to implement
those ports.

The CLI may compose concrete adapters. It must not contain provider decoding,
rule evaluation, lifecycle decisions, or SQL.

## 4. Component duties

### 4.1 `airborne-core`

Own:

- checked IDs and value types;
- subjects, watches, typed rules, rule versions, candidates, observations,
  alerts, poll attempts, and source issues;
- provider-neutral rule state;
- the pure `reconcile` function;
- stable multi-job source identity hashing;
- domain errors and invariants.

Must not own:

- provider payloads or HTTP;
- async scheduling;
- wall-clock lookup;
- storage or migrations;
- text tables, JSON envelopes, or exit codes.

Its tests must run with no system services.

### 4.2 `airborne-github`

Own:

- canonical GitHub PR URL parsing;
- GitHub request construction, authentication, paging, status handling, and
  response decoding;
- GitHub-shaped records for pull requests, check runs, and commit statuses;
- safe mapping from transport failures to typed provider errors.

Must not know:

- the meaning of Bugbot;
- how Buildkite links are resolved;
- watches, baselines, alerts, SQLite, or command output.

The public client contract is narrow:

```rust
pub trait GitHubApi: Send + Sync {
    async fn pull_request(&self, key: &GitHubPullRequestKey)
        -> Result<PullRequestSnapshot, GitHubError>;

    async fn check_runs(&self, repository: &GitHubRepositoryKey, revision: &Revision)
        -> Result<Vec<CheckRun>, GitHubError>;

    async fn commit_statuses(&self, repository: &GitHubRepositoryKey, revision: &Revision)
        -> Result<Vec<CommitStatus>, GitHubError>;
}
```

Concrete transport and fake clients implement the same port. Contract tests
must drive the concrete decoder with captured HTTP status, headers, and bodies.

### 4.3 `airborne-buildkite`

Own:

- strict canonical Buildkite public URL parsing;
- Buildkite request construction, authentication, paging if required, status
  handling, and response decoding;
- typed build and job states;
- derivation of build completion from `state` and `finished_at`;
- safe mapping from transport failures to typed provider errors.

Contract:

```rust
pub trait BuildkiteApi: Send + Sync {
    async fn build(&self, key: &BuildkiteBuildKey)
        -> Result<BuildSnapshot, BuildkiteError>;
}
```

The decoded build must expose `state`, `finished_at`, and current jobs. It must
not define a response field named `finished`.

The component must accept only fixed Buildkite API hosts generated from a
checked build key. It must never fetch a provider-supplied target URL directly.

### 4.4 `airborne-pr`

Own:

- the GitHub pull request subject monitor;
- typed PR rules;
- fetch-plan construction from enabled rules;
- exact check, status context, pipeline, and job matching;
- grouping Buildkite rules by build key;
- conversion of provider snapshots into candidates and scoped source issues.

It composes `GitHubApi` and `BuildkiteApi`, but must not know their concrete
HTTP implementations. It implements the `SubjectMonitor` port owned by
`airborne-runtime`.

Runtime-owned contract:

```rust
pub trait SubjectMonitor: Send + Sync {
    fn kind(&self) -> SubjectKind;

    async fn poll(&self, request: MonitorRequest)
        -> MonitorReport;
}

pub struct MonitorRequest {
    pub subject: Subject,
    pub rules: Vec<VersionedRule>,
    pub observed_at: Timestamp,
}

pub struct MonitorReport {
    pub subject_key: SubjectKey,
    pub metadata: Option<SubjectMetadataUpdate>,
    pub revision: Option<Revision>,
    pub results: Vec<RulePollResult>,
    pub subject_issue: Option<SourceIssueDraft>,
}

pub enum RulePollResult {
    Candidate(Candidate),
    Issue { rule: RuleKey, issue: SourceIssueDraft },
}
```

The report must contain one result for every requested rule unless
`subject_issue` prevents the revision lookup. A rule issue lives in that rule's
`RulePollResult`; it is not copied into a second list. Results must follow input
rule order for stable output. One source failure must affect only dependent
rules.

Fetch-plan rules:

- Fetch the PR once.
- Fetch check runs only when a check rule needs them.
- Fetch commit statuses only when a Buildkite rule needs them.
- Fetch each derived Buildkite build once.
- Do not let a failed check-run request suppress Buildkite work or the reverse.
- A failed PR lookup stops that subject because no current revision is known.

### 4.5 `airborne-runtime`

Own:

- selecting active watches for a refresh;
- grouping rules and invoking the right subject monitor;
- lifecycle reconciliation;
- durable per-revision missing-threshold and source-seen lifecycle state;
- atomic application through storage ports;
- refresh and poll reports;
- bounded concurrency and cancellation;
- runner timing, using injected clock and sleeper ports;
- runner and refresh lease policy.

It is an application service. It does not decode provider JSON, execute SQL,
read Keychain, parse command arguments, render output, or send notifications.

Core runtime contract:

```rust
pub trait RuntimeStore: Send + Sync {
    async fn load_refresh_targets(&self, scope: RefreshScope)
        -> Result<Vec<RefreshTarget>, StoreError>;

    async fn load_rule_history(&self, keys: &[RuleKey])
        -> Result<RuleHistorySet, StoreError>;

    async fn apply_poll(&self, commit: PollCommit)
        -> Result<AppliedPoll, StoreError>;
}

pub trait LeaseStore: Send + Sync {
    async fn acquire_refresh(&self, wait: Duration)
        -> Result<RefreshLease, LeaseError>;

    async fn acquire_runner(&self)
        -> Result<RunnerLease, LeaseError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct PollCommit {
    pub attempt: PollAttemptDraft,
    pub observations: Vec<Observation>,
    pub alerts: Vec<AlertDraft>,
    pub issues: Vec<SourceIssueDraft>,
    pub metadata: Option<SubjectMetadataUpdate>,
}
```

`apply_poll` is one transaction for one subject. It inserts alerts with their
unique keys and returns only alerts newly inserted by that transaction. The
runtime builds its report from committed results, not uncommitted drafts.

No `AlertEffect` belongs in the version 1 runtime. The CLI prints committed new
alerts from the refresh report. A later notifier consumes committed alerts
through a separate adapter; it must not take part in reconciliation or the
storage transaction.

The runtime may poll independent subjects concurrently with a small fixed
limit. It must process all candidates for one subject into one commit. Tests
must control ordering and time; production correctness must not depend on task
completion order.

### 4.6 `airborne-store-sqlite`

Own:

- the database connection pool or serialized connection policy;
- schema, forward migrations, and prototype import;
- repository implementations for command CRUD;
- `RuntimeStore` and `LeaseStore` implementations;
- atomic subject poll commits;
- unique and foreign-key constraints;
- retention of operational poll records;
- WAL, busy timeout, integrity checks, and safe recovery errors.

It must return domain records, not SQLite rows or JSON blobs. Typed rule
definitions may use an internal tagged encoding, but the store must validate
them when reading and fail safely on an unknown version.

Required logical records:

```text
subject
watch
rule
  rule_definition
  observation
  revision_lifecycle
  alert
  poll_attempt
  source_issue
  setting
  lease
  schema_migration
  migration_repair
  import_record
```

Physical table names may differ, but migrations and store tests must show a
direct mapping to every record above.

Required database constraints include:

- one non-archived watch per subject;
- unique `(rule_id, version)` definition;
- unique `(rule_id, rule_version, revision)` observation;
- unique `(rule_id, rule_version, revision, event_kind, source_identity)`
  alert, with a normalized sentinel for a missing source identity;
- one non-archived rule kind per watch, enforced by a partial unique index or
  an equivalent transactional constraint;
- no cascading delete from watch or rule into alerts;
- monotonic schema migration versions.

Lease rows must include owner identity and expiry so a crashed process cannot
block Airborne forever. An active runner lease must use renewal. Refresh leases
must cover only a refresh, not runner sleep, and must renew during long network
requests. Loss of lease renewal must cancel new work before another commit.

The migration that introduces the rule-kind constraint must retain every legacy
rule, version, observation, and alert. It must keep the earliest active row in
each duplicate `(watch_id, kind)` group (creation time, then ID) and archive
the other rows. A durable repair record must name the surviving and archived
rule IDs and the deterministic reason. It must be idempotent.

### 4.7 `airborne-credentials-macos`

Own:

- Keychain reads, writes, status, and deletion under the Airborne service name;
- explicit import from `com.prwatcher.v0`;
- a `CredentialProvider` adapter for the host.

Contract:

```rust
pub enum ProviderCredential {
    GitHub,
    Buildkite,
}

pub trait CredentialStore {
    fn get(&self, provider: ProviderCredential)
        -> Result<SecretString, CredentialError>;
    fn set(&self, provider: ProviderCredential, value: SecretString)
        -> Result<(), CredentialError>;
    fn remove(&self, provider: ProviderCredential)
        -> Result<(), CredentialError>;
}
```

Secret values must use a type whose debug and display forms redact content.
Provider clients receive secrets at composition time and must not expose them
through their error types.

Credential composition belongs in the CLI layer. Release builds use a nonempty
process environment value before Keychain and never load `.env`. Debug builds
use a nonempty process value first, then `./.env` in the process working
directory; they never read Keychain for normal credential resolution, status,
or doctor. An empty process value may be filled by a nonempty `.env` value.
Otherwise it is missing, never a reason to fall back. A missing `.env` is valid;
duplicate credential keys, or an existing malformed or unreadable file, fail
safely only when a provider needs resolution from `.env`; a nonempty process
value for that provider skips the file. Status and doctor retain whether a
debug value came from the process environment or `.env`. A release Keychain
read error propagates as a safe error, never as a missing credential.

Keychain operations remain explicit: `auth set`, `auth remove`, and
`migrate prototype --credentials` call the Keychain store in both build modes.
Tests use an in-memory credential store or a controlled `.env`; they do not call
Keychain.

### 4.8 `airborne-cli`

Own:

- Clap command and option definitions;
- composition of concrete stores, monitors, credentials, runtime, clock, and
  HTTP clients;
- terminal detection, prompts, tables, color, and progress;
- JSON and NDJSON envelopes;
- mapping typed errors and refresh outcomes to exit codes;
- signal handling and top-level cancellation;
- data directory selection;
- `doctor` checks.

Command handlers may call narrow repository services for CRUD and the runtime
for refresh and run. They must return typed command results before rendering.
This lets human and JSON output describe the same result.

```rust
pub struct CommandResult<T> {
    pub outcome: CommandOutcome,
    pub data: T,
    pub diagnostics: Vec<Diagnostic>,
}
```

Renderer snapshot tests must not stand in for domain or runtime tests.

## 5. Command service boundaries

Runtime polling and command CRUD have different needs. Keep the ports small:

```rust
pub trait CatalogRepository {
    async fn add_watch(&self, draft: NewWatch) -> Result<Watch, StoreError>;
    async fn list_watches(&self, filter: WatchFilter) -> Result<Vec<Watch>, StoreError>;
    async fn change_watch_state(&self, id: WatchId, state: WatchState)
        -> Result<Watch, StoreError>;

    async fn add_rule(&self, draft: NewRule) -> Result<Rule, StoreError>;
    async fn update_rule(&self, change: RuleChange) -> Result<Rule, StoreError>;
    async fn change_rule_state(&self, id: RuleId, enabled: bool)
        -> Result<Rule, StoreError>;
}

pub trait AlertRepository {
    async fn list_alerts(&self, filter: AlertFilter) -> Result<Vec<Alert>, StoreError>;
    async fn acknowledge(&self, selection: AlertSelection, at: Timestamp)
        -> Result<AcknowledgeResult, StoreError>;
}

pub trait StatusRepository {
    async fn status(&self, scope: StatusScope) -> Result<StatusView, StoreError>;
}
```

`watch add` also needs current GitHub metadata. Put that workflow in a small
application service owned by runtime or CLI composition; do not let the SQLite
repository call GitHub.

`add_rule` must return a typed conflict when a non-archived rule of that kind
already exists for the watch, including when that existing rule is disabled.
Rule updates must distinguish policy set, policy clear, and policy unchanged so
the CLI can implement explicit start enable/disable and missing-threshold
set/clear flags without ambiguous optional values.

## 6. Error contracts

Each boundary returns a typed error with:

- a stable category for policy and exit mapping;
- a safe message for the user;
- a source chain for verbose local diagnosis;
- retryability where it matters;
- provider and request class, but no secret or private response body.

Required categories are:

```text
invalid_input
not_found
conflict
credential_missing
credential_rejected
rate_limited
network
provider_response
source_resolution
storage
lease_busy
canceled
internal
```

The runtime turns provider failures into source issues and continues when it
can. It returns storage failures because it cannot claim work was saved. The
CLI alone maps final errors to process exit codes.

Unknown provider enum values are provider response errors. Preserve the raw
safe value in verbose diagnostics so a new state can be added with evidence.

## 7. Data and control flow

### 7.1 One-shot refresh

```text
CLI parses command and opens adapters
  -> runtime acquires refresh lease
  -> store returns active targets and versioned rules
  -> PR monitor builds and runs the fetch plan
  -> monitor returns candidates and scoped issues
  -> runtime loads rule history and reconciles candidates
  -> store commits one subject poll atomically
  -> runtime returns only committed results and new alerts
  -> CLI renders text or JSON and selects exit code
```

### 7.2 Foreground runner

```text
CLI acquires runner lease
  -> refresh immediately
  -> render cycle report
  -> sleep with cancellation
  -> refresh again
  -> on signal, stop safely and release runner lease
```

The runner does not hold the refresh lease while sleeping. A manual `refresh`
may run between cycles.

### 7.3 Partial failure

```text
GitHub PR fetch succeeds
  -> check-run fetch fails
  -> commit-status and Buildkite fetch succeed
  -> check rules return source issues
  -> Buildkite rules return candidates
  -> runtime reconciles and commits candidates, alerts, and issues
  -> CLI reports partial outcome and exits 3
```

## 8. Concurrency, cancellation, and time

- The runtime must accept an injected clock. Domain functions never read wall
  time.
- Network requests must have connect and total timeouts.
- Subject concurrency is limited to four and total provider-request concurrency
  is limited to eight. Keep both as code-level settings, not version 1 user
  options.
- There may be only one refresh per data directory at a time, across processes.
- Cancellation must stop new provider calls. A subject transaction already in
  commit may finish; it must never be left half-written.
- Report order must be stable by watch and rule ID, regardless of concurrent
  task order.
- Lease expiry uses a wall-clock timestamp for crash recovery and an owner
  token for safe release. A process must never release another owner's lease.

## 9. Testing seams

Every system dependency needs a replaceable boundary:

| Dependency | Production adapter | Test adapter |
| --- | --- | --- |
| GitHub | Reqwest GitHub client | Fake client and captured HTTP server |
| Buildkite | Reqwest Buildkite client | Fake client and captured HTTP server |
| Storage | SQLite file | Temporary SQLite file |
| Credentials | macOS Keychain | In-memory redacted store |
| Time | System clock | Fixed/manual clock |
| Sleep | Tokio sleep | Manual sleeper |
| Signals | OS signal listener | Cancellation token |
| Terminal | Standard streams | Captured byte streams |

Tests must assert calls through fake provider ports so fetch-plan behavior is
visible. Contract tests must separately exercise real decoding code; fake
models alone cannot prove provider compatibility.

## 10. Rules for future components

A new subject type must add a typed subject key, typed rules, and one monitor.
It may reuse runtime and storage ports. Do not add a generic JSON fact map.

A desktop app, daemon, MCP server, or notifier may become another outer host or
adapter. It must consume the same runtime and committed alert records. It must
not fork lifecycle policy.

Do not make Airborne depend on file watchers or process-output tools. If future
packaging needs process supervision, keep it outside the core and runtime.
