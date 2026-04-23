# Media management

Tuwunel provides a set of admin room commands for inspecting and deleting
media. All commands are invoked in the admin room and prefixed with
`!admin media`.

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

Extracts all MXC URIs from the event — including the primary media URL,
thumbnail URL, and encrypted file URL — and deletes each one. Returns the
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

When a server sends spam media to your users, the typical response is:

**1. Identify the source server.**
Check the MXC URIs in the reported messages — the server name is the
authority component: `mxc://<server_name>/<media_id>`.

**2. Delete cached copies of their media.**

```
!admin media delete-all-from-server badserver.tld
```

**3. Block future media downloads from that server.**
Add the server to `prevent_media_downloads_from` in your config and reload
or restart Tuwunel:

```toml
prevent_media_downloads_from = ["badserver\\.tld$"]
```

**4. If the spam arrived within a known time window**, use `delete-range` to
catch anything missed:

```
!admin media delete-range 2h --newer-than
```

**5. If you have a list of specific MXC URIs** (e.g. from a moderation tool
or a shared blocklist), use `delete-list` to remove them in bulk.

**6. Consider server-level federation blocks** via
`forbidden_remote_server_names` if the server is persistently abusive,
which will block all federation traffic rather than just media.
