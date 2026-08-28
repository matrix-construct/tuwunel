#!/usr/bin/env bash
# repro-unread-receipt.sh — Deterministic reproduction of "room stays unread
# after reading everything" between Tuwunel (/sync v3) and FluffyChat
# (matrix-dart-sdk).
#
# A read receipt that shares its /sync window with a non-notifying timeline
# event (here: a reaction) caused the server to omit the zeroed
# unread_notifications; clients mirroring the counts then kept a stale unread
# badge with no way to clear it. All syncs use timeout=0 with explicit `since`
# tokens, so the receipt and the reaction are forced into the same sync window
# by construction.
#
# Usage:
#   ALICE_PASS=... BOB_PASS=... ./tests/repro-unread-receipt.sh
#   Optional env: BASE_URL (default https://matrix.agiadn.org),
#                 ALICE_USER (default kimi, the reader),
#                 BOB_USER   (default imik, the sender),
#                 OUT_DIR    (default /tmp/repro-unread)
#
# Exit code: 0 if the bug was reproduced (and the control case behaved
# correctly), 1 otherwise. On a fixed server the bug case ends with
# client_count=0 and the script exits 1.

set -euo pipefail

BASE="${BASE_URL:-https://matrix.agiadn.org}"
ALICE_USER="${ALICE_USER:-kimi}"   # reader (simulates the FluffyChat user)
ALICE_PASS="${ALICE_PASS:?set ALICE_PASS}"
BOB_USER="${BOB_USER:-imik}"       # sender
BOB_PASS="${BOB_PASS:?set BOB_PASS}"
OUT="${OUT_DIR:-/tmp/repro-unread}"
mkdir -p "$OUT"

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

sync() { # sync TOKEN [SINCE] -> sync response JSON (timeout=0)
	local tok="$1" since="${2:-}"
	local url="/sync?timeout=0"
	[[ -n "$since" ]] && url+="&since=$since"
	api GET "$url" "$tok"
}

