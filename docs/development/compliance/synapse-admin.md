# Synapse Admin API

Tuwunel serves the Synapse administration API under the `/_synapse/admin` path so that existing Matrix server administration tools work against it, including administration dashboards such as synapse-admin and ketesa and moderation bots such as Draupnir and Meowlnir.

This page lists every `/_synapse/admin` endpoint Tuwunel is aware of and whether it is served. Endpoints that are not served return `404 M_UNRECOGNIZED`. All endpoints require an administrator access token unless the description says otherwise.

## Status

- ✅ **supported**: implemented and served today.
- 🟨 **planned**: on the current implementation roadmap; not yet served.
- 🟥 **not-implemented**: a supportable endpoint that Tuwunel does not serve yet, deferred pending new storage or a subsystem and not on the current roadmap.
- ⬛ **not-applicable**: does not apply to Tuwunel's storage engine or data model, or is intentionally omitted.

## Counts

- Endpoints (method and path): 100
  - ✅ supported: 69
  - 🟨 planned: 1
  - 🟥 not-implemented: 22
  - ⬛ not-applicable: 8

### By domain

| Domain | ✅ | 🟨 | 🟥 | ⬛ | total |
|---|---:|---:|---:|---:|---:|
| Users | 20 | 1 | 8 | 1 | 30 |
| Devices and registration tokens | 12 | 0 | 1 | 6 | 19 |
| Rooms | 20 | 0 | 0 | 0 | 20 |
| Media and statistics | 8 | 0 | 10 | 1 | 19 |
| Federation and miscellaneous | 9 | 0 | 3 | 0 | 12 |
| **total** | **69** | **1** | **22** | **8** | **100** |

## Users

| Method | URL | Status | Description |
|---|---|---|---|
| GET | `/_synapse/admin/v1/register` | ✅ | Fetch a one-time nonce for shared-secret registration. Unauthenticated; not served when MAS is active. |
| POST | `/_synapse/admin/v1/register` | ✅ | Register a user with shared-secret HMAC authentication. Not served when MAS is active. |
| GET | `/_synapse/admin/v2/users` | ✅ | List and search local users with filtering and pagination. |
| GET | `/_synapse/admin/v3/users` | ✅ | List users with a tri-state deactivated filter (version 3 of the user list). |
| GET | `/_synapse/admin/v2/users/{user_id}` | ✅ | Return a single user's account details. |
| PUT | `/_synapse/admin/v2/users/{user_id}` | ✅ | Create or modify a user account, including admin status, deactivation, profile, and third-party ids. |
| POST | `/_synapse/admin/v1/deactivate/{user_id}` | ✅ | Deactivate a user, optionally erasing their data. |
| POST | `/_synapse/admin/v1/reset_password/{user_id}` | ✅ | Reset a user's password. Not served when MAS is active. |
| GET | `/_synapse/admin/v1/users/{user_id}/admin` | ✅ | Report whether a user is a server administrator. |
| PUT | `/_synapse/admin/v1/users/{user_id}/admin` | 🟨 | Grant or revoke a user's server-administrator status. |
| GET | `/_synapse/admin/v1/users/{user_id}/joined_rooms` | ✅ | List the rooms a user is joined to. |
| GET | `/_synapse/admin/v1/users/{user_id}/memberships` | ✅ | Return the user's room memberships as a room-to-state map. |
| GET | `/_synapse/admin/v1/users/{user_id}/pushers` | ✅ | List a user's configured pushers. |
| GET | `/_synapse/admin/v1/users/{user_id}/accountdata` | ✅ | Return a user's global and per-room account data. |
| GET | `/_synapse/admin/v1/whois/{user_id}` | ✅ | Return connection and device information for a user. Also served on the client-server admin path. |
| PUT | `/_synapse/admin/v1/suspend/{user_id}` | ✅ | Suspend or release a user account (MSC4323). |
| POST | `/_synapse/admin/v1/users/{user_id}/login` | ✅ | Mint an access token to act as a user (impersonation). |
| POST, DELETE | `/_synapse/admin/v1/users/{user_id}/shadow_ban` | 🟥 | Not implemented; requires new per-user storage (deferred). |
| GET, POST, DELETE | `/_synapse/admin/v1/users/{user_id}/override_ratelimit` | 🟥 | Not implemented; Tuwunel has no per-user request limiter to override (deferred). |
| POST | `/_synapse/admin/v1/user/{user_id}/redact` | ✅ | Redact a user's events across their rooms as a background task. |
| GET | `/_synapse/admin/v1/user/redact_status/{redact_id}` | ✅ | Poll the status of a user-redaction task. |
| GET | `/_synapse/admin/v1/users/{user_id}/sent_invite_count` | 🟥 | Not implemented; no timestamp-indexed invite log to count from (deferred). |
| GET | `/_synapse/admin/v1/users/{user_id}/cumulative_joined_room_count` | 🟥 | Not implemented; no timestamp-indexed membership log (deferred). |
| POST | `/_synapse/admin/v1/users/{user_id}/_allow_cross_signing_replacement_without_uia` | ✅ | Allow cross-signing key replacement without interactive auth for a bounded window. |
| GET | `/_synapse/admin/v1/threepid/{medium}/users/{address}` | ✅ | Look up the local user bound to a third-party identifier (email). |
| GET | `/_synapse/admin/v1/auth_providers/{provider}/users/{external_id}` | 🟥 | Not implemented; requires an external-identifier index (deferred). |
| GET | `/_synapse/admin/v1/search_users/{term}` | ⬛ | Intentionally omitted; undocumented and exposes password hashes. |

