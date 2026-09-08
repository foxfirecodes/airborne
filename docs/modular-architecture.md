# Modular Airborne

> Historical desktop-first architecture. The authoritative component design is
> now [`docs/cli/architecture.md`](cli/architecture.md). Where the documents
> conflict, the CLI design wins.

## Decision

Replace the current all-in-one Tauri backend with a Cargo workspace. Keep one
repository and one desktop app. Split it into a small set of crates with useful
boundaries; do not publish them or build a plugin system.

The reusable product is a monitor runtime:

```text
watch + rules
    -> subject monitor
    -> typed rule results
    -> lifecycle engine
    -> durable alerts
    -> desktop effects
```

A PR is the first subject monitor. Later subjects can use the same runtime
without making the PR monitor generic JSON machinery.

## Why the first implementation failed

The existing backend has sensible files, but `Poller` still owns too much:

- It fetches GitHub data, resolves Buildkite, evaluates rules, applies baseline
  policy, writes SQLite, and sends notifications.
- It fetches checks and statuses even when the PR only has Bugbot rules.
- One source failure ends `poll_watch`, so a Buildkite problem can suppress a
  valid Bugbot result for the same PR.
- It fetches the same Buildkite build once per matching rule.
- Editing a rule does not create a new baseline, so changing a job name can
  cause a retroactive alert.

There is also a concrete API mismatch. `src-tauri/src/buildkite.rs` deserializes
`finished: bool`; Buildkite's build response documents `state` and
`finished_at`, not `finished`. The live response therefore fails before job
rules run. The existing tests construct jobs in memory, so they cannot catch a
bad Buildkite response shape.

Do not try to refactor that poller a method at a time. Preserve it as a
reference if useful, but build the new vertical slice beside it and delete it
only after the new path handles a real PR.

## Workspace shape

```text
Cargo.toml                         # workspace only
crates/
  airborne-core/                   # no HTTP, SQLite, Tokio, or Tauri
  airborne-github/                 # GitHub API client and decoded payloads
  airborne-buildkite/              # Buildkite API client and decoded payloads
  airborne-pr/                     # GitHub PR monitor; composes the two clients
  airborne-runtime/                # polling, grouping, lifecycle, alert handoff
  airborne-store-sqlite/           # SQLite implementation of runtime storage
apps/
  airborne-desktop/
    src-tauri/                     # Tauri commands, Keychain, tray, notifications
    src/                           # React UI
```

Dependencies point inward:

```text
airborne-core <- airborne-pr <- airborne-runtime <- airborne-desktop
      ^                ^                   ^
      |                |                   |
airborne-store-sqlite  |           desktop supplies store and effects
                       |
             airborne-github + airborne-buildkite
```

`airborne-core` and the API clients must not depend on Tauri. The desktop app is
only a host. A future CLI, daemon, or MCP can use the same runtime without
copying polling or alert logic.

Do not make crates for the tray, notifications, settings form, or each rule.
Those are desktop details or small modules, not reusable boundaries.

## Responsibility of each crate

### `airborne-core`

Own the durable concepts and the pure lifecycle state machine:

- `WatchId`, `RuleId`, `RuleVersion`, `SubjectRef`, and `Revision`.
- A generic `RuleResult`: waiting, in progress, completed, failed, unavailable.
- `SourceIdentity`, source URL, and human-readable detail.
- Initial-baseline, revision-change, and deduplication policy.
- The decision to create an alert.

It has no provider types, database code, time source, or desktop effects. Its
central API should look like this:

```rust
pub fn reconcile(
    rule: RuleKey,
    prior: Option<&Observation>,
    candidate: Candidate,
) -> Reconciliation;

pub struct Reconciliation {
    pub observation: Observation,
    pub new_alert: Option<AlertDraft>,
}
```

`Candidate` is an already-evaluated result from a subject monitor. It includes
the subject revision and source identity. `reconcile` decides whether that
candidate is the rule's initial baseline or a new event.

The alert key is:

```text
(rule_id, rule_version, subject_revision, source_identity)
```

Increment `rule_version` for every material rule edit. This prevents a changed
rule from inheriting old observations or deduplication records.

### `airborne-github`

Own only GitHub transport and GitHub-shaped data:

- Parse and validate a GitHub PR URL.
- Fetch a PR and return its title and head SHA.
- Fetch check runs for a SHA.
- Fetch commit statuses for a SHA.
- Handle GitHub authentication, paging, response errors, and response decoding.

It must not know what Cursor Bugbot is, what Buildkite is, or when an alert is
sent.

### `airborne-buildkite`

Own only Buildkite transport and Buildkite-shaped data:

- Strictly parse a canonical Buildkite build URL.
- Fetch one exact build and its jobs.
- Expose build state, `finished_at`, and the current jobs.
- Classify raw Buildkite job states as running or terminal.

The decoded build type must reflect the documented response:

```rust
pub struct Build {
    pub state: BuildState,
    pub finished_at: Option<DateTime<Utc>>,
    pub jobs: Vec<Job>,
}
```

Do not invent a `finished` field. `Build::is_finished()` can derive that answer
from its typed state or `finished_at`.

### `airborne-pr`

This is the first `SubjectMonitor`. It owns PR-specific rule definitions and
the cross-provider resolution that makes them work:

