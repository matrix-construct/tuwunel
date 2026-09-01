#!/usr/bin/env bash
# repro-reread-noedu.sh — Verify that re-reading an already-read position
# re-announces the receipt instead of going silent.
#
# A client which missed the original receipt EDU derives its unread state
# from the latest receipt it has seen; re-reading the same position is its
# only way to ask the server for the receipt again. If the duplicate receipt
# produces no EDU, the client stays stuck in a quiet room forever.
#
# Device A reads at an event; device B (same user, own session) consumes
# that receipt, then re-reads the same event. On a fixed server B's next
# sync carries the receipt EDU again (exit 0). On a buggy server the
# duplicate receipt produces no sync traffic at all (exit 1).
#
# Usage:
#   ALICE_PASS=... BOB_PASS=... ./tests/repro-reread-noedu.sh
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
	curl "${args[@]}" "$BASE/_matrix/client$path"
}

login() { # login USER PASS [DEVICE] -> token
	local token
	token=$(api POST /v3/login '' "$(jq -nc --arg u "$1" --arg p "$2" --arg d "${3:-}" \
		'{type:"m.login.password", identifier:{type:"m.id.user",user:$u}, password:$p}
		 + (if $d == "" then {} else {device_id: $d} end)')" | jq -r '.access_token')
	[[ "$token" != "null" && -n "$token" ]] || { echo "login failed for $1" >&2; exit 1; }
	echo "$token"
}

sync() { # sync TOKEN [SINCE] -> sync response JSON (timeout=0)
	local url="/v3/sync?timeout=0"
	[[ -n "${2:-}" ]] && url+="&since=$2"
	api GET "$url" "$1"
}

sss() { # sss TOKEN [POS] BODY -> sliding sync response JSON
	local url="/unstable/org.matrix.simplified_msc3575/sync"
	[[ -n "${2:-}" ]] && url+="?pos=$2"
	api POST "$url" "$1" "$3"
}

echo "== Logging in $ALICE_USER (devices A and B) and $BOB_USER on $BASE"
A_TOKEN="$(login "$ALICE_USER" "$ALICE_PASS" REREADA)"
B_TOKEN="$(login "$ALICE_USER" "$ALICE_PASS" REREADB)"
BOB_TOKEN="$(login "$BOB_USER" "$BOB_PASS")"
ALICE_ID="$(api GET /v3/account/whoami "$A_TOKEN" | jq -r '.user_id')"

room=$(api POST /v3/createRoom "$BOB_TOKEN" \
	"$(jq -nc --arg a "$ALICE_ID" '{preset:"private_chat", invite:[$a]}')" | jq -r '.room_id')
[[ "$room" != "null" && -n "$room" ]] || { echo "createRoom failed" >&2; exit 1; }
api POST "/v3/rooms/$room/join" "$A_TOKEN" >/dev/null
echo "room: $room"

msg=$(api PUT "/v3/rooms/$room/send/m.room.message/cli$(date +%s%N)$RANDOM" "$BOB_TOKEN" \
	'{"msgtype":"m.text","body":"re-read repro"}' | jq -r '.event_id')
[[ "$msg" != "null" && -n "$msg" ]] || { echo "send failed" >&2; exit 1; }
echo "message: $msg"

# B learns the message.
s0=$(sync "$B_TOKEN" | jq -r '.next_batch')
s0=$(sync "$B_TOKEN" "$s0" | jq -r '.next_batch')

# A reads at the message; B consumes the original receipt EDU.
api POST "/v3/rooms/$room/read_markers" "$A_TOKEN" \
	"$(jq -nc --arg e "$msg" '{"m.read":$e}')" >/dev/null
resp=$(sync "$B_TOKEN" "$s0")
s1=$(jq -r '.next_batch' <<<"$resp")
echo "original receipt EDU delivered to B: $(jq --arg r "$room" \
	'[.rooms.join[$r].ephemeral.events[]? | select(.type=="m.receipt")] | length' <<<"$resp")"

# B re-reads the same position: identical to the stored receipt.
api POST "/v3/rooms/$room/read_markers" "$B_TOKEN" \
	"$(jq -nc --arg e "$msg" '{"m.read":$e}')" >/dev/null

# v3: the duplicate receipt must re-announce as a fresh EDU.
resp=$(sync "$B_TOKEN" "$s1")
v3_edus=$(jq --arg r "$room" \
	'[.rooms.join[$r].ephemeral.events[]? | select(.type=="m.receipt")] | length' <<<"$resp")
echo "v3 receipt EDUs after duplicate re-read: $v3_edus"

# MSC4186: same assertion on the receipts extension, the path sliding-sync
# clients (matrix-rust-sdk) consume.
p0=$(sss "$B_TOKEN" "" "$(jq -nc --arg r "$room" '{
	lists: {main: {ranges: [[0,19]], required_state: [], timeline_limit: 1}},
	room_subscriptions: {($r): {required_state: [], timeline_limit: 1}},
	extensions: {receipts: {enabled: true}}
}')" | jq -r '.pos')

api POST "/v3/rooms/$room/read_markers" "$B_TOKEN" \
	"$(jq -nc --arg e "$msg" '{"m.read":$e}')" >/dev/null

resp=$(sss "$B_TOKEN" "$p0" "$(jq -nc --arg r "$room" '{
	lists: {main: {ranges: [[0,19]], required_state: [], timeline_limit: 1}},
	room_subscriptions: {($r): {required_state: [], timeline_limit: 1}},
	extensions: {receipts: {enabled: true}}
}')")
v5_edus=$(jq --arg r "$room" '.extensions.receipts.rooms[$r] | length' <<<"$resp")
echo "v5 receipts-extension entries after duplicate re-read: $v5_edus"

if [[ "$v3_edus" -ge 1 && "$v5_edus" -ge 1 ]]; then
	echo "OK: duplicate re-reads re-announce the receipt; stuck clients can self-heal."
	exit 0
fi

echo "BUG: duplicate re-read produced no receipt EDU (v3=$v3_edus v5=$v5_edus);" >&2
echo "a client which missed the original receipt stays unread forever." >&2
exit 1