# Client-side count simulation, mirroring matrix-dart-sdk semantics:
# the stored count is only updated when the `unread_notifications` key is
# present in the room's sync update; otherwise the stale value is kept.
CLIENT_COUNT=0
update_client_count() { # update_client_count ROOM SYNC_JSON_FILE
	local v
	v=$(jq -r --arg r "$1" \
		'if .rooms.join[$r].unread_notifications != null
		 then (.rooms.join[$r].unread_notifications.notification_count // 0)
		 else empty end' "$2")
	if [[ -n "$v" ]]; then CLIENT_COUNT="$v"; fi
	return 0
}

send_text() { # send_text ROOM TOKEN BODY -> event_id
	api PUT "/rooms/$1/send/m.room.message/cli$(date +%s%N)$RANDOM" "$2" \
		"$(jq -nc --arg b "$3" '{msgtype:"m.text", body:$b}')" | jq -r '.event_id'
}

# ---------------------------------------------------------------------------
echo "== Logging in $ALICE_USER (reader) and $BOB_USER (sender) on $BASE"
read -r ALICE_TOKEN ALICE_ID <<<"$(login "$ALICE_USER" "$ALICE_PASS")"
read -r BOB_TOKEN BOB_ID <<<"$(login "$BOB_USER" "$BOB_PASS")"
echo "   reader: $ALICE_ID"
echo "   sender: $BOB_ID"

# ---------------------------------------------------------------------------
# run_case NAME TRIGGER   TRIGGER ∈ none | reaction | redaction
# Runs the full read-a-room flow. Prints the final simulated client count.
# ---------------------------------------------------------------------------
run_case() {
	local name="$1" trigger="$2"
	echo
	echo "=== Case: $name (trigger: $trigger) ==="

	local room msg_ev
	room=$(api POST /createRoom "$BOB_TOKEN" \
		"$(jq -nc --arg a "$ALICE_ID" '{preset:"private_chat", invite:[$a]}')" |
		jq -r '.room_id')
	echo "room: $room"
	api POST "/rooms/$room/join" "$ALICE_TOKEN" >/dev/null

	# Baseline sync for Alice.
	local s0
	s0=$(sync "$ALICE_TOKEN" | jq -r '.next_batch')

	# Bob sends a notifying message.
	msg_ev=$(send_text "$room" "$BOB_TOKEN" "hello from $name")
	echo "message: $msg_ev"

	# Alice syncs: must observe notification_count >= 1.
	local s1 resp
	resp="$OUT/$name.resp1.json"
	sync "$ALICE_TOKEN" "$s0" >"$resp"
	local count1
	count1=$(jq -r --arg r "$room" \
		'.rooms.join[$r].unread_notifications.notification_count // 0' "$resp")
	s1=$(jq -r '.next_batch' "$resp")
	echo "after message: server notification_count=$count1"
	if [[ "$count1" -lt 1 ]]; then
		echo "FAIL: expected notification_count >= 1 after message" >&2
		return 1
	fi
	CLIENT_COUNT="$count1"

	# Alice reads the room exactly like FluffyChat does:
	# POST /read_markers with m.fully_read + m.read + m.read.private (unthreaded).
	api POST "/rooms/$room/read_markers" "$ALICE_TOKEN" \
		"$(jq -nc --arg e "$msg_ev" \
			'{"m.fully_read":$e, "m.read":$e, "m.read.private":$e}')" >/dev/null

	# The trigger: a NON-notifying timeline event lands in the same sync
	# window as the receipt.
	case "$trigger" in
		reaction)
			api PUT "/rooms/$room/send/m.reaction/cli$(date +%s%N)$RANDOM" "$BOB_TOKEN" \
				"$(jq -nc --arg e "$msg_ev" \
					'{"m.relates_to":{rel_type:"m.annotation",event_id:$e,key:"👍"}}')" >/dev/null
			;;
		redaction)
			api POST "/rooms/$room/redact/$msg_ev/cli$(date +%s%N)$RANDOM" "$BOB_TOKEN" '{}' >/dev/null
			;;
		none) ;;
	esac

	# Alice syncs: this window contains the receipt EDU (+ the trigger event).
	resp="$OUT/$name.resp2.json"
	sync "$ALICE_TOKEN" "$s1" >"$resp"
	local has_room has_un count2
	has_room=$(jq -r --arg r "$room" '(.rooms.join // {}) | has($r)' "$resp")
	has_un=$(jq -r --arg r "$room" \
		'if .rooms.join[$r] == null then "n/a"
		 else (.rooms.join[$r] | has("unread_notifications")) end' "$resp")
	count2=$(jq -r --arg r "$room" \
		'.rooms.join[$r].unread_notifications.notification_count // "absent"' "$resp")
	update_client_count "$room" "$resp"
	echo "sync with receipt+trigger: room_in_sync=$has_room" \
		"unread_notifications_present=$has_un notification_count=$count2" \
		"=> client_count=$CLIENT_COUNT"
	local s2
	s2=$(jq -r '.next_batch' "$resp")

	# Alice syncs again: normally the room is omitted entirely, so the stale
	# count can never be corrected.
	resp="$OUT/$name.resp3.json"
	sync "$ALICE_TOKEN" "$s2" >"$resp"
	has_room=$(jq -r --arg r "$room" '(.rooms.join // {}) | has($r)' "$resp")
	update_client_count "$room" "$resp"
	echo "next sync: room_in_sync=$has_room => client_count=$CLIENT_COUNT"
	local s3
	s3=$(jq -r '.next_batch' "$resp")

	# Alice re-reads the same position (FluffyChat re-opens the room):
	# monotonic receipts mean no new stamp/EDU is produced.
	api POST "/rooms/$room/read_markers" "$ALICE_TOKEN" \
		"$(jq -nc --arg e "$msg_ev" \
			'{"m.fully_read":$e, "m.read":$e, "m.read.private":$e}')" >/dev/null
	resp="$OUT/$name.resp4.json"
	sync "$ALICE_TOKEN" "$s3" >"$resp"
	has_room=$(jq -r --arg r "$room" '(.rooms.join // {}) | has($r)' "$resp")
	update_client_count "$room" "$resp"
	echo "re-read + sync: room_in_sync=$has_room => client_count=$CLIENT_COUNT"

	echo "FINAL simulated client notification_count: $CLIENT_COUNT"
}

# ---------------------------------------------------------------------------
# Control: read WITHOUT any extra timeline event in the window. The zeroed
# counts MUST be delivered — this proves the trigger below is the cause.
run_case control none
if [[ "$CLIENT_COUNT" != "0" ]]; then
	echo
	echo "UNEXPECTED: control case did not clear the count; hypothesis wrong?" >&2
	exit 1
fi
echo "CONTROL OK: reading a quiet room clears the unread count."

# Bug case: receipt shares its sync window with a reaction.
run_case bug_reaction reaction
if [[ "$CLIENT_COUNT" != "0" ]]; then
	echo
	echo "*** BUG REPRODUCED: room is stuck unread (client count=$CLIENT_COUNT)" \
		"even though every message was read. ***"
	echo "*** FluffyChat would show this room as unread forever. ***"
else
	echo
	echo "Bug NOT reproduced with reaction trigger; server delivered zero counts."
	exit 1
fi

# Variant: same with a redaction instead of a reaction.
run_case bug_redaction redaction
if [[ "$CLIENT_COUNT" != "0" ]]; then
	echo
	echo "*** BUG ALSO REPRODUCED with redaction trigger (count=$CLIENT_COUNT). ***"
else
	echo "Redaction variant did not reproduce."
fi

echo
echo "Raw sync responses saved in $OUT"
