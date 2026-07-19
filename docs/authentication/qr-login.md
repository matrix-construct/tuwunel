# QR Code Login

Tuwunel supports the unstable MSC4108 QR code login flow. A new device and an
existing device exchange short-lived handshake messages through a rendezvous
session, then the OAuth device authorization grant completes the sign-in.

## Requirements

QR code login requires the built-in OIDC authorization server and a configured
`well_known.client` URL. The approving step currently also requires an
`identity_provider`; approval with native accounts is not available yet.

## Configuration

The rendezvous API is enabled by default. These settings are reloadable:

- `rendezvous_enabled` defaults to `true`. It advertises and serves QR login
  rendezvous sessions.
- `rendezvous_session_max_bytes` defaults to `4096`. It limits one session's
  payload size.
- `rendezvous_session_ttl` defaults to `600`. It sets the seconds a session
  lives after its last write. Devices also time the whole sign-in against the
  expiry advertised at creation, so the window covers an interactive login on
  the approval page.
- `rendezvous_max_sessions` defaults to `100`. It limits concurrent sessions
  before the oldest is evicted.

Sessions are held in memory, cleared on restart, and removed lazily after their
lifetime expires. Creating a session above the configured count evicts the
oldest session.

## Reverse proxies

Pass the `ETag`, `If-Match`, and `If-None-Match` headers through unchanged. Do
not cache responses under
`/_matrix/client/unstable/org.matrix.msc4108/rendezvous`. Browser clients also
need the server's `Access-Control-Expose-Headers: ETag` response header to reach
the client unchanged.
