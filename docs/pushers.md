# Push Notifications

Tuwunel implements the homeserver side of the Matrix
[Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/).
Clients create pushers naming their app's push gateway URL, and Tuwunel
posts each notifiable event to that URL. No server-side setup is needed
for ordinary hosted gateways.

## UnifiedPush

UnifiedPush works out of the box. UnifiedPush gateways (embedded in push
servers such as ntfy and NextPush, or run standalone) accept the same
`/_matrix/push/v1/notify` requests, so there is no UnifiedPush-specific
server support to enable. The gateway discovery response
(`{"unifiedpush":{"gateway":"matrix"}}`) is served by the push server,
not by Tuwunel.

## Self-hosted gateways on private networks

Outbound requests, push notifications included, refuse loopback and
private-range destinations by default (the `ip_range_denylist` option).
A self-hosted gateway on a LAN address, for example an ntfy server on
`192.168.0.0/16`, is rejected at pusher creation with "HTTP pusher URL
is a forbidden remote address". Narrow `ip_range_denylist` to admit your
gateway's range. Plain `http://` gateway URLs are accepted.

A gateway serving the notify endpoint under a nonstandard path prefix
can be accommodated with the `notification_push_path` option.
