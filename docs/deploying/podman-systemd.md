# Podman, Quadlets, and systemd

For a rootless setup, we can use quadlets and systemd to manage the container lifecycle.

> [!IMPORTANT]
> If this is the first container managed with quadlets for your user, ensure that linger
> is enabled so your containers are not killed after logging out.
>
> `sudo loginctl enable-linger <username>`  

### Step One

Copy quadlet files to `~/.config/containers/systemd/tuwunel`

**tuwunel.container**

<details>
<summary>tuwunel container quadlet</summary>

```
{{#include ../../quadlet/tuwunel.container}}
```

</details>

**tuwunel-db.volume**

<details>
<summary>tuwunel database volume quadlet</summary>

```
{{#include ../../quadlet/tuwunel-db.volume}}
```

</details>

**tuwunel.env**

<details>
<summary>tuwunel environment variable quadlet</summary>

```env
{{#include ../../quadlet/tuwunel.env}}
```

</details>


```
mkdir -p ~/.config/containers/systemd/tuwunel
```

### Step Two

Modify `tuwunel.env` and [`tuwunel.toml`](generic.md#creating-the-tuwunel-configuration-file)
to desired values. This can be saved in your user home directory if desired.

### Step Three

- Reload daemon to generate our systemd unit files:

```
systemctl --user daemon-reload
```

### Step Four

- Start tuwunel:

```
systemctl --user start tuwunel
```

## Logging 

To check the logs, run:
```
systemctl --user status tuwunel
```
or

```
podman logs tuwunel-homeserver
```

## Health checking outside a quadlet

The quadlet above declares the health check explicitly, so quadlet users get the
same probe Docker users get. A bare `podman run` of the published image does
not, and the reason is worth knowing rather than working around blindly.

The image is published in OCI format, and the health check rides in its config
as an extra field, which is where Docker reads it from. Podman does not read
that field out of an OCI config
([containers/podman#25454](https://github.com/containers/podman/issues/25454),
[#18904](https://github.com/containers/podman/issues/18904), both open), so it
reports no health check at all:

```console
$ podman inspect --format '{{json .HealthCheck}}' ghcr.io/matrix-construct/tuwunel:latest
null
$ docker inspect --format '{{json .Config.Healthcheck}}' ghcr.io/matrix-construct/tuwunel:latest
{"Test":["CMD","tuwunel","--health-check"],...}
```

Nothing is missing from the image and nothing needs rebuilding in another
format. Pass the probe on the command line instead:

```bash
podman run -d --name tuwunel \
  --health-cmd '["/usr/bin/tuwunel", "--health-check"]' \
  --health-interval 30s \
  --health-timeout 15s \
  --health-start-period 1800s \
  --health-retries 3 \
  ghcr.io/matrix-construct/tuwunel:latest
```

The JSON array form is required. A bare string runs through a shell, and the
image is built `FROM scratch` with none. See [Health during a
migration](docker.md#health-during-a-migration) for why the start period is that
wide.

## Troubleshooting systemd unit file generation

Look for errors in the output:
`/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun`

