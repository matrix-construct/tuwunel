# Multimedia and storage provision

Tuwunel handles media uploads, remote media fetching, thumbnail generation,
URL previews, and blurhash generation. This chapter covers configuration for
all of these features, as well as the storage backends that back them.

- [Storage providers](media/storage.md) — Local filesystem and S3-compatible
  object storage backends.

- [Media management](media/management.md) — Commands for inspecting, deleting,
  and bulk-removing media, including spam response.

## Upload limits

| Option | Default | Description |
|---|---|---|
| `max_request_size` | `24 MiB` | Maximum size of a single media upload. Accepts SI/IEC units, e.g. `"50 MiB"`. |
| `max_pending_media_uploads` | `5` | Maximum number of in-progress asynchronous uploads a single user can have at once. |
| `media_create_unused_expiration_time` | `86400` | Seconds before an unused pending MXC URI is expired and removed (default: 24 hours). |
| `media_rc_create_per_second` | `10` | Maximum media-create requests per second from a single user before rate limiting applies. |
| `media_rc_create_burst_count` | `50` | Maximum burst size for media-create rate limiting per user. |

## Legacy media endpoints

Matrix spec version 1.11 introduced authenticated media endpoints. The older
unauthenticated endpoints are deprecated but some clients and servers still
use them.

| Option | Default | Description |
|---|---|---|
| `allow_legacy_media` | `false` | Serve the unauthenticated `/_matrix/media/*/` endpoints locally. The authenticated equivalents are always enabled. |
| `request_legacy_media` | `false` | Fall back to unauthenticated requests when fetching media from remote servers. Unauthenticated remote media was removed around 2024Q3; enabling this adds federation traffic that is unlikely to succeed. |

## Thumbnails

Thumbnails are generated on demand from the original and cached. A request for
a picture larger than the original is answered with the original itself rather
than an upscale.

| Option | Default | Description |
|---|---|---|
| `media_thumbnail_max_pixels` | `50000000` | Largest picture the thumbnailer will decode, in pixels. Anything larger is served without a thumbnail. Applies to uploaded pictures and to frames extracted from video. |

A generated thumbnail is a PNG, and is served as `image/png` under the filename
`thumbnail.png` rather than the content type or name of the file it came from.
Thumbnails already cached before this was true keep the labelling they were
stored with; only newly generated ones are relabelled.

## Video thumbnails

Image thumbnails need no further configuration. Video thumbnails do: tuwunel
decodes no video itself, so an external program supplies the still frame it
thumbnails. Clients that upload a video without a thumbnail of their own
otherwise leave nothing to preview.

Point `media_video_thumbnail_command` at a program that reads a video and
writes one frame to standard output. With ffmpeg installed:

```toml
media_video_thumbnail_command = [
  "ffmpeg", "-loglevel", "error",
  "-i", "{input}",
  "-vf", "thumbnail",
  "-frames:v", "1",
  "-f", "image2pipe", "-c:v", "mjpeg", "pipe:1",
]
```

The list is an argument vector, not a shell line: the first entry is the
program and the rest are its arguments, passed through without a shell. Every
argument has these tokens substituted before each call.

| Token | Value |
|---|---|
| `{input}` | Path of a temporary file holding the source video. |
| `{width}` | Requested thumbnail width. |
| `{height}` | Requested thumbnail height. |

The frame may be PNG, JPEG, WebP or GIF; tuwunel scales and crops it exactly as
it would an uploaded picture, and caches the result, so the program runs once
per video and size rather than once per request. The ffmpeg `thumbnail` filter
above picks a representative frame rather than the first, which is often black.
The frame comes back at the video's full resolution; feed `{width}` and
`{height}` to a `scale` filter if your users post video large enough for that
to matter.

A video whose frame cannot be produced is served whole, as it was before any of
this, so a failure costs a preview and nothing else. Failures are logged with
the program's own standard error, which is where a misconfigured command
reports itself.

| Option | Default | Description |
|---|---|---|
| `media_video_thumbnail_command` | `[]` | Argument vector of the frame-extraction program. Empty leaves videos without thumbnails. |
| `media_video_thumbnail_timeout` | `30` | Seconds allowed per request, counted from the request rather than the spawn so a queue cannot compound the wait. On expiry the program and anything it spawned are killed. |
| `media_video_thumbnail_concurrency` | `1` | Programs permitted to run at once. Requests past this wait for a slot. Raise it where cores are spare; a restart is required to apply a change. |
| `media_video_thumbnail_max_size` | `128 MiB` | Largest video staged for the program. A larger one is served without a thumbnail. |
| `media_video_thumbnail_path` | `<database_path>/tmp` | Directory a video is staged in, one file per running program. |

