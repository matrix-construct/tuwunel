# tuwunel in Podman systemd

Copy [tuwunel.container](tuwunel.container) to ~/.config/containers/systemd/tuwunel.container.
Reload daemon:
```
systemctl --user daemon-reload
```
Start the service:
```
systemctl --user start tuwunel
```

To check the logs, run:
```
journalctl -eu tuwunel.container --user
```
