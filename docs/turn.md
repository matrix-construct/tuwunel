# Setting up TURN/STURN

In order to make or receive calls, a TURN server is required. Tuwunel suggests
using [Coturn](https://github.com/coturn/coturn) for this purpose, which is also
available as a Docker image.

### Configuration

Create a configuration file called `coturn.conf` containing:

```conf
use-auth-secret
static-auth-secret=<a secret key>
realm=<your server domain>
```

A common way to generate a suitable alphanumeric secret key is by using `pwgen
-s 64 1`.

These same values need to be set in Tuwunel. See the [example
config](configuration/examples.md) in the TURN section for configuring these and
restart Tuwunel after.

`turn_secret` or a path to `turn_secret_file` must have a value of your
coturn `static-auth-secret`, or use `turn_username` and `turn_password`
if using legacy username:password TURN authentication (not preferred).

`turn_uris` must be the list of TURN URIs you would like to send to the client.
Typically you will just replace the example domain `example.turn.uri` with the
`realm` you set from the example config.

If you are using TURN over TLS, you can replace `turn:` with `turns:` in the
`turn_uris` config option to instruct clients to attempt to connect to
TURN over TLS. This is highly recommended.

If you need unauthenticated access to the TURN URIs, or some clients may be
having trouble, you can enable `turn_guest_access` in Tuwunel which disables
authentication for the TURN URI endpoint `/_matrix/client/v3/voip/turnServer`

### Run

Run the [Coturn](https://hub.docker.com/r/coturn/coturn) image using

```bash
docker run -d --network=host -v
$(pwd)/coturn.conf:/etc/coturn/turnserver.conf coturn/coturn
```

or docker-compose. For the latter, paste the following section into a file
called `docker-compose.yml` and run `docker compose up -d` in the same
directory.

```yml
services:
    turn:
      container_name: coturn-server
      image: docker.io/coturn/coturn
      restart: unless-stopped
      network_mode: "host"
      volumes:
        - ./coturn.conf:/etc/coturn/turnserver.conf
```

To understand why the host networking mode is used and explore alternative
configuration options, please visit [Coturn's Docker
documentation](https://github.com/coturn/coturn/blob/master/docker/coturn/README.md).

For security recommendations see Synapse's [Coturn
documentation](https://element-hq.github.io/synapse/latest/turn-howto.html).

The TURN server requires the appropriate ports to be forwarded, where your installation is behind NAT. 
These are currently `3478/tcp`, `3478/udp`, `5349/tcp`, and `5349/udp`. 
