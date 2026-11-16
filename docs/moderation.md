# Policy and Moderation

This chapter covers the federation-level, room-level, and user-level controls
Tuwunel exposes to operators for keeping their server, its users, and the rooms
they participate in within a desired policy posture. Most controls fall into
one of three layers: configuration (statically applied at startup or reload),
admin-room commands (live operator actions), and per-room policy delegation
(MSC4284).

Configuration values shown here are documented in the [example
configuration](./configuration/examples.md#example-configuration) with their
defaults and full per-knob explanation. This page summarises each knob's role
in the moderation toolkit rather than restating the example file.

## Federation server blocklists

The largest hammer. Three knobs in `tuwunel-example.toml`:

- `forbidden_remote_server_names`: regex patterns matched against room IDs,
  room aliases, sender server names, sender users' server names, inbound
  X-Matrix origins, and outbound federation handlers. Effectively a global
  ACL. Matching servers can neither send to nor receive from this homeserver,
  and local clients cannot join their rooms.
- `allowed_remote_server_names_experimental`: an allow-list counterpart. When
  non-empty, anything not matching is denied. `forbidden_remote_server_names`
  is still applied after, so a wide allow (`.*\\.example\\.com`) plus a narrow
  deny (`bad\\.example\\.com`) is a valid composition.
- `forbidden_remote_room_directory_server_names`: limits the broader knob to
  outbound room-directory queries, useful when the goal is to keep users from
  discovering rooms on a server without cutting all federation with it.

These checks are inexpensive and apply on every relevant code path. Use a
regex denylist when a server has crossed a hard line; use the directory-only
variant for cases that warrant friction without isolation.

## Admin commands for moderation

The admin room (joined automatically on first registration unless
`create_admin_room = false`) accepts the following moderation-relevant
commands. Run any command with `--help` for argument detail.

### Rooms

- `!admin rooms moderation ban-room <room>`: bans a room, evicts every local
  user including admins, removes local aliases, unpublishes from the
  directory, and disables federation with the room.
- `!admin rooms moderation ban-list-of-rooms`: bulk variant; takes a code
  block of room IDs or aliases.
- `!admin rooms moderation unban-room <room>`: reverses a ban and re-enables
  inbound federation handling; the `m.federate` caveat under
  [Federation](#federation) below applies here too.
- `!admin rooms moderation list-banned-rooms`: lists every banned room.
- `!admin rooms delete <room>`: harder than ban; removes the room from the
  database after evicting users.

### Federation

- `!admin federation disable-room <room>`: blocks new inbound PDUs for one
  room without banning it. Useful for stalling a runaway room while
  investigating.
- `!admin federation enable-room <room>`: re-enables inbound handling.
  Neither command touches the `m.federate` property of the room's
  `m.room.create` event, which is fixed when the room is created; see
  [Troubleshooting](./troubleshooting.md).
- `!admin federation incoming-federation`: lists the incoming events whose
  missing earlier events the server is fetching or processing (room, event,
  origin server, phase, elapsed time), then the rooms where another federation
  step, such as a transaction, a join or a backfill, holds the room's
  federation lock. A room with a listed event is left out of the second list.
- `!admin federation fetch-support-well-known <server>`: fetches a remote
  server's `.well-known/matrix/support` record (administrator and security
  contacts), letting you raise abuse reports out-of-band before resorting to
  blocks.

### Users

- `!admin users deactivate <user>`: deactivates a local account; by default
  also leaves all rooms. The last active admin cannot be deactivated, by this
  command or any other route: make another user an admin first.
- `!admin users deactivate-all`: bulk variant accepting a code block of
  usernames.
- `!admin users reject-invites <user>`: rejects all pending invites, with
  an optional reason. Useful when an account has been targeted by an invite
  flood.
- `!admin users redact-event <event_id>`: forcibly redacts an event from a
  local sender even when the user is offline or unwilling.
- `!admin users force-demote <user> <room>`: drops a user's power level to
  the room default when permissions allow.
- `!admin users set-profile-key <user> <key> <value>`: sets a single profile
  key (e.g. `displayname`, `avatar_url`, `m.tz`, or a custom key) on a local
  user, for example to remove an abusive display name. With `--clear` in
  place of a value it removes the key instead, and then also accepts a remote
  user (without `--propagate-to`), dropping this server's cached copy of their
  field. The value returns on their next profile lookup if their server still
  serves it.
- `!admin users refresh-profile <user>`: re-fetches a remote user's profile
  from their server and drops cached fields it no longer serves, such as a
  status the user has since removed. Clients see the removal on their next
  sync.
- `!admin users delete-room-tag` / `put-room-tag`: room-tag housekeeping;
  the `m.server_notice` tag pinned to the admin room is the typical use.

### Media

See [Multimedia and Storage > Management](./media/management.md) for the full
admin command reference. Tools relevant to moderation:

- `!admin media delete --mxc <mxc_uri>`: single-file removal.
- `!admin media delete-by-event --event-id <event_id>`: removes every MXC
  URI referenced by an event.
- `!admin media delete-list`: bulk removal from a code block of MXC URIs.
- `!admin media delete-range <duration> --older-than|--newer-than`: time-range
  delete; remote-only by default, local with the
  `--yes-i-want-to-delete-local-media` confirmation.
- `!admin media delete-all-from-user <username>`: removes every upload by a
  local user.
- `!admin media delete-all-from-server <server>`: drops every cached copy of
  remote media from the named server.

## Media policy

- `prevent_media_downloads_from`: regex denylist for remote media downloads.
  Narrower than `forbidden_remote_server_names`; the server can still
  federate, just no media flows in.
- `media_storage_provider` (under `[media.storage]`): when files live in an
  external provider, deletion semantics still apply through the same admin
  commands.

## Identifier blocklists

Applied at registration, alias creation, and at startup as warnings against
existing entries.

- `forbidden_alias_names`: regex patterns matched against newly created room
  aliases and custom room IDs. Existing aliases that match are warned about
  on startup but not removed.
- `forbidden_usernames`: same shape, applied to local username availability
  checks and registration.

## Invite gating

- `block_non_admin_invites`: when `true`, only server admins can send room
  invites (local or remote) and only admins can receive remote invites. The
  cheapest mitigation for an invite-spam wave on a small or invite-only
  server.

## Auto-deactivation triggers

- `auto_deactivate_banned_room_attempts`: when `true`, any local user who
  attempts to join a banned room, an alias matching `forbidden_alias_names`,
  or an alias / room ID containing a `forbidden_remote_server_names` match,
  is fully deactivated and made to leave every room. Off by default because
  rooms are sometimes banned for non-moderation reasons.

## Per-room policy delegation (MSC4284)

If a room uses a *policy server*, its moderators delegate event signing to
that server under MSC4284. Its ed25519 signature must be present on every
non-policy event in the room. Tuwunel adds the signature to
`event.signatures` and passes it to other servers through federation, the
exchange of room events between servers. For local sends, Tuwunel asks the
policy server to sign through `/sign`. For incoming events, it verifies the
signature and requests a new one if it is missing or invalid.

Only rooms with a valid `m.room.policy` state event use this policy. Rooms
without that state event are unaffected.

### Policy server settings

Set these options in your Tuwunel configuration file under
`[global.rooms.policy_server]`.

- `enable`: takes part in the room's policy (default `true`; legacy key
  `enable_policy_servers`).
