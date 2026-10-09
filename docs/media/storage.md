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
or `s3`. For container deployments (Docker Compose, Podman, Kubernetes) where
mounting a configuration file is inconvenient, please refer to the section on
[environment variables](#environment-variables) instead.


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

### Google Cloud Storage

```toml
[global]
media_storage_providers = ["media_on_gcs"]

[global.storage_provider.media_on_gcs.gcs]
bucket = "my-matrix-media"
```

This selects GCS for media storage. Credentials resolve from the environment and
fall back to the instance metadata server, so on Google Kubernetes Engine or
Compute Engine the server can authenticate as the attached service account with
no exported key material. Grant that service account
`roles/storage.objectAdmin` on the bucket, or equivalent object permissions.

Use this rather than pointing the `s3` provider at
`https://storage.googleapis.com`. The S3-interoperability endpoint accepts only
HMAC keys, which are service-account keys, so it cannot use an attached service
account. The `constraints/iam.disableServiceAccountKeyCreation` policy prevents
creating the new service-account keys that this setup would require.

An object prefix is set separately, with `base_path`:

```toml
[global.storage_provider.media_on_gcs.gcs]
bucket    = "my-matrix-media"
base_path = "prefix"
```

A `gs://bucket/path` URL is not accepted here, because the path component would
not be applied as a prefix.

#### GCS configuration reference

| Option | Default | Description |
|---|---|---|
| `bucket` | None | Bucket name. Required; an unset bucket disables the provider. |
| `base_path` | None | Object prefix within the bucket. Alias: `path`. |
| `service_account_path` | None | Path to a service account key file. Leave unset to use the environment, which on GCE/GKE ends at the metadata server, the keyless and recommended configuration. |
| `application_credentials_path` | None | Path to an application default credentials file. Only service-account and authorized-user documents are recognised. |
| `multipart_threshold` | 100 MiB | Upload size above which multipart upload is used. |
| `multipart_part_size` | 10 MiB | Size of each part within a multipart upload. |
| `startup_check` | true | Ping the provider at startup; failures abort startup. The probe is a listing operation, so it requires `storage.objects.list`, passes on an empty bucket, and proves neither write nor delete. |

Credential file settings are applied to the environment-derived builder. Do not
combine `service_account_path` with `GOOGLE_SERVICE_ACCOUNT_KEY`; the underlying
client rejects simultaneous service-account path and inline-key credentials.
Environment service-account credentials take priority over
`application_credentials_path`.

Media redirects (`media_allow_redirect`) work without a key as well: with no
private key configured, signing goes through the IAM `signBlob` API, where
Google holds the key. That path additionally needs the IAM Service Account
Credentials API enabled and `iam.serviceAccounts.signBlob` on the service
account.

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

### Environment variables

The variable name is built from four parts joined by `__` (double underscore):

```
TUWUNEL_STORAGE_PROVIDER__<NAME>__<brand>__<FIELD>
```

- **`TUWUNEL_STORAGE_PROVIDER`** — fixed prefix that maps to
  `[global.storage_provider]`.
- **`<NAME>`** — the provider name you reference in `media_storage_providers`
  (e.g., `MEDIA`, `MEDIA_ON_S3`).
- **`<brand>`** — the provider type: `LOCAL` or `S3`.
- **`<FIELD>`** — the field name from the tables below, uppercased.

#### Local filesystem example

```env
TUWUNEL_STORAGE_PROVIDER__MEDIA__LOCAL__BASE_PATH="/var/lib/tuwunel/media"
TUWUNEL_STORAGE_PROVIDER__MEDIA__LOCAL__CREATE_IF_MISSING="false"
```

#### S3 example

```env
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__BUCKET="my-matrix-media"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__REGION="us-east-1"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__KEY="AKIAIOSFODNN7EXAMPLE"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__SECRET="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

#### GCS example

```env
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_GCS__GCS__BUCKET="my-matrix-media"
```

No credential variables: on GCE/GKE the attached service account is used.

#### Self-hosted S3-compatible example

```env
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__ENDPOINT="https://minio.example.com:9000"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__BUCKET="matrix-media"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__REGION="us-east-1"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__KEY="minioadmin"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__SECRET="minioadmin"
TUWUNEL_STORAGE_PROVIDER__MEDIA_ON_S3__S3__USE_VHOST_REQUEST="false"
```

The `media_storage_providers` and `store_media_on_providers` lists are
top-level settings and follow the standard env var pattern using TOML array
syntax:

```env
TUWUNEL_MEDIA_STORAGE_PROVIDERS='["media", "media_on_s3"]'
TUWUNEL_STORE_MEDIA_ON_PROVIDERS='["media_on_s3"]'
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

## Importing media from a Conduit S3 bucket

When migrating from Conduit (see [Deployment](../deploying.md)), media that lived
in an S3 bucket rather than on local disk is imported on first boot, the same way
filesystem-backed Conduit media is. Point Tuwunel at the source bucket with a
named storage provider, and scope your destination so the import does not write
back into it.

The example below imports a Conduit S3 bucket into Tuwunel's own S3 bucket. A
local destination works the same way; only the destination provider differs.

```toml
[global]
# Read and write normal media only on the destination, so the import does not
# also copy media back into the read-only source bucket.
media_storage_providers = ["media"]

# Name the provider the importer reads Conduit's originals from.
conduit_source_media_provider = "conduit_source"

# Match Conduit's media.directory_structure. The default Deep { length = 2,
# depth = 2 } is shown; for a flat layout (Conduit v0.10.0) set depth = 0.
conduit_media_directory_depth  = 2
conduit_media_directory_length = 2

# Destination: Tuwunel's own media store.
[global.storage_provider.media.s3]
endpoint = "https://s3.example.com"
bucket   = "tuwunel-media"
region   = "us-east-1"
key      = "AKIAIOSFODNN7EXAMPLE"
secret   = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

# Source: Conduit's existing media bucket, read-only during the import.
[global.storage_provider.conduit_source.s3]
endpoint  = "https://s3.example.com"
bucket    = "conduit-media"
region    = "us-east-1"
key       = "AKIAIOSFODNN7EXAMPLE"
secret    = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
base_path = "media"   # Conduit's media.path prefix; omit if it had none
```

On first boot Tuwunel reads each original from the source bucket and re-uploads
it to the destination through its normal media path. The source object key is
reconstructed from the content hash using `conduit_media_directory_depth` and
`conduit_media_directory_length`, so those must match the source Conduit's
`media.directory_structure`. If Conduit used a path prefix (its `media.path`),
set the source provider's `base_path` to it.

A few things to know:

- **Set `media_storage_providers` to the destination only.** Otherwise the
  importer also writes a second copy of every file back into the read-only source
  bucket. It is harmless (the key namespaces differ) but wasteful, and it
  continues for normal uploads afterward.
- **The import is a copy, not a move.** The source bucket is left untouched. Once
  the migration completes and you have verified your media, remove the
  `conduit_source` provider and the `conduit_source_media_*` settings, then delete
  the old objects from the source bucket at your convenience.
- **A transient bucket error stops the import safely.** If the source bucket is
  unreachable, Tuwunel retries a few times and then aborts startup with a clear
  message rather than dropping media. Nothing is left half-migrated: fix the
  bucket and restart, and the import resumes from the beginning. A genuinely
  missing object (a metadata entry whose file was already deleted) is skipped, not
  treated as an error.
- **Thumbnails are regenerated, not imported.** Only original files are copied;
  Tuwunel regenerates thumbnails on demand.
- **For path-style requests** (Conduit's `bucket_use_path = true`), set
  `use_vhost_request = false` on the source provider.

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