```rust
pub enum PrRule {
    CheckCompletes { check_name: String },
    BuildkiteJobCompletes {
        github_status_context: String,
        expected_build: BuildkitePipeline,
        job_name: String,
        notify_on: NotifyOn,
    },
}
```

The monitor works in two phases:

1. Fetch the GitHub PR once and get its current head SHA.
2. Build a fetch plan from enabled rules, fetch each necessary source once, then
   return one `Candidate` or source error per rule.

For v0, the rules work as follows:

- **Cursor Bugbot:** find the exact `Cursor Bugbot` check run for the head SHA.
  It completes when `status == "completed"`, regardless of conclusion. The
  Discord fixture used `conclusion: "neutral"`.
- **Buildkite job:** find the latest GitHub commit status with the rule's exact
  context, parse its target URL as the configured Buildkite pipeline and build,
  fetch that one build, then match its exact job name.

The monitor must group Buildkite rules by `BuildRef`, fetching a build once even
when several rules select its jobs. A GitHub status fetch failure affects only
Buildkite rules; a check-run fetch failure affects only check rules. A PR-head
fetch failure affects the whole PR because there is no trustworthy revision.

`airborne-pr` may use typed enums for this v0. Do not replace them with a
string-keyed fact map or an arbitrary expression language. Adding a subject
later means adding another monitor, its typed rules, and its tests.

### `airborne-runtime`

Own the application service, not a provider:

- Select active watches due for a poll.
- Ensure one in-process refresh at a time.
- Group work by watched subject.
- Call the appropriate registered `SubjectMonitor`.
- Feed successful candidates to `airborne-core::reconcile`.
- Persist observations and new alerts atomically.
- Call the alert effect only after the alert commit succeeds.
- Record source failures as displayable status, never as alerts.

Use narrow ports:

```rust
pub trait WatchStore { /* watches, rules, prior state, atomic apply */ }
pub trait SubjectMonitor { /* poll one typed subject */ }
pub trait AlertEffect { fn deliver(&self, alert: &Alert); }
```

The runtime must return a refresh report to the host: how many watches ran,
which sources failed, and how many alerts were inserted. It must not suppress a
successful candidate because another rule or source failed.

### `airborne-store-sqlite`

Implement `WatchStore` with SQLite. It owns schema and migrations, not rule
policy. Persist:

- subjects and watches;
- rules, including a monotonic `rule_version`;
- the latest observation for each `(rule, rule_version, revision)`;
- immutable alerts and their read state;
- settings that are not secrets.

The unique alert index must use the full alert key from `airborne-core`.

### `airborne-desktop`

Compose the system and contain all platform details:

- Tauri commands and React dashboard.
- Keychain-backed credential provider.
- A single background task that asks `airborne-runtime` to refresh at the chosen
  interval.
- Native notifications and tray updates from committed alerts.
- Menubar blink state based only on unread alert count.

It must not decode provider JSON, decide rule state, or talk directly to SQLite
outside the store adapter.

## Watch and event model

Keep these terms separate:

| Term | Meaning |
| --- | --- |
| Subject | The remote thing being watched, initially a GitHub PR. |
| Watch | The user's local record for a subject and its rules. |
| Revision | The version against which a result applies; for a PR, its head SHA. |
| Rule | A condition that one monitor can evaluate for that subject. |
| Candidate | A monitor's current result for a rule, before lifecycle policy. |
| Alert | An immutable, user-visible event created by the lifecycle engine. |

Lifecycle rules:

1. The first successful candidate for a new rule version is a baseline; do not
   alert even if it is already complete.
2. A later completion on the same revision alerts once per source identity.
3. A completion for a new revision alerts, even if the first poll after a push
   sees the completed result.
4. A failed request does not advance the baseline and does not alert.
5. Editing a rule increments its version, then its next successful candidate is
   a new baseline.

## What to reuse from the other tools

Do not make Airborne depend on Stalker or marker-watch-mcp. It neither watches
files nor interprets a child process's stdout.

Do not put `foxrun` in the core either. The Tauri desktop app owns one runtime,
so it should enforce one in-process refresh and use the normal single-instance
desktop-app behavior. If we later ship a headless `airborne run` command that
can be launched independently by several callers, that binary can opt into
foxrun's existing cwd-and-command lease instead of rebuilding process
deduplication.

The useful shared idea is the boundary: each component owns one durable thing
and exposes a narrow protocol. Airborne's version is a subject monitor and
event lifecycle, not stdout markers or filesystem events.

## Delivery plan

1. Create the workspace and implement `airborne-core` with table tests for all
   baseline, revision, rule-edit, and deduplication transitions.
2. Implement GitHub and Buildkite clients from captured JSON fixtures. Include
   a Buildkite build fixture with `state` and `finished_at` so the old decoding
   error cannot return.
3. Implement `airborne-pr` with a fetch-plan test: Bugbot-only rules do not
   call statuses or Buildkite; two jobs in one build cause one Buildkite call;
   one source failure does not block other rules.
4. Implement SQLite and runtime atomic-apply tests, including restart
   deduplication.
5. Replace the Tauri host with a thin adapter and restore the dashboard, tray,
   and notifications.
6. Run one real read-only smoke test against the Discord fixture PR and a
   configured Buildkite token. Then verify the menubar and notifications on
   macOS.

Do not start with the dashboard. The engine and provider fixtures are the
product's hard part; the desktop shell should be the final, thin layer.