- `advisory_room_versions`: applies policy servers in room versions where
  the signature is advisory rather than an authorization rule (default
  `true`). This includes version `12` and every earlier version, covering
  every room version Tuwunel currently supports.
- `fail_open`: lets events through when the policy server gives no verdict
  (default `false`, the behavior the specification requires).
- `request_timeout`: time limit in seconds (default `30`; legacy key
  `policy_server_request_timeout`) for both outbound `/sign` requests and
  inbound signature-fetch requests.

MSC4416 (the room-version-13 successor that
makes a missing or invalid policy signature an auth-rule rejection rather
than a soft fail) is not yet implemented in Tuwunel and depends on upstream
typing work.

> [!WARNING]
> Turning off `enable` is not recommended. Other servers in the room keep
> enforcing the policy, so local users see events the rest of the room hides.
> Events sent from your server are withheld elsewhere when the policy server
> would have refused them.

Local sends wait for the policy server before being sent to other servers.
Other local sends to the same room wait behind that request.
`request_timeout` limits this wait. A small `request_timeout` together with
`fail_open = true` sends after a short wait when the policy server is slow
or down.

### Refusals and unavailable policy servers

By default, an outgoing event the policy server does not sign is not sent.
An incoming event left without a valid signature soft-fails, which keeps it
out of the room timeline; [Soft-failed events and
recovery](#soft-failed-events-and-recovery) covers when its state can still
reach clients.

An explicit refusal stops outgoing sends and soft-fails incoming events,
even with `fail_open = true`. A refusal is `400 M_FORBIDDEN` or, on the
unstable variant, `200 OK` with no signature for the configured `via`.
Outgoing sends fail with the policy server's own error.

When the policy server gives no verdict, `fail_open = true` lets the event
through. Even with the default `fail_open = false`, a local user's leave,
a kick or a ban is sent when there is no verdict, so an outage cannot trap
users in a room. This includes rejecting an invite or withdrawing a knock.
An explicit refusal still stops these events.

For a local send blocked because there is no verdict, the client receives
one of these errors:

| Cause | Client response |
| --- | --- |
| The policy server times out, cannot be reached, is backed off, or returns a signature that fails verification | `503 M_UNKNOWN` |
| The policy server returns an error | Its error is passed through, except that `401` becomes `403` and an error about the user's own session, account or homeserver (`M_UNKNOWN_TOKEN`, `M_MISSING_TOKEN`, `M_USER_DEACTIVATED`, `M_USER_LOCKED`, `M_USER_SUSPENDED`, `M_CONSENT_NOT_GIVEN` or `M_RESOURCE_LIMIT_EXCEEDED`) becomes `403 M_FORBIDDEN` keeping only the policy server's message |
| Your server's configuration forbids the request because federation is disabled or the policy server's name is forbidden | `403 M_FORBIDDEN`. Tuwunel also logs a warning |

Tuwunel verifies returned signatures against the key in the room's
`m.room.policy` state. A signature that fails verification is never added
to the event and counts as no verdict. If an incoming event's policy
signature fails verification and no valid replacement is obtained, Tuwunel
stores the event without that signature.

Tuwunel pauses requests to a policy server that times out, has a transport
failure, returns a server error or rate-limits requests. This *backoff*
lasts as long as any retry time the policy server specifies, up to 24
hours. During backoff, Tuwunel decides whether events pass without asking the
policy server. Ordinary federation traffic to that server shares the same
backoff.

If `via` names your own server, Tuwunel contacts it over federation like
any other server. Tuwunel serves no `/sign` itself. This setup works only
when a reverse proxy routes `/_matrix/policy/v1/sign` for that name to a
policy server. Otherwise, events in that room get no verdict.

### Soft-failed events and recovery

A soft-failed event stays stored but never appears in the room timeline or
reaches clients as a timeline event. Other servers can still fetch it
directly over federation. An event can soft-fail because its sender lacks
permission to redact, a policy server refused it or gave no verdict, or it
failed authorization against the current room state (server-server receipt
check 6).

A refused membership or power-level change cannot take effect when it first
arrives. Its stored state can still affect the room later through *state
resolution*, which reconciles room state from different event branches.
When a later accepted event names a soft-failed event among its predecessors,
Tuwunel resolves the room's current state against that branch, as the
specification requires. If state resolution keeps the soft-failed event, it
becomes current state. Clients receive it through the `state` of their sync
responses, though it still never enters the timeline.

The soft-fail verdict is not permanent. When federation supplies the event
again, Tuwunel re-checks it if the retry interval has elapsed. That interval
widens from five minutes to a day. If the event passes, Tuwunel clears the
verdict. An explicit policy-server refusal stays cached for 24 hours.
Re-checks within that window use the cached refusal. Tuwunel asks the policy
server again only after it expires.

To clear the stored verdicts for a room, run
`!admin rooms clear-soft-failed-events <room>` in the admin room. This also
clears cached policy decisions and retry delays. Events are checked again
when federation supplies them.

### Privacy in encrypted rooms

The room event (PDU) is forwarded to the policy server for signing. The
policy server cannot read the encrypted content, but it can read metadata
(sender, timestamp, room, event type). The room decides whether to use
policy delegation in an encrypted room. Tuwunel does not block it.

## URL previews and outbound network policy

URL preview generation is a frequent attack surface (SSRF, exfiltration via
forced fetches). Tuwunel exposes both a domain policy layer and an IP egress
layer.

Domain policy:

- `url_preview_domain_contains_allowlist`, `url_preview_url_contains_allowlist`:
  permissive contains-match allowlists. The contains-match is intentionally
  loose; treat as "anything mentioning this string passes".
- `url_preview_domain_explicit_allowlist`: strict equality match on the host.
- `url_preview_domain_explicit_denylist`: strict equality match, evaluated
  before the allowlists unless one of them holds `"*"`.
- `url_preview_check_root_domain`: when `true`, retries the explicit
  denylist and allowlist against the host with its first label removed, so an
  explicit allow on `wikipedia.org` also lets `en.wikipedia.org` through, but
  not `en.m.wikipedia.org`.
- `url_preview_max_spider_size` (bytes; SI/IEC suffix accepted): caps the
  spider response body.
- `url_preview_bound_interface`: pins outbound preview requests to a specific
  interface or source IP. Linux/Android/Fuchsia accept interface names
  (`eth0`); other platforms accept addresses.

IP egress:

- `ip_range_denylist`: list of IPv4/IPv6 CIDR ranges Tuwunel will not send
  outbound requests to. Defaults to RFC1918, loopback, multicast, link-local,
  and the documentation/testnet ranges. This is application-layer enforcement
  and not a substitute for a host firewall, but it closes the obvious SSRF
  vectors out of the box. Forward proxy endpoints are exempt. Destination
  addresses remain filtered for direct requests and for locally resolving
  `socks4` or `socks5` proxies. HTTP(S) forward proxies and `socks4a` or
  `socks5h` resolve destinations remotely, so they must enforce their own
  egress policy. Set to `[]` only if a firewall is enforcing the same
  constraints upstream.

## Redaction retention and forensics

Moderation actions often hinge on what an event said before redaction.

- `save_unredacted_events` (default `true`): keeps the pre-redaction PDU in
  storage so admins can recover content for incident review.
- `redaction_retention_seconds` (default 60 days): how long unredacted copies
  live before being dropped. Lower this for tighter privacy posture; raise
  it when investigations span longer horizons.
- `allow_room_admins_to_request_unredacted_events` (default `true`): lets a
  user with `redact` power level retrieve unredacted copies via MSC2815.
  Server admins can request regardless.
- `disable_local_redactions`: blocks local users from sending redactions
  (server admins exempt). Useful only in archival or read-only deployments;
  most operators leave this off.

The `!admin debug get-retained-pdu` command surfaces a retained event from
the admin room.

## Responding to a spam incident

When a server sends spam media to your users, the typical response is:

**1. Identify the source server.**
Check the MXC URIs in the reported messages. The server name is the
authority component: `mxc://<server_name>/<media_id>`. The sender server
on the offending event is the canonical source even when media is hosted
elsewhere.

**2. Delete cached copies of the spam media.**

```
!admin media delete-all-from-server badserver.tld
```

**3. Block future media downloads from that server.**
Add the server to `prevent_media_downloads_from` in your config and reload
or restart Tuwunel:

```toml
prevent_media_downloads_from = ["badserver\\.tld$"]
```

**4. If the spam arrived within a known time window**, use `delete-range` to
catch anything missed:

```
!admin media delete-range 2h --newer-than
```

**5. If you have a list of specific MXC URIs** (e.g. from a moderation tool
or a shared blocklist), use `delete-list` to remove them in bulk.

**6. Consider server-level federation blocks** via
`forbidden_remote_server_names` if the server is persistently abusive. This
blocks all federation traffic, not just media, and so is the right tool only
once the source has demonstrated it will not stop.

**7. If the spam came as invite floods**, set `block_non_admin_invites = true`
to halt all non-admin invite traffic during the incident, then run
`!admin users reject-invites <user>` for affected accounts. Re-enable invites
once the source server is blocked.

**8. If a single room is the vector** (for example, a public room being used
to flood a user's timeline), `!admin rooms moderation ban-room <room>` evicts
your local users and severs federation with the room without affecting
other rooms on the same server.

**9. For ongoing moderation against a class of senders**, consider whether
a per-room MSC4284 policy server is appropriate. This delegates per-event
signing to a moderation service, with the trade-offs described in
[Per-room policy delegation](#per-room-policy-delegation-msc4284) above.
This is a heavier setup than a one-time response and is worth it only when
the room expects sustained policy enforcement.
