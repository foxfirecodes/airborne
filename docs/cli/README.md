# Airborne CLI specification

## Status

This directory defines Airborne as a production CLI. It is the source of truth
for new work.

The older [`docs/v0.md`](../v0.md) and
[`docs/modular-architecture.md`](../modular-architecture.md) describe the first
desktop app and the design work that followed it. They remain as history. If
they conflict with this directory, this directory wins.

## Product decision

Airborne is a local command-line tool that watches pull requests and records
alerts when selected checks or Buildkite jobs finish. A Bugbot rule may also
alert when its check starts or stays undetected past a configured delay. It has
no required
desktop app, web view, tray icon, or hosted service.

The CLI is the product host, not a test shell around a future desktop app. A
later desktop host may use the same runtime, but it must not move policy out of
the shared crates or weaken the CLI contract.

## Documents

- [`requirements.md`](requirements.md) defines the CLI, production needs,
  milestones, completion gates, and validation plan.
- [`domain-model.md`](domain-model.md) defines Airborne's shared language,
  records, state changes, and alert rules.
- [`architecture.md`](architecture.md) defines components, dependency rules,
  and contracts.

## How to use these documents

Each requirement uses one of these words:

- **Must**: required for the production-ready CLI.
- **Should**: expected unless a later decision records a sound reason to differ.
- **May**: allowed but not required.

Code, tests, command help, and storage migrations must use the names in the
domain model. A change to product behavior must update these documents in the
same change as the code. This specification deliberately leaves a broader CLI
UX redesign for later work.
