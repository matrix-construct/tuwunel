#!/usr/bin/env bash
# probe-read-state.sh — Diagnose a stuck read marker for a given user+room.
# Run with YOUR OWN credentials:
#   RU_USER=ruka RU_PASS=... ./tests/probe-read-state.sh '!room1:server' '!room2:server'
#
# Per room it dumps the three signals a client derives its unread state from
# (notification counts, own receipts, m.fully_read), then posts /read_markers
# at the latest message (exactly what FluffyChat sends) and dumps them again.
set -euo pipefail

BASE="${BASE_URL:-https://matrix.agiadn.org}"
USER="${RU_USER:?set RU_USER}"
PASS="${RU_PASS:?set RU_PASS}"

LOGIN=$(curl -sS -X POST -H 'Content-Type: application/json' \
	-d "$(jq -nc --arg u "$USER" --arg p "$PASS" \
		'{type:"m.login.password", identifier:{type:"m.id.user",user:$u}, password:$p}')" \
	"$BASE/_matrix/client/v3/login")
TOKEN=$(jq -r '.access_token' <<<"$LOGIN")
MXID=$(jq -r '.user_id' <<<"$LOGIN")
[[ "$TOKEN" != "null" && -n "$TOKEN" ]] || { echo "login failed" >&2; exit 1; }

# dump_state ROOM — print every server-side signal a client derives unread
# state from: stored m.fully_read, unread notification counts, the user's own
# latest receipts as served in /sync, and the room's actual latest event.
dump_state() { # dump_state ROOM
	local room="$1"

	local fr
	fr=$(curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/user/$MXID/rooms/$room/account_data/m.fully_read")
	echo "m.fully_read: $fr"

	local mu
	mu=$(curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/user/$MXID/rooms/$room/account_data/m.marked_unread")
	echo "m.marked_unread: $mu"
	mu=$(curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/user/$MXID/rooms/$room/account_data/com.famedly.marked_unread")
	echo "com.famedly.marked_unread: $mu"

	local sync
	sync=$(curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/sync?timeout=0")

	jq -r --arg r "$room" --arg u "$MXID" '
		.rooms.join[$r] as $j |
		"unread_notifications: \($j.unread_notifications // "absent")",
		([$j.ephemeral.events[]? | select(.type=="m.receipt") | .content
			| to_entries[] | .key as $ev | .value
			| ((.["m.read"][$u].ts // null) as $r2 |
				(.["m.read.private"][$u].ts // null) as $p |
				select($r2 != null or $p != null) |
				"own receipt: \($ev) m.read.ts=\($r2) m.read.private.ts=\($p)")]
			| if length == 0 then "own receipt: NONE in sync" else .[] end)
	' <<<"$sync"

	curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/rooms/$room/messages?dir=b&limit=3" |
		jq -r '.chunk[] | "latest events: \(.event_id) \(.type) \(.sender) ts=\(.origin_server_ts)"'
}

for ROOM in "$@"; do
	echo
	echo "=== $ROOM (before) ==="
	dump_state "$ROOM"

	LATEST=$(curl -sS -H "Authorization: Bearer $TOKEN" \
		"$BASE/_matrix/client/v3/rooms/$ROOM/messages?dir=b&limit=10" |
		jq -r '[.chunk[] | select(.type=="m.room.message")][0].event_id // empty')
	[[ -n "$LATEST" ]] || continue

	echo
	echo "POST /read_markers at $LATEST ->"
	curl -sS -w '\nHTTP %{http_code}\n' -X POST -H "Authorization: Bearer $TOKEN" \
		-H 'Content-Type: application/json' \
		-d "$(jq -nc --arg e "$LATEST" \
			'{"m.fully_read":$e, "m.read":$e, "m.read.private":$e}')" \
		"$BASE/_matrix/client/v3/rooms/$ROOM/read_markers"

	echo
	echo "=== $ROOM (after) ==="
	dump_state "$ROOM"
done
