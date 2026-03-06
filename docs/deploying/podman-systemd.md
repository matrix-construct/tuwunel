# Podman, Quadlets, and systemd

For a rootless setup, we can use quadlets and systemd to manage the container lifecycle.

> If this is the first container managed with quadlets for your user, ensure that linger is enabled so your containers are not killed after logging out.
>
> `sudo loginctl enable-linger <username>`  
 
## Installation
1. Copy quadlet files to `~/.config/containers/systemd/tuwunel`

### tuwunel.container

<details>
<summary>tuwunel container quadlet</summary>

```
{{#include ../../quadlet/tuwunel.container}}
```

</details>

### tuwunel-db.volume

<details>
<summary>tuwunel database volume quadlet</summary>

```
{{#include ../../quadlet/tuwunel-db.volume}}
```

</details>

### tuwunel.env

<details>
<summary>tuwunel environment variable quadlet</summary>

```env
{{#include ../../quadlet/tuwunel.env}}
```

</details>




```
mkdir -p ~/.config/containers/systemd/tuwunel
```

2. 
	- Modify tuwunel.env to desired values.  
	- Modify [tuwenel.toml](generic.md#creating-the-tuwunel-configuration-file) to desired values. This can be saved in your user home directory if desired. 

3. Reload daemon to generate our systemd unit files: 
```
systemctl --user daemon-reload
```
4. Start tuwunel:
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
#### Troubleshooting systemd unit file generation

Look for errors in the output:


`/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun`

