# Known limits and external release gates

## Version 1 limits

- Airborne supports macOS only. Windows and Linux are not supported releases.
- It watches GitHub pull requests only. It does not support issues, other Git hosts, webhooks, sync, OAuth, hosted accounts, job control, or CI log reads.
- It polls in the foreground. It does not install a background service or send native notifications.
- One `run` process may own a data directory. A refresh may run while it sleeps, but refreshes for that directory do not overlap.
- Poll intervals range from 30 seconds to 24 hours. Runtime concurrency is fixed at four subjects and eight provider requests.
- Buildkite matching is exact and case-sensitive for context, organization, pipeline, and job name.

## External gates before release

Do not mark a release ready until these gates have recorded evidence:

- An unsigned archive and checksum pass the packaging smoke test for every supported macOS CPU. Code signing and notarization are separate external release gates; do not publish until both have recorded evidence.
- The CI sequence passes: format, Clippy, tests, release build, and command help checks.
- User-supplied local credentials complete the read-only GitHub and Buildkite live-validation record in [live-validation.md](live-validation.md).
- A 24-hour foreground soak of the release binary completes with no stuck lease, duplicate alert, runaway memory, lost committed result, or secret in saved evidence. Use `scripts/release-soak.sh`.
- Prototype migration has upgrade and idempotency evidence before the desktop prototype is removed.

If a required environment is unavailable, record the gate as blocked. Do not report it as passed.
