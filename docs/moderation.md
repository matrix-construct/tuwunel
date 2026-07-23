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
  federation.
- `!admin rooms moderation list-banned-rooms`: lists every banned room.
- `!admin rooms delete <room>`: harder than ban; removes the room from the
  database after evicting users.

### Federation

- `!admin federation disable-room <room>`: blocks new inbound PDUs for one
  room without banning it. Useful for stalling a runaway room while
  investigating.
- `!admin federation enable-room <room>`: re-enables inbound handling.
- `!admin federation incoming-federation`: lists rooms with active inbound
  PDU handlers.
- `!admin federation fetch-support-well-known <server>`: fetches a remote
  server's `.well-known/matrix/support` record (administrator and security
  contacts), letting you raise abuse reports out-of-band before resorting to
  blocks.

### Users

- `!admin users deactivate <user>`: deactivates a local account; by default
  also leaves all rooms.
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
  user, for example to remove an abusive display name.
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

MSC4284 lets a room's moderators delegate event signing to a third-party
*policy server* whose ed25519 signature must be present on every non-policy
event in the room. The signature folds into `event.signatures` and federates
transitively. Tuwunel implements outbound `/sign` on local sends, inbound
verification on federated receives, fetch-on-missing for inbound events
without a signature, and refusal/backoff caching to avoid hammering a server
that is rate-limiting or has refused.

Two configuration knobs:

- `enable_policy_servers`: master switch (default `false`). When `false`,
  Tuwunel ignores policy state entirely. When `true`, the gate engages only
  in rooms that carry a valid `m.room.policy` state event.
- `policy_server_request_timeout`: seconds (default `5`) for both outbound
  `/sign` and inbound signature-fetch requests.

Operator-relevant implications when enabling:

- **Per-room opt-in.** The global flag only allows the gate to engage; the
  room's own `m.room.policy` state event is what activates it. Rooms without
  the state event are unaffected.
- **Latency cost.** Every outbound send in a policy-room round-trips to the
  policy server before federating. The default 5-second cap prevents a
  single misbehaving policy server from stalling sends indefinitely.
- **Fail-open on transport failure.** Network errors and timeouts are logged
  and the event is sent or accepted unsigned, on the assumption that the
  next homeserver in the room will pick up the gap.
- **Fail-closed on explicit refusal.** A policy server returning
  `M_FORBIDDEN` (or, on the unstable variant, `200 OK` with no signature for
  the configured `via`) causes outbound sends to fail with `M_FORBIDDEN`,
  and inbound events to soft-fail.
- **Privacy in encrypted rooms.** The PDU is forwarded to the policy server
  for signing. Ciphertext is opaque, but metadata (sender, timestamp, room,
  event type) is not. Encrypted-room policy delegation is the room's call;
  Tuwunel does not block it.
- **Refusal and rate-limit caching.** Per-event refusals and per-policy-server
  `M_LIMIT_EXCEEDED` backoffs are persisted, so repeated arrivals of the
  same event do not re-hit a refusing or throttled server.

For room version compatibility, MSC4416 (the room-version-13 successor that
makes a missing or invalid policy signature an auth-rule rejection rather
than a soft fail) is not yet active in Tuwunel and depends on upstream
typing work. Until v13 ships, "leave it off" is safe; once v13 rooms become
common, leaving it off in such a room means accepting events that the rest
of the federation will reject.

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
  before the allowlist.
- `url_preview_check_root_domain`: when `true`, applies the contains and
  explicit allowlists against the root domain, so an allow on `wikipedia.org`
  also lets `en.m.wikipedia.org` through.
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
  vectors out of the box. Set to `[]` only if a firewall is enforcing the
  same constraints upstream.

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
