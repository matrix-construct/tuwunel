# Media management

Tuwunel provides a set of admin room commands for inspecting and deleting
media. All commands are invoked in the admin room and prefixed with
`!admin media`.

For storage-native archival and migration integrations, the authenticated
[Media Object Locations API](#media-object-locations-api) resolves original MXC media
to object key names across selected providers.

## Inspecting media

### get-file-info

```
!admin media get-file-info <mxc_uri>
```

Returns the stored metadata for a media file: content type, file size,
creation time, and which user uploaded it. Useful for investigating a
reported file before deciding whether to delete it.

### get-remote-file

```
!admin media get-remote-file <mxc_uri> [-s <server>] [-t <timeout_ms>]
```

Fetches a remote media file from the originating server and returns its
metadata. The actual content is discarded after fetching so the admin room
is not flooded. Default timeout is 10 000 ms.

### get-remote-thumbnail

```
!admin media get-remote-thumbnail <mxc_uri> \
  [-s <server>] [-t <timeout_ms>] [--width <px>] [--height <px>]
```

Like `get-remote-file` but requests a thumbnail at the given dimensions
(default 800×800). Useful for confirming what a thumbnail looks like without
sending it to a client.

## Deleting media

### Delete a single file

```
!admin media delete --mxc mxc://example.com/AbCdEfGhIjKl
```

Removes one media file from the database and from storage. Use
`get-file-info` first to confirm you have the right MXC URI.

### Delete media referenced by an event

```
!admin media delete-by-event --event-id $abc123:example.com
```

Extracts all MXC URIs from the event (including the primary media URL,
thumbnail URL, and encrypted file URL) and deletes each one. Returns the
number of files deleted. Useful when a user reports a specific message
containing unwanted media.

### Delete a list of MXC URIs

Paste a code block of MXC URIs into the admin room, one per line:

````
!admin media delete-list
```
mxc://example.com/AbCdEfGhIjKl
mxc://example.com/MnOpQrStUvWx
mxc://badserver.tld/YzAbCdEfGhIj
```
````

Errors on individual URIs are ignored. The command returns the total number
deleted and the number that failed to parse.

### Delete by time range

```
!admin media delete-range <duration> --older-than
!admin media delete-range <duration> --newer-than
```

Deletes remote media whose filesystem modification time falls outside the
given duration from now. Exactly one of `--older-than` or `--newer-than`
must be specified.

Duration format: `30s`, `5m`, `2h`, `7d`, etc.

By default only remote media is deleted. To also delete locally-uploaded
media, append the confirmation flag:

```
!admin media delete-range 90d --older-than --yes-i-want-to-delete-local-media
```

Examples:

```
# Delete all remote media older than 30 days
!admin media delete-range 30d --older-than

# Delete remote media uploaded in the last hour (e.g. after a spam burst)
!admin media delete-range 1h --newer-than
```

### Delete all media from a local user

```
!admin media delete-all-from-user <username>
```

Deletes every media file uploaded by the named local user. The username is
the localpart only, without the `@` or server name. Errors on individual
files are ignored.

### Delete all media from a remote server

```
!admin media delete-all-from-server <server_name>
```

Deletes every cached copy of remote media originating from the given server.
This only affects remotely-fetched media by default. To also remove local
uploads that somehow reference the server, add the confirmation flag:

```
!admin media delete-all-from-server <server_name> \
  --yes-i-want-to-delete-local-media
```

## Responding to a spam incident

The full incident playbook (cached-media delete, federation blocks, invite
gating, and the per-room policy-server option) lives in
[Policy and Moderation > Responding to a spam incident](../moderation.md#responding-to-a-spam-incident).
That chapter is the source of truth; refer to it when an incident is in
progress.

## Media Object Locations API

`POST /_tuwunel/admin/v1/media/object-locations` resolves original media to
S3-compatible or Google Cloud Storage object key names. An archive, backup or
migration integration can then use the provider's native copy API.
Tuwunel does not copy objects through this endpoint.

### Request

```http
POST /_tuwunel/admin/v1/media/object-locations
Authorization: Bearer <server-admin-access-token>
Content-Type: application/json

{
  "mxc_uris": ["mxc://example.org/A1b2C3", "mxc://example.org/D4e5F6"],
  "providers": ["media_on_s3"],
  "verify": true
}
```

| Field | Meaning |
| --- | --- |
| `mxc_uris` | Required array of 1 to 1,000 strings.|
| `providers` | Optional array of 1 to 100 exact provider IDs. Duplicates are removed in first-occurrence order. Omission selects `media_storage_providers`; when that setting is empty, all active providers are selected, matching media reads. |
| `verify` | Optional boolean, default `true`. Check presence and size through provider metadata requests, without reading payloads. |

Explicit provider selection can include any configured storage provider. Unknown
or disabled provider IDs produce `provider_unavailable` results for known media.
An explicit empty provider list is rejected. Provider selection never falls back
to another provider implicitly.

With `verify=false`, local MXCs return candidate locations with
`presence: "unverified"` and no `size`; these do not prove an object exists.
Remote MXCs always require a successful presence check before their locations
are exported, including when `verify=false` is requested. This prevents a stale
remote metadata row from being reported as a locally cached object.

### Response

```json
{
  "locations": [
    {
      "mxc_uri": "mxc://example.org/A1b2C3",
      "status": "found",
      "objects": [
        {
          "provider": "media_on_s3",
          "object_key_name": "provider-resolved-object-name",
          "presence": "present",
          "size": 428112
        }
      ]
    },
    {
      "mxc_uri": "mxc://example.org/D4e5F6",
      "status": "not_found",
      "objects": []
    }
  ]
}
```

For media with original-file metadata, `objects` contains one entry for every
selected provider. `object_key_name` is the original's logical object name,
without a provider prefix, and is identical across S3/GCS providers. Consumers
use `provider` to identify the backend, bucket and optional `base_path` in their
own configuration, then apply that prefix to resolve the full bucket key.

| `presence` | Meaning |
| --- | --- |
| `present` | The provider confirmed the object and supplied `size` in bytes. |
| `unverified` | A local-MXC candidate, without a presence check or size. |
| `not_present` | The provider returned an object-not-found result. |
| `provider_unavailable` | The provider is unknown, disabled, denied access, or failed its metadata lookup. |
| `unsupported_backend` | A local filesystem provider cannot supply an object-store transfer address. |

Local filesystem, unknown and disabled providers have no `object_key_name` field.
A known S3/GCS provider can still supply a local-MXC candidate key when HEAD
reports absence or failure. For remote MXCs, the key is omitted unless presence
is confirmed in that provider.

The per-MXC `status` summarizes the results in this precedence:
`found` if any object is present, then `unverified`, `provider_unavailable`,
`not_present`, or `unsupported_backend`. All provider results remain available,
even when another provider succeeds. `found` means at least one transferable
object-store copy exists; a local-filesystem-only deployment reports
`unsupported_backend`.

`not_found` with an empty `objects` array means original-file metadata is absent.
`metadata_unavailable` with an empty array means original metadata could not be
read or decoded. It must not be treated as absence. `invalid_mxc` with an empty
array identifies an invalid URI string. These failures do not fail the remaining
batch. Malformed request fields and out-of-range array lengths produce HTTP 400.
Missing or invalid tokens produce HTTP 401; an
authenticated non-administrator receives HTTP 403.

### Operational properties

- Resolves originals only. Thumbnails and pending uploads are excluded.
- Never federates a request or materializes lazy URL-preview media. Absent
  remote media remains absent.
- Makes metadata-only provider requests. It never downloads media content,
  signs URLs, or exports credentials, raw provider settings, or host paths.
- Processes at most 16 items concurrently per batch, with at most 16 provider
  HEAD operations active across all batches on the server.
- Bounds each verification attempt, including waiting for a concurrency slot,
  to five seconds. All attempts share a batch budget of 30 seconds or half
  `client_request_timeout`, whichever is shorter. Checks that time out or
  cannot start within that budget report `provider_unavailable`.
- Logs the requesting administrator, selected provider IDs, item count and
  verification mode, without logging the MXC batch at normal log levels.
- Sends `Cache-Control: no-store`. Locations describe a point in time; objects
  can subsequently be deleted or moved. Lookup does not establish a legal hold.