## Devices and registration tokens

| Method | URL | Status | Description |
|---|---|---|---|
| GET | `/_synapse/admin/v2/users/{user_id}/devices` | ✅ | List a user's devices. |
| POST | `/_synapse/admin/v2/users/{user_id}/devices` | ✅ | Create a device for a user. |
| GET, PUT, DELETE | `/_synapse/admin/v2/users/{user_id}/devices/{device_id}` | ✅ | Fetch, rename, or delete a specific device. |
| POST | `/_synapse/admin/v2/users/{user_id}/delete_devices` | ✅ | Bulk-delete a set of a user's devices. |
| GET | `/_synapse/admin/v1/registration_tokens` | ✅ | List registration tokens. Not served when MAS is active. |
| POST | `/_synapse/admin/v1/registration_tokens/new` | ✅ | Create a registration token. Not served when MAS is active. |
| GET | `/_synapse/admin/v1/registration_tokens/{token}` | ✅ | Return a registration token. Not served when MAS is active. |
| PUT | `/_synapse/admin/v1/registration_tokens/{token}` | ✅ | Update a registration token's usage cap or expiry. Not served when MAS is active. |
| DELETE | `/_synapse/admin/v1/registration_tokens/{token}` | ✅ | Delete a registration token. Not served when MAS is active. |
| GET | `/_synapse/admin/v1/username_available` | ✅ | Check whether a username is available for registration. |
| POST | `/_synapse/admin/v1/account_validity/validity` | 🟥 | Not implemented; requires an account-expiry subsystem (deferred). |
| GET, PUT | `/_synapse/admin/v1/experimental_features/{user_id}` | ⬛ | Not applicable; Tuwunel feature flags are server-wide, not per user. |
| GET, POST | `/_synapse/admin/v1/background_updates/enabled` | ⬛ | Not applicable; RocksDB has no background-update mechanism. |
| GET | `/_synapse/admin/v1/background_updates/status` | ⬛ | Not applicable; RocksDB has no background-update mechanism. |
| POST | `/_synapse/admin/v1/background_updates/start_job` | ⬛ | Not applicable; RocksDB has no background-update mechanism. |

## Rooms

| Method | URL | Status | Description |
|---|---|---|---|
| GET | `/_synapse/admin/v1/rooms` | ✅ | List and search rooms. |
| GET | `/_synapse/admin/v1/rooms/{room_id}` | ✅ | Return a single room's details. |
| DELETE | `/_synapse/admin/v1/rooms/{room_id}` | ✅ | Synchronously delete a room and evict its local members. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/members` | ✅ | List a room's joined members. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/state` | ✅ | Return a room's full state with an administrator visibility bypass. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/messages` | ✅ | Page a room's timeline with an administrator visibility bypass. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/context/{event_id}` | ✅ | Return the events surrounding an event with an administrator visibility bypass. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/timestamp_to_event` | ✅ | Find the event nearest a timestamp. |
| GET | `/_synapse/admin/v1/rooms/{room_id}/hierarchy` | ✅ | Return a room's space hierarchy (local rooms only). |
| GET, PUT | `/_synapse/admin/v1/rooms/{room_id}/block` | ✅ | Read or set a room's blocked status. |
| POST | `/_synapse/admin/v1/rooms/{room_id}/make_room_admin` | ✅ | Grant room-administrator power to a user. |
| GET, DELETE | `/_synapse/admin/v1/rooms/{room_id}/forward_extremities` | ✅ | Inspect or reduce a room's forward extremities. |
| DELETE | `/_synapse/admin/v2/rooms/{room_id}` | ✅ | Delete a room asynchronously as a background task. |
| GET | `/_synapse/admin/v2/rooms/delete_status/{delete_id}` | ✅ | Poll a room-deletion task by delete id. |
| GET | `/_synapse/admin/v2/rooms/{room_id}/delete_status` | ✅ | List room-deletion tasks for a room. |
| POST | `/_synapse/admin/v1/join/{room_id_or_alias}` | ✅ | Join a local user to a room as an administrator. |
| POST | `/_synapse/admin/v1/purge_history/{room_id}` | ✅ | Purge a room's history up to a point in time or an event. An optional trailing event id selects the boundary. |
| GET | `/_synapse/admin/v1/purge_history_status/{purge_id}` | ✅ | Poll a history-purge task. |

