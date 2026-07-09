# Tuwunel 1.8.1

July 9, 2026

### New Features & Enhancements

- **Synapse-compatible admin API**. The Synapse admin surface is served: user, room, media, device and access-token endpoints, the version and event-fetch endpoints, and room deletion, purge, and background-task tracking, backed by an in-memory task tracker and documented on a coverage status page. Opened by @iwalkalone69 in (#38). The user endpoints include listing a user's joined rooms, opened by @ngophuocloi-miracle-aavn in (#494).

- **Threads list** (MSC3856). The `/threads` endpoint now orders threads by latest activity, honors an `include=participated` filter, serves per-requester views that respect ignored users, guards its inputs upfront, and carries the newest edit on each thread's `latest_event`.

- **Stable threading** (MSC3440). Threading is advertised in `/versions`, the `related_by_senders` and `related_by_rel_types` event filters are implemented, and nested thread relations are rejected at the send endpoint.

- **Sender erasure** (MSC4025). An erasure marker lands with admin surfacing, erased senders' events are served as pruned copies, and federation serving of those events is gated accordingly.

- **Native OIDC account registration and login**, so Tuwunel can act as its own identity provider. Requested by @temp1403-oss (#479).

- **Configurable default power-level override** for newly created rooms, courtesy of @basnijholt in (#496).

- User suspension is now enforced at the API boundary, contributed by @dasha-uwu.

- OAuth falls back to Apple `id_token` claims when the userinfo endpoint fails, shipped by @basnijholt in (#495).

- The `admin query raw` commands gain a `put` command and hex key decoding, from @dasha-uwu.

- A `SECURITY.md` with a detached PGP signature, along with issue and pull-request templates and contact links, graciously added by @x86pup.

- Event bundling advances across three proposals: aggregations bundled on search context events (MSC3666), `m.reference` children bundled as an event-id chunk (MSC3267), and the latest `m.replace` edit bundled as a full event (MSC3925).

- Private read receipts now carry a timestamp.

- Rust is bumped to 1.95.0.

### Bug Fixes

- Sliding sync silently dropped `m.space` rooms, so spaces were absent from the room list where Synapse showed them; the list filters a client omits are now cleared before applying (MSC4186, fixes #503). Reported by @sdenike.

- Rooms made space-visible did not appear in the space overview for new users. The room hierarchy cache is now evicted on any state change (fixes #498). Reported by @Lazalatin.

- Registration with OIDC and LDAP configured together was broken: LDAP users are now provisioned even when provider registration is disabled (fixes #499). Reported by @balintbarna.

- Tuwunel builds on FreeBSD again, with rust-rocksdb vendoring RocksDB there (fixes #492). Reported by @syobocat.

- The MatrixRTC/Livekit setup docs were missing the Docker address-advertisement configuration, now explained (fixes #493). Reported by @Wanja-L.

- Conduit database import gains several repairs: the `roomuserid_joined` repair runs in a single pass, the conduwuit-era membership repairs are skipped for Conduit imports, and systemd's start timeout is extended so long startup migrations are not killed (#41). Thanks to @x86pup.

- The remote-server version endpoint returns our own version for a self-query instead of failing, with appreciation to @x86pup.

- Unauthenticated TURN access was possible with `turn_allow_guests` enabled; guest access is now gated and appservice users are excluded from the guest TURN credentials check, credit to @dasha-uwu.

- Federation delivery is steadier: a stale resolver route is evicted on a non-JSON response (9bac54488), the sender flushes when an unhealthy peer shows inbound activity (2d9c6848d), a first-failure retry grace precedes the backoff curve (e7f5769dd), and the sender wakes to retry a failed destination (93a772e47).

- EDU delivery is more reliable: selected device-list and receipt EDUs persist until acknowledged (ffdfc1b41), EDU selections queue past the transaction budget (7091f84fc), and fresh EDUs are selected on the post-response path (356931737).

- `/messages` is forbidden on a room the requester cannot see (a791d7ed5).

- An unsupported method on a known path returns 405 instead of 404 (cd0513a0c).

- The request extractor separates an empty body from a malformed one (80727f81a).

- `is_direct` is omitted from member events unless it is true (adb78b98c).

- The registration email binds regardless of UIA stage order (9b58caede).

- The error log on undecodable presence data is restored (d81f34092).

- FIFO cache column TTLs are bounded to each column's validity window, with intra-L0 compaction enabled for those columns (9b3011aa5, e9004c909).

- Release builds could fail to compile the room-summary layout after a 1.8.0 change; boxing the membership format at the invite edge cuts the recursion (regression d2c473fd4).
