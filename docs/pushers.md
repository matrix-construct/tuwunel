# Push Notifications

Tuwunel implements the homeserver side of the Matrix
[Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/).
Clients create pushers naming their app's push gateway URL, and Tuwunel
posts each notifiable event to that URL. No server-side setup is needed
for ordinary hosted gateways. For notifications by email, see
[Email notifications](#email-notifications).

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

## Email notifications

To offer email notifications to your users, first set up outgoing mail
and have users verify their addresses:

1. Configure the outgoing mail service (SMTP) in `[global.smtp]` in your
   Tuwunel configuration file. Use the same setup as
   [email verification](authentication/legacy.md#email-verification).
   Without it, the server refuses email pushers and the client's toggle
   fails.
2. Have each user verify an email address on their account. The server
   refuses an email pusher for any other address. It also deletes a
   pusher whose address is later removed from the account.

Clients such as Element Web offer a toggle for each email address verified
on the account. Turning it on registers an email pusher for that address,
a request for Tuwunel to email unread notifications. Users stop the emails
by turning email notifications off in their client.

Tuwunel waits `push.email_delay` seconds (`600` seconds, or 10 minutes, by
default) before an unread notification can trigger an email. A room that
appeared in a recent email can make it wait longer. Once a notification is
due, Tuwunel includes all other unread notifications waiting for that user
in the same email. Notifications the user reads before the email is sent
are left out.

Repeated emails about the same room are spaced further apart. After the
first email that includes a room, the room must wait the configured delay
before it can trigger another email. This wait is capped at one day. Each
further email that includes the room multiplies the wait by `144`, up to
that cap. With the default delay, the wait is one day from the second email
onward.

The room's notifications can still appear sooner in an email triggered by
another room. Reading the room resets its wait. Setting `push.email_delay`
to `0` sends notifications as they arrive, without either wait.

Each email is plain text and shows the newest notifications grouped by
room, with the sender, time, and start of each message. It shows up to 20
notifications and counts any others at the end. For an encrypted message,
it shows the sender and time but replaces the message content with
`Encrypted message`. Rooms link to matrix.to.