### Bounding what a video can cost

Everything the program sees comes from an upload, so three limits stand between
a crafted file and the host.

`media_video_thumbnail_max_size` decides which videos are staged at all, and
bounds the frame read back: a program offering more is refused rather than
truncated into a decode failure. The staged copy is written mode `0600` into
`media_video_thumbnail_path` and removed as soon as the program exits, on the
deadline and on a cancelled request alike. That path defaults to a `tmp`
subdirectory of the database rather than the system temporary directory: `/tmp`
is frequently a tmpfs, where staging a large video spends memory rather than
disk.

`media_thumbnail_max_pixels` bounds the decode. A video's frame arrives at the
video's own resolution, so a file declaring 100000x100000 would otherwise ask
the thumbnailer for the memory to hold it. The dimensions are read from the
header and checked before any decoder allocates. The default of 50 megapixels
is about four 8K frames, budgeted at four bytes per pixel, and the budget is
per in-flight request rather than per server, since thumbnail requests are not
otherwise limited in number.

The same limit applies to uploaded pictures, where the equivalent file is a
decompression bomb, and there it is a tightening: before this existed the
effective ceiling was the image decoder's own 512 MiB allocation default,
around 128 megapixels. A picture between that and the new limit will now be
served whole instead of thumbnailed. Raise the value if you serve originals
that large.

`media_video_thumbnail_timeout` and `media_video_thumbnail_concurrency` bound
time and parallelism. On expiry the whole process group is killed, not merely
the program tuwunel started, so a wrapper script cannot leave its own decoder
running. The same happens when the request is cancelled or the server shuts
down.

A video the program fails on, or whose frame the thumbnailer then refuses, is
left alone for five minutes rather than retried on the next request. Without
that, one upload the decoder chokes on would spend a slot on every thumbnail
request it received, at any size, and at the default concurrency of one that is
the slot every other video is waiting for. Only the program's own verdict
counts: a request that gave up waiting for a slot, or that reached the front of
the queue with too little of its deadline left to give the program a fair run,
says nothing about the video and is not remembered. The cost is that fixing a
misconfigured command does not take effect for already-failed videos until the
interval passes.

The decoder's memory is not tuwunel's to cap: it is a separate process. Under
systemd it is spawned into the service's cgroup, so a `MemoryMax=` on the unit
covers it. Note what else that covers, though: the cgroup holds tuwunel too,
and tuwunel's own footprint is dominated by the RocksDB block cache, so a limit
chosen for the decoder alone will be far below what the server needs and will
get the server killed instead. Size `MemoryMax=` for the whole service, or
leave it unset and rely on the limits above.

### Shutdown, reload and restart

A running extraction never delays a shutdown past its own deadline, and never
outlives the server that started it.

The program runs inside the request that asked for the thumbnail. On shutdown
that request is given the `client_shutdown_timeout` grace period and is then
dropped, which kills the program's process group, releases its slot and unlinks
the staged video. A client that disconnects mid-request has the same effect at
once. Nothing waits on the program itself, so the stop time is bounded by the
grace period rather than by `media_video_thumbnail_timeout`.

A configuration reload does not disturb work in flight. An extraction already
under way keeps the deadline it computed at entry and finishes under it; the
next request reads the new values. `media_video_thumbnail_concurrency` is the
exception, as it sizes a semaphore built at startup.

Two cases skip the orderly path. An in-place restart replaces the running image
without unwinding, and a `SIGKILL` gives nothing a chance to run, so either can
leave a staged video behind. Tuwunel reclaims those at startup by sweeping its
staging directory, which is why that directory should hold nothing else. Under
systemd the decoder is in the unit's cgroup, so stopping the service kills it
along with everything else in the unit.

### What the program is exposed to

The video handed to the program is whatever someone uploaded, on your server or
on any server you federate with, since a remote original cached locally is
thumbnailed the same way. That is the point of the feature and it cannot be
otherwise, so it is worth being precise about what does and does not stand
behind it.

