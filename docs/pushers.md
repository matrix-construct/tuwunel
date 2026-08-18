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

When a forward proxy carries the request, its endpoint is exempt from
`ip_range_denylist`. HTTP(S) forward proxies and `socks4a` or `socks5h`
resolve destination names remotely, so they must enforce the destination
network boundary themselves because Tuwunel cannot inspect that address.
Destination addresses remain subject to the denylist for direct requests and
for locally resolving `socks4` or `socks5` proxies, so the LAN guidance above
still applies.

Every push notification is sent to the gateway's Matrix spec path
`/_matrix/push/v1/notify`; this path is not configurable. The
`notification_push_path` option only names the path suffix stripped from
the end of a stored pusher URL before that spec path is appended to the
remainder. An occurrence anywhere earlier in the URL is left in place,
and a URL whose path does not end with the configured value is used
unchanged, so leave the option at its default unless your pushers
register URLs that end in a different path needing to be removed first.
