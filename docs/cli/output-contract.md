# Human output contract

This document fixes the human output for `status`, `watch`, `rule`, and
`alerts`. It complements the stable JSON contract in
[`requirements.md`](requirements.md). JSON remains uncolored and keeps its
existing machine-readable fields; human output may evolve only by updating this
contract.

## Shared rules

- Every human command uses ANSI styling when standard output is an interactive
  terminal. `--no-color` and `NO_COLOR` disable it.
- Headings and primary entity values are bold. Primary values are the pull
  request (`owner/repo #number — title`), rule name, and alert name.
- Green marks active, enabled, completed, and successful states. Yellow marks
  paused, pending, acknowledged, waiting, started, and in-progress states. Red
  marks failed, unavailable, missing, and not-detected states. Never rely on
  color alone.
- IDs, URLs, revisions, versions, and timestamps are dim. Keep IDs on their
  own final line or after the useful content; do not lead a row with an ID.
- Human timestamps use the user's local time zone and the form
  `Sep 10, 2026 at 2:41 PM EDT`. JSON timestamps remain UTC RFC 3339.
- Empty sections say what is empty, such as `No pending alerts`.

The samples use styling labels rather than literal escape sequences:
`[bold]`, `[green]`, `[yellow]`, `[red]`, and `[dim]`.

## `airborne status`

```text
[bold]Airborne status[/bold]
2 watches  ·  1 pending alert

[bold]discord/discord #12345 — Add guild profiles[/bold]  [green]active[/green]
  Cursor Bugbot  [yellow]in progress[/yellow]
    [dim]observed Sep 10, 2026 at 2:41 PM EDT[/dim]
  Buildkite · test-linux  [green]completed[/green]
    [dim]observed Sep 10, 2026 at 2:40 PM EDT[/dim]
  [dim]Revision a8d3c19[/dim]
  [dim]Checked Sep 10, 2026 at 2:41 PM EDT  ·  last poll success[/dim]
  [dim]watch_01J8…  ·  github.com/discord/discord/pull/12345[/dim]

[bold]airborne/airborne #87 — Improve status output[/bold]  [yellow]paused[/yellow]
  Cursor Bugbot  [red]not detected[/red]
    [dim]observed Sep 10, 2026 at 1:40 PM EDT[/dim]
    [red]Issue: GitHub check runs are unavailable[/red]
  [yellow]1 pending alert[/yellow]
  [red]Issue: GitHub is unavailable[/red]
  [dim]Revision f49ca20[/dim]
  [dim]Checked Sep 10, 2026 at 1:40 PM EDT  ·  last poll partial[/dim]
  [dim]watch_01J9…  ·  github.com/airborne/airborne/pull/87[/dim]
```

## `airborne watch list`

```text
[bold]Watches[/bold]
2 total  ·  1 active  ·  1 paused

[bold]discord/discord #12345 — Add guild profiles[/bold]  [green]active[/green]
  2 rules  ·  [dim]checked Sep 10, 2026 at 2:41 PM EDT[/dim]
  [dim]https://github.com/discord/discord/pull/12345[/dim]
  [dim]watch_01J8…[/dim]

[bold]airborne/airborne #87 — Improve status output[/bold]  [yellow]paused[/yellow]
  1 rule  ·  1 pending alert  ·  [dim]checked Sep 10, 2026 at 1:40 PM EDT[/dim]
  [dim]https://github.com/airborne/airborne/pull/87[/dim]
  [dim]watch_01J9…[/dim]
```

The canonical pull request URL shown here may be passed directly to
`watch show` and `watch remove`. Both commands continue to accept a watch ID.

## `airborne watch show`

