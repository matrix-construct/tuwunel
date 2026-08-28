#!/usr/bin/env bash
# repro-fully-read-regression.sh — Checks whether the server lets a stale
# device move the m.fully_read marker BACKWARDS.
#
# Unfixed server: a read_markers/receipt request naming an older event moves
# the marker back (exit 0). Fixed server: the backwards write is ignored and
# the marker stays (exit 1).
#
# Usage:
#   ALICE_PASS=... BOB_PASS=... ./tests/repro-fully-read-regression.sh
#   Optional env: BASE_URL (default https://matrix.agiadn.org),
#                 ALICE_USER (default kimi, the reader),
#                 BOB_USER   (default imik, the sender)
set -euo pipefail

BASE="${BASE_URL:-https://matrix.agiadn.org}"
ALICE_USER="${ALICE_USER:-kimi}"
ALICE_PASS="${ALICE_PASS:?set ALICE_PASS}"
BOB_USER="${BOB_USER:-imik}"
BOB_PASS="${BOB_PASS:?set BOB_PASS}"

api() { # api METHOD PATH [TOKEN] [JSON]
	local method="$1" path="$2" token="${3:-}" body="${4:-}"
	local args=(-sS -X "$method" -H 'Content-Type: application/json')
	[[ -n "$token" ]] && args+=(-H "Authorization: Bearer $token")
	[[ -n "$body" ]] && args+=(-d "$body")
	curl "${args[@]}" "$BASE/_matrix/client/v3$path"
}

login() { # login USER PASS -> "token user_id"
	local resp
	resp=$(api POST /login '' "$(jq -nc --arg u "$1" --arg p "$2" \
		'{type:"m.login.password", identifier:{type:"m.id.user",user:$u}, password:$p}')")
	jq -e -r '.access_token' <<<"$resp" >/dev/null
	echo "$(jq -r '.access_token' <<<"$resp") $(jq -r '.user_id' <<<"$resp")"
}

read -r ALICE_TOKEN ALICE_ID <<<"$(login "$ALICE_USER" "$ALICE_PASS")"
read -r BOB_TOKEN BOB_ID <<<"$(login "$BOB_USER" "$BOB_PASS")"

ROOM=$(api POST /createRoom "$BOB_TOKEN" \
	"$(jq -nc --arg a "$ALICE_ID" '{preset:"private_chat", invite:[$a]}')" | jq -r '.room_id')
api POST "/rooms/$ROOM/join" "$ALICE_TOKEN" >/dev/null
echo "room: $ROOM"

send() { # send TOKEN BODY -> event_id
	api PUT "/rooms/$ROOM/send/m.room.message/cli$(date +%s%N)$RANDOM" "$1" \
		"$(jq -nc --arg b "$2" '{msgtype:"m.text", body:$b}')" | jq -r '.event_id'
}

fully_read() { # -> stored m.fully_read event id
	api GET "/user/$ALICE_ID/rooms/$ROOM/account_data/m.fully_read" "$ALICE_TOKEN" |
		jq -r '.event_id // empty'
}

mark() { # mark EVENT — post read_markers like FluffyChat does
	api POST "/rooms/$ROOM/read_markers" "$ALICE_TOKEN" \
		"$(jq -nc --arg e "$1" '{"m.fully_read":$e, "m.read":$e, "m.read.private":$e}')" >/dev/null
}

FIRST=$(send "$BOB_TOKEN" "first")
SECOND=$(send "$BOB_TOKEN" "second")

# Forward write: marker must land on the newer event.
mark "$SECOND"
STORED=$(fully_read)
echo "after forward write:  $STORED"
[[ "$STORED" == "$SECOND" ]] || { echo "FAIL: forward write did not store" >&2; exit 1; }

# Backwards write, as a stale device would send it.
mark "$FIRST"
STORED=$(fully_read)
echo "after backward write: $STORED"

# Backwards write via the /receipt endpoint too.
api POST "/rooms/$ROOM/receipt/m.fully_read/$FIRST" "$ALICE_TOKEN" '{}' >/dev/null
STORED=$(fully_read)
echo "after /receipt write: $STORED"

if [[ "$STORED" == "$FIRST" ]]; then
	echo
	echo "*** BUG REPRODUCED: m.fully_read moved backwards. ***"
	exit 0
elif [[ "$STORED" == "$SECOND" ]]; then
	echo
	echo "GUARD PRESENT: m.fully_read stayed at the newer event."
	exit 1
else
	echo "UNEXPECTED: marker is at neither event: $STORED" >&2
	exit 2
fi