Nothing uploaded is ever executed. The staged file is written without an
execute bit, given a random name and no extension, and passed to the program as
a path argument; the program itself comes from your configuration and nowhere
else. Renaming an executable to `.png`, or to anything, changes none of that.
There is no shell, so no argument in the video's name or type can be
interpreted as one.

What remains is the decoder's own parsing of hostile input, which is the same
exposure ffmpeg carries anywhere it is pointed at untrusted media. Note that
the `video/` check is on the content type the *uploader* declared, so it
selects which media are worth trying, not which are safe; assume anything can
reach the program. Tuwunel keeps it at arm's length: a separate process, so a
crash or a corrupted heap is not in the server's address space; a deadline; a
process group killed as a unit; one slot at a time by default; and a size limit
before anything is staged. Under the packaged units the program also inherits
the service sandbox, which is the substantial part of this: no capabilities,
`NoNewPrivileges=yes`, the unit's syscall allow-list, `ProtectSystem=strict`,
`PrivateDevices=yes` and `MemoryDenyWriteExecute=yes`, that last one frustrating
the usual step from a memory-safety bug to executing anything.

Choose the decoder accordingly, keep it patched, and prefer a build with only
the demuxers you need if you have one.

### systemd

The packaged units filter syscalls down to `@system-service @resources` and
then subtract several sets, `@ipc` among them. `pipe2` belongs to `@ipc`, and
spawning the program with pipes needs it, so the units add the two pipe calls
back:

```ini
SystemCallFilter=pipe pipe2
```

A unit predating this, including one you have customised yourself, does not
carry that line, and the spawn fails with `EPERM` until it does. Add it as a
drop-in rather than editing the unit:

```ini
# /etc/systemd/system/tuwunel.service.d/video-thumbnails.conf
[Service]
SystemCallFilter=pipe pipe2
```

Nothing else in the shipped hardening obstructs the program: `/usr` stays
readable and executable under `ProtectSystem=strict`, and `PrivateDevices=yes`
still provides the `/dev/null` the program's standard input is bound to. The
staging directory is already writable, through `ReadWritePaths` on the Debian
and RPM units and through `StateDirectory=` on the Arch one; point
`media_video_thumbnail_path` somewhere else and you must grant that path
yourself.

Prefer a decoder you would already trust with remote media. Nothing about the
mechanism is specific to ffmpeg: any program that reads a video and writes one
frame will do.

### Containers

The published images carry no decoder. Installing ffmpeg on our own Debian
base, with `--no-install-recommends`, costs about 190 MB of codec libraries,
and bundling that would charge every deployment for a feature that is off by
default. So the program is yours to supply. Two ways, and the second needs no
image of your own.

Derive an image. The runtime is Debian, so this is the whole of it:

```dockerfile
FROM ghcr.io/matrix-construct/tuwunel:latest
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg \
 && rm -rf /var/lib/apt/lists/*
```

Or mount a decoder in. Because tuwunel runs a program rather than linking a
library, a statically linked ffmpeg build is just a file the container needs to
see:

```
--volume /opt/ffmpeg:/usr/local/bin/ffmpeg:ro
```

with `media_video_thumbnail_command` pointing at `/usr/local/bin/ffmpeg`. It
must be a static build; one taken from the host will look for libraries the
image does not have. This keeps you on the published image and adds one file
instead of a dependency tree.

Nothing else about the container needs changing. The program is an ordinary
child process, so no added capabilities or privileges, and the default seccomp
profile permits the pipe calls that the packaged systemd units have to allow
back explicitly. Staged videos land in `media_video_thumbnail_path`, which
defaults inside the database directory, so they go to your data volume rather
than the container's writable layer and a read-only root filesystem is no
obstacle.

## Blocking remote media

`prevent_media_downloads_from` is a list of regex patterns matched against
server names. Tuwunel refuses to download media originating from any matching
server.

```toml
prevent_media_downloads_from = [
  "badserver\\.tld$",
  "spammy-phrase",
]
```

This is useful as a reactive measure after a spam incident. See the
[Management](media/management.md) page for bulk-deletion commands to pair
with it.

## URL previews

URL previews are disabled unless at least one allowlist is configured.
All allowlist checks are evaluated before the denylist check. The
[URL Previews](media/url-previews.md) page covers user agents, the spider
budget, caching, and troubleshooting an empty preview.