```text
[bold]discord/discord #12345 — Add guild profiles[/bold]  [green]active[/green]

Pull request   [dim]https://github.com/discord/discord/pull/12345[/dim]
Revision       [dim]a8d3c19[/dim]
Last checked   [dim]Sep 10, 2026 at 2:41 PM EDT[/dim]

[bold]Rules[/bold]
  [bold]Cursor Bugbot[/bold]  [yellow]in progress[/yellow]
    Alert when started  ·  alert if missing after 10 minutes

  [bold]Buildkite · test-linux[/bold]  [green]completed[/green]
    Alert on terminal result

[bold]Alerts[/bold]
  No pending alerts

[dim]watch_01J8…[/dim]
```

## `airborne rule list`

```text
[bold]Rules[/bold]
3 total  ·  3 enabled

[bold]Cursor Bugbot[/bold]  [green]enabled[/green]
  [bold]discord/discord #12345 — Add guild profiles[/bold]
  [yellow]in progress[/yellow]  ·  [dim]checked Sep 10, 2026 at 2:41 PM EDT[/dim]
  [dim]rule_01JA…[/dim]

[bold]Buildkite · test-linux[/bold]  [green]enabled[/green]
  [bold]discord/discord #12345 — Add guild profiles[/bold]
  [green]completed[/green]  ·  [dim]checked Sep 10, 2026 at 2:40 PM EDT[/dim]
  [dim]rule_01JB…[/dim]

[bold]Cursor Bugbot[/bold]  [green]enabled[/green]
  [bold]airborne/airborne #87 — Improve status output[/bold]
  [red]not detected[/red]  ·  missing for 14 minutes
  [dim]rule_01JC…[/dim]
```

## `airborne rule show`

Use the name shown by `rule list` in place of the rule ID. `bugbot` is a short
name for a GitHub check rule. If the same name exists on more than one watch,
select the watch with its pull request URL:

```text
airborne rule show bugbot --watch https://github.com/discord/discord/pull/12345
airborne rule disable buildkite/discord-admin --watch https://github.com/discord/discord/pull/12345
```

```text
[bold]Cursor Bugbot[/bold]  [green]enabled[/green]
[bold]discord/discord #12345 — Add guild profiles[/bold]

Check name       Cursor Bugbot
Current state    [yellow]in progress[/yellow]
Last observed    [dim]Sep 10, 2026 at 2:41 PM EDT[/dim]
Revision         [dim]a8d3c19[/dim]

[bold]Alerts[/bold]
  When detected
  If missing for 10 minutes
  When completed

[dim]rule_01JA…  ·  version 3[/dim]
```

For a Buildkite rule, the rule-show detail replaces `Check name` with the
configured matching fields. It keeps the common state, observation, revision,
alert policy, and dim ID/version lines shown above.

```text
[bold]Buildkite · test-linux[/bold]  [green]enabled[/green]
[bold]discord/discord #12345 — Add guild profiles[/bold]

Job              test-linux
Context          buildkite/test-linux
Organization     discord
Pipeline         client
Current state    [green]completed[/green]
Last observed    [dim]Sep 10, 2026 at 2:40 PM EDT[/dim]
Revision         [dim]a8d3c19[/dim]

[bold]Alerts[/bold]
  On terminal result

[dim]rule_01JB…  ·  version 1[/dim]
```

## `airborne alerts list`

```text
[bold]Pending alerts[/bold]
1 alert

[bold]Cursor Bugbot was not detected[/bold]  [yellow]pending[/yellow]
  [bold]airborne/airborne #87 — Improve status output[/bold]
  Missing for 14 minutes  ·  [dim]Sep 10, 2026 at 1:30 PM EDT[/dim]
  [dim]alert_01JD…[/dim]
```

## `airborne alerts show`

```text
[bold]Cursor Bugbot was not detected[/bold]  [yellow]pending[/yellow]

Pull request    [bold]airborne/airborne #87 — Improve status output[/bold]
Rule            Cursor Bugbot
Event           [red]Missing[/red]
Detected        [dim]Sep 10, 2026 at 1:30 PM EDT[/dim]
Revision        [dim]f49ca20[/dim]

Cursor Bugbot did not appear within 10 minutes of this revision.

[dim]https://github.com/airborne/airborne/pull/87[/dim]
[dim]alert_01JD…[/dim]
```
