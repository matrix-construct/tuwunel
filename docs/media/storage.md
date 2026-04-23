# Storage providers

Tuwunel stores media through a configurable provider layer that abstracts over
local filesystem and S3-compatible object storage. Multiple providers can be
active simultaneously, which enables zero-downtime migrations.

## Default storage

Without any explicit configuration, Tuwunel stores media in a subdirectory
called `media/` inside your `database_path`. This is represented internally
as the implicit provider named `"media"`.

```toml
# These are the effective defaults — no configuration required
media_storage_providers = ["media"]
store_media_on_providers = []
```

When `store_media_on_providers` is empty, all providers in
`media_storage_providers` receive new uploads. With only the implicit
`"media"` provider active, this is simply the local filesystem.

## Configuring providers

Providers are declared as TOML sections named
`[global.storage_provider.<NAME>.<brand>]`, where `<NAME>` is the identifier
you reference in `media_storage_providers`, and `<brand>` is either `local`
or `s3`.

### Local filesystem

```toml
[global.storage_provider.media.local]
base_path = "/var/lib/tuwunel/media"
create_if_missing = false
delete_empty_directories = true
startup_check = true
```

| Field | Default | Description |
|---|---|---|
| `base_path` | required | Absolute path to the storage directory. |
| `create_if_missing` | `false` | Create the directory if it does not exist. Disabled by default to surface misconfiguration rather than silently creating a wrong path. |
| `delete_empty_directories` | `true` | Remove directories that become empty after a file is deleted. |
| `startup_check` | `true` | Verify the directory is accessible at startup. Failure aborts startup. |

### S3 and S3-compatible storage

```toml
[global.storage_provider.media_on_s3.s3]
bucket = "my-matrix-media"
region = "us-east-1"
key    = "AKIAIOSFODNN7EXAMPLE"
secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

Alternatively, supply a full S3 URL that encodes bucket, region, and path:

```toml
[global.storage_provider.media_on_s3.s3]
url    = "s3://my-matrix-media/prefix"
key    = "AKIAIOSFODNN7EXAMPLE"
secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

#### S3 configuration reference

| Field | Default | Description |
|---|---|---|
| `url` | — | S3 URL of the form `s3://bucket/path`. Components not present in the URL can be set individually below. |
| `bucket` | — | Bucket name. |
| `region` | `us-east-1` | AWS region where the bucket resides. |
| `key` | — | IAM Access Key ID. |
| `secret` | — | IAM Secret Access Key. Not logged or serialized. |
| `token` | — | Session token for temporary credentials. Not logged or serialized. |
| `base_path` | — | Path prefix inside the bucket. All objects are stored under this prefix. |
| `endpoint` | — | Override the S3 endpoint URL. Required for self-hosted S3-compatible services such as MinIO or DigitalOcean Spaces. |
| `multipart_threshold` | `100 MiB` | Files at or above this size use the S3 multipart upload API. Accepts SI/IEC unit strings. |
| `kms` | — | SSE-KMS key ARN for server-side encryption. |
| `use_bucket_key` | — | Enable S3 Bucket Keys for KMS encryption. Should match the bucket setting. |
| `use_vhost_request` | — | Override virtual-hosted-style request path. Derived automatically from the URL by default. |
| `use_https` | `true` | Require HTTPS. Set `false` only for local development with HTTP-only test endpoints. |
| `startup_check` | `true` | Ping the bucket at startup to confirm connectivity. Failure aborts startup. Set `false` if the provider may be unavailable during startup. |

#### Self-hosted S3-compatible services

For MinIO, DigitalOcean Spaces, Cloudflare R2, and similar services, set the
`endpoint` field and disable virtual-hosted-style requests if required:

```toml
[global.storage_provider.media_on_s3.s3]
endpoint         = "https://minio.example.com:9000"
bucket           = "matrix-media"
region           = "us-east-1"
key              = "minioadmin"
secret           = "minioadmin"
use_vhost_request = false
```

## Migrating to a new storage provider

To migrate from local storage to S3 (or between any two providers) without
downtime:

**Step 1** — Add the new provider and list both in `media_storage_providers`,
but direct new writes only to the new one via `store_media_on_providers`:

```toml
media_storage_providers  = ["media", "media_on_s3"]
store_media_on_providers = ["media_on_s3"]

[global.storage_provider.media_on_s3.s3]
bucket = "my-matrix-media"
region = "us-east-1"
key    = "AKIAIOSFODNN7EXAMPLE"
secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

Tuwunel now writes new media to S3 and reads from whichever provider holds
the file, falling back to local if not found on S3.

**Step 2** — Copy existing files to the new provider using the storage sync
admin command:

```
!admin query storage sync media media_on_s3
```

This copies all objects present in `media` but absent from `media_on_s3`.

**Step 3** — Once the sync is complete and verified, remove `"media"` from
both lists and restart:

```toml
media_storage_providers  = ["media_on_s3"]
store_media_on_providers = []
```

## Storage admin commands

These commands are available via `!admin query storage` and operate directly
on provider objects. They are useful for diagnostics, manual migrations, and
verifying provider state.

| Command | Description |
|---|---|
| `!admin query storage configs` | List all configured storage provider configurations. |
| `!admin query storage providers` | List all active storage provider instances. |
| `!admin query storage debug [<provider>]` | Print debug information for a provider. |
| `!admin query storage show [-p <provider>] <path>` | Show object metadata for a given path. |
| `!admin query storage list [-p <provider>] [<prefix>]` | List objects under an optional prefix. |
| `!admin query storage copy [-p <provider>] [-f] <src> <dst>` | Copy an object. `-f` overwrites an existing destination. |
| `!admin query storage move [-p <provider>] [-f] <src> <dst>` | Move (rename) an object. |
| `!admin query storage delete [-p <provider>] [-v] <path>…` | Delete one or more objects. `-v` prints each deletion. |
| `!admin query storage duplicates <src_provider> <dst_provider>` | List objects that exist in both providers. |
| `!admin query storage differences <src_provider> <dst_provider>` | List objects present in one provider but not the other. |
| `!admin query storage sync <src_provider> <dst_provider>` | Copy all objects from `src` that are missing in `dst`. |


## Startup checks

These options control what Tuwunel verifies about stored media at startup.

| Option | Default | Description |
|---|---|---|
| `media_startup_check` | `true` | Scan the media directory at startup. Removes database entries for files that no longer exist on disk (when `prune_missing_media` is enabled), and upgrades Conduit-era symlinks (when `media_compat_file_link` is enabled). Disable if startup is slow due to a large media directory and neither check applies to you. |
| `prune_missing_media` | `false` | During the startup scan, delete database metadata for any media file that is missing from disk. **Caution:** if the storage directory is temporarily inaccessible or miss-mounted, this will permanently destroy metadata for all affected files. |
| `media_compat_file_link` | `false` | Create Conduit-compatible symlinks alongside Tuwunel's media files. Only needed if you intend to switch back to Conduit. Requires `media_startup_check = true` to take effect. |
