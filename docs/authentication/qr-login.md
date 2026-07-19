# QR Code Login

Tuwunel supports the unstable MSC4108 and MSC4388 rendezvous transports for QR
code login. A new device and an existing device exchange short-lived handshake
messages through a rendezvous session, then the OAuth device authorization
grant completes the sign-in.

## Requirements

QR code login requires the built-in OIDC authorization server and a configured
`well_known.client` URL. The approving step can use a configured
`identity_provider`. Without one, enable `oidc_native_auth` to authenticate the
approving user against this server's own accounts.

## Configuration

Both rendezvous transports are enabled by default and share one in-memory
session pool. These settings are reloadable:

- `rendezvous_enabled` defaults to `true`. It advertises and serves both MSC4108
  and MSC4388 rendezvous sessions.
- `rendezvous_session_max_bytes` defaults to `4096`. It limits one session's
  payload size. MSC4388 payloads remain capped at 4096 bytes when this setting
  is larger.
- `rendezvous_session_ttl` defaults to `600`. It sets the seconds a session
  lives after its last write. Devices also time the whole sign-in against the
  expiry advertised at creation, so the window covers an interactive login on
  the approval page.
- `rendezvous_max_sessions` defaults to `100`. It limits concurrent sessions
  before the oldest is evicted.
- `rendezvous_authenticated_only` defaults to `true`. It requires an access
  token for MSC4388 discovery and creation. MSC4108 remains open.
- `rendezvous_rc_per_second` defaults to `10`. It sets the per-client-IP
  MSC4388 request refill rate. A zero value is treated as one.
- `rendezvous_rc_burst_count` defaults to `20`. It sets the MSC4388 request
  burst depth. A zero value is treated as one.

Both transports share the configured session count, expiry, eviction, and
restart behavior. Sessions are held in memory, cleared on restart, and removed
lazily after their lifetime expires. Creating a session above the configured
count evicts the oldest session. The MSC4388 request limiter is separate from
OIDC throttling.

## Reverse proxies

Do not cache responses under either unstable rendezvous namespace:

- `/_matrix/client/unstable/org.matrix.msc4108/rendezvous`
- `/_matrix/client/unstable/io.element.msc4388/rendezvous`

For MSC4108, pass the `ETag`, `If-Match`, and `If-None-Match` headers through
unchanged. Browser clients also need the server's
`Access-Control-Expose-Headers: ETag` response header to reach the client
unchanged.

For MSC4388, preserve the `Sec-Fetch-*` request headers. Tuwunel returns 403 for
browser navigation requests to session URLs.
