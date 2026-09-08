# Domain model and shared language

## 1. Purpose

This document names the things Airborne stores and the rules that change them.
Code, database schemas, command help, and tests must use these names. Provider
payload names stay inside provider components unless this document adopts them.

## 2. Shared language

| Term | Meaning |
| --- | --- |
| **Subject** | A remote thing Airborne can inspect. Version 1 supports a GitHub pull request. |
| **Subject key** | A stable identity for a subject, such as GitHub host, owner, repository, and pull request number. |
| **Revision** | The version of a subject against which rules run. For a pull request, this is the full head commit SHA. |
| **Watch** | The user's local choice to monitor one subject. A watch may be active, paused, or archived. |
| **Rule** | A typed condition attached to one watch. |
| **Rule version** | A positive number that changes when matching or alert policy changes, or when a rule is re-enabled. |
| **Fetch plan** | The smallest set of provider calls needed to evaluate the enabled rules for one subject. |
| **Candidate** | One successful rule evaluation for one rule version and revision, before lifecycle policy. |
| **Source identity** | The stable provider identity of the item that produced a candidate, such as a check-run ID or stable set of job IDs. |
| **Observation** | The durable latest candidate for one rule version and revision. |
| **Source issue** | A provider, decoding, or resolution failure. It is not a rule result and cannot cause an alert. |
| **Reconciliation** | The pure decision that turns a candidate and prior history into a new observation and optional alert draft. |
| **Alert** | An immutable durable event produced by reconciliation. |
| **Acknowledgement** | The user's explicit statement that an alert no longer needs attention. |
| **Poll attempt** | One attempt to inspect one watched subject during a refresh. |
| **Refresh** | One bounded pass over one watch or all active watches. |
| **Refresh report** | The returned summary of attempted work, issues, saved results, and new alerts. |
| **Runner** | The foreground process that starts refreshes on an interval. |
| **Lease** | A local, time-bounded claim that prevents conflicting runner or refresh work. |

Do not use `notification` as a synonym for alert. A notification is a possible
delivery effect outside the version 1 domain. Do not use `read` or `unread` for
alerts; use `pending` and `acknowledged` in displays and `acknowledged_at` in
the model.

## 3. Identities and value types

The core crate must use checked value types rather than raw strings or integers
at component boundaries:

```text
WatchId
RuleId
RuleVersion
AlertId
PollAttemptId
SubjectKey
Revision
SourceIdentity
GitHubOwner
GitHubRepository
GitHubPullRequestNumber
GitHubRepositoryKey
GitHubPullRequestKey
GitHubStatusContext
BuildkiteOrganization
BuildkitePipeline
BuildkiteBuildNumber
BuildkiteBuildKey
BuildkiteJobName
```

- Local IDs must remain stable for the life of a record.
- `RuleVersion` starts at `1` and increases without reuse.
- Provider numeric IDs must be stored as strings when their range or format is
  controlled by the provider.
- A `Revision` must preserve the full provider value. The CLI may display a
  short form but JSON and storage must not truncate it.
- A subject key must be canonical. Equivalent GitHub URLs must not create two
  watches.

## 4. Aggregate records

### 4.1 Subject

```text
Subject
  key
  kind: github_pull_request
  canonical_url
  display_title
  current_revision?
  metadata_refreshed_at?
  created_at
```

The subject is remote identity and cached display data. It does not contain
user policy. A later subject kind gets its own typed key and monitor.

### 4.2 Watch

```text
Watch
  id
  subject_key
  state: active | paused | archived
  created_at
  updated_at
  archived_at?
```

There may be at most one non-archived watch for a subject key. Pausing a watch
stops all its rules without changing their versions. Resuming a watch creates a
new version and baseline for every enabled rule in one transaction. Use rule
disable and enable when the same behavior is needed for one rule.

Archiving a watch archives its rules. It does not delete its subject, history,
or alerts.

### 4.3 Rule and rule version

```text
Rule
  id
  watch_id
  kind
  enabled
  current_version
  created_at
  updated_at
  archived_at?

RuleDefinition
  rule_id
  version
  config
  created_at
```

