# GitHub contract fixtures

Captured shape: 2026-09-08. All values are scrubbed; no fixture contains a
credential, private repository name, account name, or live URL.

| Request path | Fixture | Expected use |
| --- | --- | --- |
| `GET /repos/OWNER/REPOSITORY/pulls/NUMBER` | `pull-request.json` | title and head SHA success |
| `GET /repos/OWNER/REPOSITORY/commits/SHA/check-runs?per_page=100` | `check-runs-page-1.json`, `check-runs-page-2.json` | decoded check runs and `Link` paging |
| `GET /repos/OWNER/REPOSITORY/commits/SHA/status?per_page=100` | `statuses-page-1.json`, `statuses-page-2.json` | decoded commit statuses and paging |
| provider response | `missing-head-sha.json`, `malformed.json` | required-field and type failures |
| HTTP 401 | `auth-rejected.json` | rejected credential |
| HTTP 429 or exhausted HTTP 403 | `rate-limited.json` | rate-limit response; `Retry-After` is test-controlled |
| HTTP 502, 503, or 504 | `server-error.json` | retryable server failure |