| Option | Default | Description |
|---|---|---|
| `url_preview_domain_explicit_allowlist` | `[]` | Exact domain matches allowed for previewing. `"google.com"` matches `https://google.com` but not `https://subdomain.google.com`. Set to `["*"]` to allow all domains. |
| `url_preview_domain_contains_allowlist` | `[]` | Substring domain matches. `"google.com"` matches any URL whose domain contains that string — including unrelated domains. Set to `["*"]` to allow all domains. |
| `url_preview_url_contains_allowlist` | `[]` | Substring match against the full URL (not just the domain). Set to `["*"]` to allow all URLs. |
| `url_preview_domain_explicit_denylist` | `[]` | Exact domain matches explicitly blocked. The denylist is checked first. Setting to `["*"]` has no effect. |
| `url_preview_check_root_domain` | `false` | When enabled, domain allowlist checks are applied to the root domain. Allows all subdomains of any allowed domain — e.g. allowing `wikipedia.org` also allows `en.m.wikipedia.org`. |
| `url_preview_max_spider_size` | `786432` | Maximum bytes read from a page when generating a preview (default: 768 KiB). A page whose OpenGraph tags sit past this point yields an empty preview. |
| `url_preview_max_media_size` | `52428800` | Maximum size of a single media item fetched or relayed for a URL preview: the og:image measurement fetch and the lazy-media relay. Media larger than this is not registered, and an over-cap relay is refused (default: 50 MiB). |
| `url_preview_cache_ttl` | `86400` | Seconds a generated preview is cached before the page is fetched again, empty previews included (default: 24 hours). Raise it to spare origins in a room whose history is read often, lower it to pick up changed OpenGraph metadata sooner. |
| `url_preview_bound_interface` | — | Network interface name or IP address to bind when making URL preview requests. Example: `"eth0"` or `"1.2.3.4"`. |
| `url_preview_user_agent` | — | User-Agent header sent when fetching pages to extract their OpenGraph tags. Defaults to the versioned server User-Agent, e.g. `"Tuwunel/1.8.1 preview"`. |
| `url_preview_media_user_agent` | — | User-Agent header sent when fetching and relaying preview media files themselves. Falls back to `url_preview_user_agent`. |

> [!NOTE]
> Setting any allowlist to `["*"]` opens significant attack surface — a
> malicious client could cause the server to make requests to arbitrary URLs
> on the local network. Use explicit allowlists wherever possible.

`og:image`, `og:video`, and `og:audio` (and direct links to image, video,
and audio files) resolve to an `mxc://` URI on this server rather than the
third-party URL, and are not copied into media storage: requests for that
`mxc://` URI are relayed — the server fetches the source URL on the client's
behalf (subject to the same SSRF/CIDR checks as everything else on this
page, and capped at `url_preview_max_media_size`) and passes the content
through, so the third party sees the server's address rather than the
client's. Images are the exception to the lazy fetch: one is downloaded
while generating the preview to measure `og:image:width`/`og:image:height`
and `matrix:image:size`, and those bytes are staged so the first client
download does not fetch the origin a second time.
`og:video:width`/`og:video:height` are populated when the page declares
them. Clients cache the results themselves per the immutable cache headers
on media downloads. A preview's `mxc://` URI is only as durable as its
source URL: if the source expires or changes, later fetches reflect that,
unlike uploaded media. Upstream error responses are never relayed as media.

## Blurhash

Tuwunel can generate [blurhashes](https://blurha.sh/) for uploaded images,
which clients use to show a blurred placeholder before the full image loads.
This requires the `blurhashing` compile-time feature.

Blurhash settings live in a dedicated config section:

```toml
[global.blurhashing]
components_x = 4
components_y = 3
blurhash_max_raw_size = 33554432
```

| Option | Default | Description |
|---|---|---|
| `components_x` | `4` | Horizontal detail components. Higher values produce more detailed hashes at the cost of a larger hash string. |
| `components_y` | `3` | Vertical detail components. |
| `blurhash_max_raw_size` | `33554432` | Maximum raw image size (after decoding to pixel data) that will be blurhashed, in bytes (default: ~32 MiB). Set to `0` to disable blurhashing entirely. Should be at or above `max_request_size` to avoid silently skipping large uploads. |
