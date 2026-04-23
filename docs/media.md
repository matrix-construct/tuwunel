# Media

Tuwunel handles media uploads, remote media fetching, thumbnail generation,
URL previews, and blurhash generation. This chapter covers configuration for
all of these features, as well as the storage backends that back them.

- [Storage Providers](media/storage.md) — local filesystem and S3-compatible
  object storage backends
- [Management](media/management.md) — admin commands for inspecting, deleting,
  and bulk-removing media, including spam response workflows

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
All allowlist checks are evaluated before the denylist check.

| Option | Default | Description |
|---|---|---|
| `url_preview_domain_explicit_allowlist` | `[]` | Exact domain matches allowed for previewing. `"google.com"` matches `https://google.com` but not `https://subdomain.google.com`. Set to `["*"]` to allow all domains. |
| `url_preview_domain_contains_allowlist` | `[]` | Substring domain matches. `"google.com"` matches any URL whose domain contains that string — including unrelated domains. Set to `["*"]` to allow all domains. |
| `url_preview_url_contains_allowlist` | `[]` | Substring match against the full URL (not just the domain). Set to `["*"]` to allow all URLs. |
| `url_preview_domain_explicit_denylist` | `[]` | Exact domain matches explicitly blocked. The denylist is checked first. Setting to `["*"]` has no effect. |
| `url_preview_check_root_domain` | `false` | When enabled, domain allowlist checks are applied to the root domain. Allows all subdomains of any allowed domain — e.g. allowing `wikipedia.org` also allows `en.m.wikipedia.org`. |
| `url_preview_max_spider_size` | `256000` | Maximum bytes fetched from a URL when generating a preview (default: 256 KB). |
| `url_preview_bound_interface` | — | Network interface name or IP address to bind when making URL preview requests. Example: `"eth0"` or `"1.2.3.4"`. |

> [!NOTE]
> Setting any allowlist to `["*"]` opens significant attack surface — a
> malicious client could cause the server to make requests to arbitrary URLs
> on the local network. Use explicit allowlists wherever possible.

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
