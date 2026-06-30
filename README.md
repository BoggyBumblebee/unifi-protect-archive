# UniFi Protect Archive

[![Build](https://github.com/boggybumblebee/unifi-protect-archive/actions/workflows/build.yml/badge.svg)](https://github.com/boggybumblebee/unifi-protect-archive/actions/workflows/build.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=boggybumblebee_unifi-protect-archive&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=boggybumblebee_unifi-protect-archive)
[![Bugs](https://sonarcloud.io/api/project_badges/measure?project=boggybumblebee_unifi-protect-archive&metric=bugs)](https://sonarcloud.io/summary/new_code?id=boggybumblebee_unifi-protect-archive)
[![Code Smells](https://sonarcloud.io/api/project_badges/measure?project=boggybumblebee_unifi-protect-archive&metric=code_smells)](https://sonarcloud.io/summary/new_code?id=boggybumblebee_unifi-protect-archive)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=boggybumblebee_unifi-protect-archive&metric=coverage)](https://sonarcloud.io/summary/new_code?id=boggybumblebee_unifi-protect-archive)
[![Duplicated Lines (%)](https://sonarcloud.io/api/project_badges/measure?project=boggybumblebee_unifi-protect-archive&metric=duplicated_lines_density)](https://sonarcloud.io/summary/new_code?id=boggybumblebee_unifi-protect-archive)

Rust CLI for creating UniFi Protect **Video Archiving** tasks.

This tool does not download recordings to the machine running it. It logs into the local UniFi OS console and calls Protect's built-in archive API so Protect writes the recording archive to a preconfigured storage destination, such as a UniFi Drive/NAS shared drive.

## What This Does

- Creates Protect Video Archiving jobs for selected cameras and time ranges.
- Sends archive requests one at a time.
- Waits for each archive job to leave the pending queue before submitting the next job.
- Uses Protect's configured archive destination, such as UniFi Drive/NAS.
- Can optionally delete the archived Protect footage after each archive task completes. This is off by default and requires an explicit confirmation flag or local confirmation string.

## What We Learned

- UniFi API keys can read cameras from `/proxy/protect/integration/v1/cameras`.
- API keys do not work for Protect's private Video Archiving endpoints.
- Video Archiving task creation requires a UniFi OS login session cookie.
- Archive POST requests also require the `x-csrf-token` returned by login.
- MFA/SSO accounts are not suitable for this headless tool.
- The NAS/Drive archive provider is visible per Protect user. The service user must have the archive destination linked in Protect.

## Setup Checklist

### 1. Create a Local Service User

Create a local UniFi OS / Protect user for automation.

Recommended properties:

- Local console user, not a cloud-only SSO account.
- No MFA requirement.
- Can log into the local UniFi OS console.
- Has Protect access.
- Has permission to read camera media and create archive tasks.

Example service account name:

```text
archive-service-user
```

### 2. Link Video Archiving for That User

Log into Protect as the service user and configure Video Archiving / NAS / UniFi Drive for that same user.

For your local setup, put the real values in `protect-archive.local.toml`:

```toml
archive_host = "nas.example.invalid"
archive_shared_drive = "ProtectArchive"
```

Protect may store this as a per-user archive provider. If the service user can list cameras but archive requests return `403 Forbidden`, check this linkage first.

### 3. Create Local Secrets

Create `.env` in the repo root.

Example:

```sh
UNIFI_PROTECT_USERNAME='archive-service-user'
UNIFI_PROTECT_PASSWORD='service-user-password'
```

`.env` is ignored by git. Do not commit it.

You may keep `UNIFI_PROTECT_API_KEY` in `.env` for camera-listing experiments, but `auth_method = "password"` is required for Video Archiving because the archive endpoints do not accept the API key.

### 4. Create Config

Copy the commit-safe template to a local ignored config:

```sh
cp protect-archive.example.toml protect-archive.local.toml
```

Edit `protect-archive.local.toml` with your real local values:

```toml
controller = "https://unifi-console.example.invalid"
auth_method = "password"
api_key_env = "UNIFI_PROTECT_API_KEY"
username_env = "UNIFI_PROTECT_USERNAME"
password_env = "UNIFI_PROTECT_PASSWORD"
archive_destination = "NAS"
archive_host = "nas.example.invalid"
archive_shared_drive = "ProtectArchive"
camera_ids = ["camera-id-or-camera-name"]
segment_seconds = 900
lookback_seconds = 3600
minimum_age_seconds = 120
poll_seconds = 300
archive_status_poll_seconds = 15
wait_for_archive_completion = true
delete_after_archive = false
delete_after_archive_confirmation = ""
verify_tls = false
```

`protect-archive.local.toml` is ignored by git because it contains local environment details.

TOML string values must be quoted. Camera IDs should look like:

```toml
camera_ids = ["camera-id-or-camera-name"]
```

not:

```toml
camera_ids = [camera-id-or-camera-name]
```

### 5. List Cameras

Run:

```sh
cargo run -- cameras --config protect-archive.local.toml
```

Use the returned camera IDs or names in `camera_ids` or with the `--camera` flag.

### 6. Run a Small Archive Test

Start with one camera and a five-minute range that ended at least ten minutes ago:

```sh
cargo run -- run-once \
  --config protect-archive.local.toml \
  --camera camera-id-or-camera-name \
  --start 2026-06-30T13:38:27Z \
  --end 2026-06-30T13:43:27Z
```

A successful run exits with code `0`. The test above successfully created a Protect archive task and the pending archive count was `0` afterward.

Use RFC 3339 timestamps with a timezone:

```text
2026-06-30T13:38:27Z
2026-06-30T14:38:27+01:00
```

### 7. Optional: Delete After Archiving

Deletion is disabled by default.

When enabled, the tool waits for each Protect archive task to leave the pending queue, then calls Protect's media deletion API for the exact camera and time range that was just archived. This removes that footage from Protect storage. Treat this as permanent.

For a one-off run, both flags are required:

```sh
cargo run -- run-once \
  --config protect-archive.local.toml \
  --camera camera-id-or-camera-name \
  --start 2026-06-30T13:38:27Z \
  --end 2026-06-30T13:43:27Z \
  --delete-after-archive \
  --i-understand-this-deletes-protect-footage
```

For daemon use, set both values in `protect-archive.local.toml`:

```toml
delete_after_archive = true
delete_after_archive_confirmation = "DELETE_PROTECT_FOOTAGE_AFTER_ARCHIVE"
```

The tool refuses to delete if `wait_for_archive_completion = false` or if Protect's archive response cannot be tracked with a `fileId`.

### 8. Run a Rolling Archive Pass

With `lookback_seconds = 3600`, this archives the previous hour, stopping short of very recent footage by `minimum_age_seconds`:

```sh
cargo run -- run-once --config protect-archive.local.toml
```

### 9. Run Continuously

```sh
cargo run -- daemon --config protect-archive.local.toml
```

The daemon reloads config each pass and waits `poll_seconds` between passes.

## Commands

Write sample config:

```sh
cargo run -- init-config
```

List cameras:

```sh
cargo run -- cameras --config protect-archive.local.toml
```

Archive a specific range:

```sh
cargo run -- run-once \
  --config protect-archive.local.toml \
  --camera "Camera Name" \
  --start "2026-06-30T09:00:00+01:00" \
  --end "2026-06-30T10:00:00+01:00"
```

Archive and then delete that exact Protect time range:

```sh
cargo run -- run-once \
  --config protect-archive.local.toml \
  --camera "Camera Name" \
  --start "2026-06-30T09:00:00+01:00" \
  --end "2026-06-30T10:00:00+01:00" \
  --delete-after-archive \
  --i-understand-this-deletes-protect-footage
```

Run continuously:

```sh
cargo run -- daemon --config protect-archive.local.toml
```

## SonarCloud

This repo includes SonarCloud configuration copied from the shape used by `BoggyBumblebee/squirrel-feed-alphavantage` and adapted for this Rust CLI.

Required GitHub setup:

- Create/import the SonarCloud project with key `boggybumblebee_unifi-protect-archive`.
- Add a repository secret named `SONAR_TOKEN`.

The workflow runs on pushes to `main`/`master` and on pull requests. It generates `lcov.info` with `cargo-llvm-cov`, then runs the SonarCloud scanner.

Run coverage locally:

```sh
scripts/coverage.sh
```

To enforce a local minimum line coverage threshold:

```sh
COVERAGE_FAIL_UNDER_LINES=80 scripts/coverage.sh
```

## Configuration Reference

- `controller`: Base URL for the UniFi OS console.
- `auth_method`: `auto`, `api-key`, or `password`. Use `password` for Video Archiving.
- `api_key_env`: Environment variable containing the API key. Defaults to `UNIFI_PROTECT_API_KEY`.
- `username_env`: Environment variable containing the local username.
- `password_env`: Environment variable containing the local password.
- `archive_destination`: Use `NAS` for a UniFi Drive/NAS-backed share.
- `archive_host`: Host/IP of the configured storage target.
- `archive_shared_drive`: Share/drive name selected in Protect.
- `camera_ids`: Optional list of camera IDs or camera names. Empty means all visible cameras.
- `segment_seconds`: Length of each Protect archive task.
- `lookback_seconds`: Rolling lookback used when no explicit `--start`/`--end` is provided.
- `minimum_age_seconds`: Avoid archiving very recent footage that Protect may still be writing.
- `wait_for_archive_completion`: Keep `true` to submit the next task only after the previous task is no longer pending.
- `delete_after_archive`: When `true`, delete each archived camera/time range from Protect after the archive task is no longer pending. Defaults to `false`.
- `delete_after_archive_confirmation`: Must be exactly `DELETE_PROTECT_FOOTAGE_AFTER_ARCHIVE` for daemon deletion.
- `archive_status_poll_seconds`: Poll interval for pending archive status.
- `poll_seconds`: Delay between daemon archive passes.
- `verify_tls`: Keep `true` for valid certificates; set `false` for a local self-signed console.

## Troubleshooting

### `MFA_AUTH_REQUIRED`

The account is valid, but it requires MFA/SSO. Use a local service user without MFA.

### `AUTHENTICATION_FAILED_INVALID_CREDENTIALS`

The `.env` username or password is wrong for local UniFi OS login.

Verify the same credentials can log into:

```text
https://unifi-console.example.invalid
```

### API Key Can List Cameras But Archive Fails

Expected. API keys work for:

```text
/proxy/protect/integration/v1/cameras
```

but the archive endpoints require a logged-in UniFi OS session:

```text
/proxy/protect/api/cloud-provider/video-archive
/proxy/protect/api/video-archive/fetch-pending
```

### `403 Forbidden` When Creating an Archive

Likely causes:

- The service user does not have Video Archiving / NAS linked.
- The configured `archive_host` or `archive_shared_drive` does not match Protect.
- The user lacks the required Protect archive/media permissions.
- The request is missing CSRF. The Rust client captures and sends CSRF automatically after login.

### Config Parse Error Around `camera_ids`

Quote camera IDs:

```toml
camera_ids = ["camera-id-or-camera-name"]
```

## API Model

The tool follows the same API shape used by the Protect web UI:

```text
POST /api/auth/login
GET  /proxy/protect/api/bootstrap
POST /proxy/protect/api/cloud-provider/video-archive
GET  /proxy/protect/api/video-archive/fetch-pending
DELETE /proxy/protect/api/video?camera=...&start=...&end=...
```

Archive requests are deliberately serialized. The tool creates one Protect archive task at a time and, by default, waits until that task is no longer pending before submitting the next task. This avoids overlapping archive work, which can destabilize some consoles.

Deletion, when enabled, is also serialized and runs immediately after the matching archive task is no longer pending. It uses the same camera ID, start timestamp, and end timestamp as the archive request.

The Protect Video Archiving API is not formally documented by Ubiquiti and may change between Protect releases. This implementation was derived from the Protect web UI bundle on a UniFi OS 5.2.23 / Protect 6.3.1-era console.

## License

MIT. See [LICENSE.md](LICENSE.md).
