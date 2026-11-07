# Setting up TURN/STUN

A TURN server relays call traffic and is required to make or receive legacy
calls in Matrix.
Tuwunel suggests using [Coturn](https://github.com/coturn/coturn) for this
purpose, which is also available as a Docker image. To use a service instead
of running your own TURN server, see
[Hosted TURN providers](#hosted-turn-providers).

> [!NOTE]
> If you are setting up MatrixRTC and only need a TURN server to improve
> Livekit call reliability (not legacy calling), Livekit ships its own
> built-in TURN server. See
> [Using the Livekit Built-In TURN Server](matrix_rtc.md#using-the-livekit-built-in-turn-server)
> in the MatrixRTC documentation. This page covers standalone Coturn and
> hosted TURN providers for legacy Matrix calls. If you run both Coturn and
> Livekit, review
> [External TURN Integration](matrix_rtc.md#external-turn-integration) for
> the port allocation considerations.

### Configuration

Create a configuration file called `coturn.conf` containing:

```conf
use-auth-secret
static-auth-secret=<a secret key>
realm=<your server domain>
```

A common way to generate a suitable alphanumeric secret key is by using
`pwgen -s 64 1`.

These same values need to be set in Tuwunel. See the [example
config](../configuration/examples.md) in the TURN section for configuring
these and restart Tuwunel after.

`turn_secret` or a path to `turn_secret_file` must have a value of your
coturn `static-auth-secret`, or use `turn_username` and `turn_password`
if using legacy username:password TURN authentication (not preferred).

`turn_uris` must be the list of TURN URIs you would like to send to the
client. Typically you will just replace the example domain `example.turn.uri`
with the `realm` you set from the example config.

If you are using TURN over TLS, you can replace `turn:` with `turns:` in the
`turn_uris` config option to instruct clients to attempt to connect to TURN
over TLS. This is highly recommended.

If you need unauthenticated access to the TURN URIs, or some clients may be
having trouble, you can enable `turn_guest_access` in Tuwunel which disables
authentication for the TURN URI endpoint
`/_matrix/client/v3/voip/turnServer`.

### Run

Run the [Coturn](https://hub.docker.com/r/coturn/coturn) image using

```bash
docker run -d --network=host \
  -v $(pwd)/coturn.conf:/etc/coturn/turnserver.conf \
  coturn/coturn
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

The TURN server requires the appropriate ports to be forwarded where your
installation is behind NAT. These are currently `3478/tcp`, `3478/udp`,
`5349/tcp`, and `5349/udp`.

### Hosted TURN providers

Instead of running Coturn, you can use a TURN service that issues expiring
credentials (a username and password) through an HTTP API. Tuwunel fetches
the credentials and server addresses, called URIs, from the provider. Clients
receive them through the existing
`/_matrix/client/v3/voip/turnServer` endpoint and need no changes.

Configure only one of the providers below. While a provider is configured,
Tuwunel ignores `turn.uris`, the shared secret and the static credentials.
Add the chosen provider section to the configuration file your Tuwunel
service loads. Packaged services use `/etc/tuwunel/tuwunel.toml`; see
[Configuration](../configuration.md#basics) if you run Tuwunel another way.

#### Cloudflare

1. Create a TURN key for Cloudflare's Realtime TURN service in the Cloudflare
   dashboard.
2. On the machine running Tuwunel, save the key's API token in
   `/etc/tuwunel/.cloudflare_turn_token`. The file must be readable by Tuwunel.
3. Add the following section to your Tuwunel configuration file, replacing
   `<TURN key ID>` with your key's ID.

```toml
[global.turn.cloudflare]
key_id = "<TURN key ID>"
token_file = "/etc/tuwunel/.cloudflare_turn_token"
```

4. [Reload Tuwunel's configuration](../deploying/configuration-reload.md).

Cloudflare credentials last at most 48 hours. Tuwunel shortens a `turn.ttl`
above that limit. It sends clients only Cloudflare's TURN URIs and excludes
those using port `53`, because browsers refuse to connect to that port.

#### Any other provider

Use `[global.turn.endpoint]` if your provider has an HTTP endpoint that
answers `GET <url>?service=turn&ttl=<seconds>` in either of the formats below.
The first is the TURN REST API format, which is also what Matrix clients
receive:

```json
{
  "username": "1700000000:example",
  "password": "<password>",
  "ttl": 86400,
  "uris": ["turn:turn.example.com?transport=udp"]
}
```

The second is a WebRTC ICE server list, either `{"iceServers": [...]}` or a
bare list. Each entry describes a server with `urls`, `username` and
`credential`. Matrix clients accept a single username and password. Tuwunel
uses the first entry with a nonempty username and password, then drops entries
whose credentials differ. In either format, only `turn:` and `turns:` URIs
reach clients, as the Matrix specification requires.

The [STUNner auth service](https://github.com/l7mp/stunner-auth-service)
answers in both formats. If your provider's API uses a different format,
you can put an adapter in front of it to translate requests and responses.

1. If the provider requires a bearer token, save it in
   `/etc/tuwunel/.turn_endpoint_token` on the machine running Tuwunel. The
   file must be readable by Tuwunel.
2. Add the following section to your Tuwunel configuration file. Replace
   `url` with your provider's credential endpoint. Omit `token_file` if the
   provider does not use bearer authentication.

```toml
[global.turn.endpoint]
url = "http://127.0.0.1:8088/"
token_file = "/etc/tuwunel/.turn_endpoint_token"
```

3. [Reload Tuwunel's configuration](../deploying/configuration-reload.md).

When `token` or `token_file` is set, Tuwunel sends the token in the
`Authorization` header as a bearer token. Query parameters already in `url`
are kept, so an API key in the query is preserved.

#### Shared credentials and failures

All users share one set of credentials, renewed once half its lifetime has
passed. The provider therefore sees about two requests per credential lifetime
however many clients ask. Tuwunel never sends it a Matrix user ID.

After a failed request, Tuwunel waits a minute before asking the provider
again. During that time, clients receive cached credentials if they have not
expired, or a `502` error if no unexpired credentials remain. If requests
keep failing, check that Tuwunel can read the token file and that the
configured provider URL or Cloudflare key ID is correct.