Rule definitions are append-only. Updating a match field or alert policy adds
a definition and increments `current_version` in one transaction. Enabling a
disabled rule also copies its current definition into a new version. Resuming
a watch does the same for each enabled rule. Disabling, renaming for display,
pausing a watch, or archiving does not change a version.

Version 1 has two rule kinds:

```text
GitHubCheckCompletes
  check_name

BuildkiteJobCompletes
  github_status_context
  expected_organization
  expected_pipeline
  job_name
  notify_on: terminal | passed
```

The provider-neutral state is not stored in the rule definition.

### 4.4 Candidate

```text
Candidate
  rule_key: (rule_id, rule_version)
  subject_key
  revision
  state: waiting | in_progress | completed | failed | unavailable
  source_identity?
  source_url?
  detail?
  alert_intent?
  observed_at
```

`Candidate` is a value returned by a subject monitor. It is not stored as-is.
The lifecycle engine checks it first and returns an observation plus an
optional alert draft.

State meanings:

| State | Meaning |
| --- | --- |
| `waiting` | The expected provider item does not exist yet, but it may appear. |
| `in_progress` | The provider item exists and has not reached the rule's terminal condition. |
| `completed` | The item reached an accepted successful or neutral completion. |
| `failed` | The item reached a known non-success terminal result. |
| `unavailable` | Providers responded correctly, but the rule cannot evaluate the result, such as a finished build with no matching job. |

`unavailable` is a valid candidate. A timeout, rejected credential, malformed
provider response, rate limit, or network error is a source issue, not an
unavailable candidate.

An alert intent contains the source identity, title, body, and whether the
candidate satisfies the rule's chosen alert policy. A candidate may be failed
and still carry an alert intent when `notify_on` is `terminal`.

### 4.5 Observation

```text
Observation
  rule_id
  rule_version
  revision
  state
  source_identity?
  source_url?
  detail?
  observed_at
```

There is one current observation for each `(rule_id, rule_version, revision)`.
A later successful candidate replaces that row. Past rule versions and
revisions remain available for lifecycle decisions and diagnosis.

Source issues never overwrite an observation.

### 4.6 Alert

```text
Alert
  id
  key: (rule_id, rule_version, revision, source_identity)
  watch_id
  subject_key
  rule_kind
  title
  body
  source_url?
  created_at
  acknowledged_at?
```

The key is unique. The alert copies the subject and rule details needed to show
useful history after a watch or rule is archived. The event fields are
immutable. Only `acknowledged_at` may change.

An alert with no acknowledgement is pending. Acknowledgement does not alter an
observation or prevent a later alert with a different key.

### 4.7 Poll attempt and source issue

```text
PollAttempt
  id
  refresh_id
  watch_id
  subject_key
  revision?
  started_at
  finished_at
  outcome: success | partial | failure | canceled

SourceIssue
  poll_attempt_id
  scope: subject | rule | source
  rule_id?
  provider: github | buildkite
  kind
  safe_message
  retryable
```

Issues are safe, structured summaries. They must not store credentials or full
private response bodies. A subject issue may prevent all rule evaluation. A
source or rule issue affects only the rules that depend on it.

The latest poll attempt supports `airborne status`. Retention of older attempts
is storage policy, not lifecycle policy.

## 5. Rule evaluation

### 5.1 GitHub check completion

For the current pull request revision:

1. Fetch check runs only if an enabled check rule needs them.
2. Match the configured check name exactly.
3. No match yields `waiting` with no source identity.
4. Any matched non-completed check yields `in_progress`.
5. A matched check with `status == "completed"` yields `completed` and an alert
   intent regardless of conclusion.
6. Include the conclusion in detail and alert text when present. `neutral` is a
   completion, not a failure.
7. Use the GitHub check-run ID as source identity.

If GitHub returns several exact-name checks, select the newest by provider
start time, then by numeric ID as a stable tie-break. Provider contract tests
must fix this ordering.

### 5.2 Buildkite job completion

For the current pull request revision:

1. Fetch commit statuses only if an enabled Buildkite rule needs them.
2. Select the newest status with the exact configured context by creation time,
   then by numeric status ID as a stable tie-break.
