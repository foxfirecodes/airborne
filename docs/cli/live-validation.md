# Live validation record

Use this record for a user-approved, read-only validation against a known pull request. Do not include tokens, authorization headers, full provider payloads, or private pull-request content.

## Run

| Field | Record |
| --- | --- |
| Date and UTC time | |
| Airborne version and artifact checksum | |
| macOS version and CPU type | |
| Data directory | Redacted local path if needed |
| Pull request | Redacted or approved canonical URL |
| Rules | Exact check, status context, organization, pipeline, job, and notify policy |
| Credential source | Environment, Keychain, or missing; never token text |

## Evidence

| Required proof | Command or redacted result | Exit status |
| --- | --- | ---: |
| Local storage and credentials | `airborne doctor` | |
| Read-only provider access | `airborne doctor --live` | |
| GitHub PR and head revision lookup | `airborne refresh` | |
| Bugbot completed neutral evaluation | Redacted refresh result | |
| GitHub status resolves to the configured Buildkite build | Redacted refresh result | |
| Buildkite decodes `state` and `finished_at` | Redacted refresh result | |
| Exact Buildkite job match | Redacted refresh result | |
| Second refresh adds no duplicate alert | Alert counts before and after | |
| Runner lease contention | Second `airborne run` exits 4 | |
| Signal shutdown | PID, signal, elapsed time, and released lease | |

## Result

- Overall outcome: pass / fail / blocked
- Failed or blocked gate, with non-secret evidence:
- Follow-up owner and date:

The 24-hour soak is recorded separately. A successful API call does not prove runner durability or signal handling.