## Media and statistics

| Method | URL | Status | Description |
|---|---|---|---|
| GET | `/_synapse/admin/v1/media/{server_name}/{media_id}` | ✅ | Return metadata for a media item, including quarantine fields. |
| DELETE | `/_synapse/admin/v1/media/{server_name}/{media_id}` | ✅ | Delete a local media item. |
| POST | `/_synapse/admin/v1/media/delete` | ✅ | Delete media by age and size. A legacy per-server alias path is also served. |
| POST | `/_synapse/admin/v1/purge_media_cache` | ✅ | Purge cached remote media older than a timestamp. |
| GET | `/_synapse/admin/v1/users/{user_id}/media` | ✅ | List media uploaded by a user. |
| DELETE | `/_synapse/admin/v1/users/{user_id}/media` | ✅ | Delete media uploaded by a user, one page per call. |
| GET | `/_synapse/admin/v1/room/{room_id}/media` | ✅ | List media referenced by a room's unencrypted events. |
| POST | `/_synapse/admin/v1/media/quarantine/{server_name}/{media_id}` | 🟥 | Not implemented; requires a media quarantine subsystem. The paired unquarantine path is also unserved (deferred). |
| POST | `/_synapse/admin/v1/user/{user_id}/media/quarantine` | 🟥 | Not implemented; requires a media quarantine subsystem (deferred). |
| POST | `/_synapse/admin/v1/room/{room_id}/media/quarantine` | 🟥 | Not implemented; requires a media quarantine subsystem (deferred). |
| GET | `/_synapse/admin/v1/media/quarantine_changes` | 🟥 | Not implemented; requires a media quarantine subsystem (deferred). |
| POST | `/_synapse/admin/v1/media/protect/{media_id}` | 🟥 | Not implemented; requires quarantine-protection storage. The paired unprotect path is also unserved (deferred). |
| GET | `/_synapse/admin/v1/statistics/users/media` | ✅ | Aggregate per-user media counts and total sizes. |
| GET | `/_synapse/admin/v1/statistics/database/rooms` | ⬛ | Not applicable; a PostgreSQL-only size estimate even in Synapse. |
| GET | `/_synapse/admin/v1/event_reports` | 🟥 | Not implemented; requires an event-reports store (deferred). |
| GET, DELETE | `/_synapse/admin/v1/event_reports/{report_id}` | 🟥 | Not implemented; requires an event-reports store (deferred). |

## Federation and miscellaneous

| Method | URL | Status | Description |
|---|---|---|---|
| GET | `/_synapse/admin/v1/server_version` | ✅ | Return the server name and version. Unauthenticated. |
| GET | `/_synapse/admin/v1/federation/destinations` | ✅ | List federation destinations and their retry state. |
| GET | `/_synapse/admin/v1/federation/destinations/{destination}` | ✅ | Return one destination's retry state. |
| GET | `/_synapse/admin/v1/federation/destinations/{destination}/rooms` | ✅ | List the rooms shared with a destination. |
| POST | `/_synapse/admin/v1/federation/destinations/{destination}/reset_connection` | ✅ | Clear a destination's backoff so delivery retries immediately. |
| POST, PUT | `/_synapse/admin/v1/send_server_notice` | ✅ | Send a server notice to a user. The idempotent form with a transaction id is also served. |
| GET | `/_synapse/admin/v1/scheduled_tasks` | ✅ | List background administrative tasks. |
| GET | `/_synapse/admin/v1/fetch_event/{event_id}` | ✅ | Return a raw event by id, without redaction. |
| GET | `/_synapse/admin/v1/user_reports` | 🟥 | Not implemented; requires a reports store (deferred). |
| GET, DELETE | `/_synapse/admin/v1/user_reports/{report_id}` | 🟥 | Not implemented; requires a reports store (deferred). |