3. Require its target to be the canonical public Buildkite build URL for the
   configured organization and pipeline.
4. Group rules by the derived build reference and fetch each build once.
5. Match all current jobs with the exact configured name.
6. No match yields `waiting` while the build can still change and `unavailable`
   after the build is finished.
7. Any matched nonterminal job yields `in_progress`.
8. All matched jobs terminal yields `completed` if all passed and `failed` if
   any did not pass.
9. `notify_on: terminal` creates alert intent for completed or failed results.
   `notify_on: passed` creates it only when all jobs passed.

Terminal job states are `passed`, `failed`, `timed_out`, `canceled`, `skipped`,
`broken`, and `expired`. An unknown job or build state must produce a source
issue until code and fixtures adopt it. It must not be guessed terminal.

One matched job uses its Buildkite job ID as source identity. Several matched
jobs use a versioned stable hash of their sorted job IDs. The hash input format
is part of the core contract and needs fixed test vectors.

Build completion derives from Buildkite's typed `state` and `finished_at`.
Airborne must not expect a `finished` field in the response.

## 6. Reconciliation and alert lifecycle

The lifecycle engine is a pure function:

```rust
pub fn reconcile(
    candidate: Candidate,
    history: RuleHistory,
) -> Reconciliation;

pub struct Reconciliation {
    pub observation: Observation,
    pub alert: Option<AlertDraft>,
}
```

`RuleHistory` supplies the latest successful observation across revisions for
the same rule version and whether the candidate's alert key already exists.

Apply these rules in order:

1. A source issue never calls `reconcile`.
2. The first candidate for a rule version is its baseline. Save the observation
   and suppress its alert intent, even if the candidate is terminal.
3. After a baseline, a candidate for a new revision is armed. If it carries an
   alert intent, create an alert even when the first candidate seen for that
   revision is already terminal.
4. On the same revision, create an alert when an eligible source identity has
   not appeared in the alert key before. Waiting and in-progress observations
   followed by a terminal intent therefore alert.
5. Never create an alert when the candidate has no alert intent.
6. Never create an alert when its full key already exists.
7. Always return the new observation, including when the alert is suppressed.

This gives these required cases:

| Prior history | Candidate | Result |
| --- | --- | --- |
| No candidate for rule version | completed | Baseline, no alert |
| Baseline waiting, same revision | completed, new identity | Alert |
| Baseline completed, same revision | same identity | No alert |
| Any baseline, new revision | completed | Alert |
| Any history | source issue | No observation, no alert |
| Rule updated or re-enabled | completed | New-version baseline, no alert |

Observation and alert insertion must commit in the same transaction. The
database unique key is the final guard against duplicates.

## 7. Pause, disable, update, and archive behavior

These actions have different meanings:

- **Pause watch:** stop all polling for it. Resume creates a new version and
  baseline for each enabled rule, so events reached while paused do not alert.
- **Disable rule:** stop evaluating one rule. Enable creates a new version and
  baseline, so results reached while disabled do not alert.
- **Update rule:** create a new version and baseline.
- **Archive rule:** stop evaluation and hide it from normal lists. Keep history.
- **Archive watch:** stop polling, archive its rules, and keep history.

Commands and docs must not use these words as if they were interchangeable.

## 8. Changes from the desktop prototype

The CLI model makes these required changes:

- Split remote `Subject` identity from the user's `Watch` policy.
- Replace rule `config_json` as a public contract with typed rule definitions.
  SQLite may still encode definitions, but repositories return typed values.
- Add append-only rule definitions and monotonic rule versions.
- Rearm enabled rules with new baselines when a paused watch resumes.
- Include rule version in observation and alert identity.
- Replace alert `read` state with optional acknowledgement time.
- Archive watches and rules instead of cascading deletion.
- Copy useful subject and rule details into alerts so history remains readable.
- Add poll attempts and scoped source issues instead of one watch-level error.
- Remove pipeline mappings from the core model; the Buildkite rule holds its
  full expected identity.
- Remove tray, window, dashboard, and notification state from the domain.
- Make the refresh report a first-class return value for human and JSON output.
