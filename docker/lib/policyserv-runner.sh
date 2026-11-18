#!/bin/bash
set -eo pipefail

# Policy server integration test (MSC4284).
#
# Stands up policyserv, its postgres and a TLS front beside tuwunel on a private
# network, has policyserv join a tuwunel room, names it the room's policy
# server, and then checks that tuwunel's sends follow its answers: a benign
# message is signed, a spammy one is refused, and nothing goes out while the
# policy server is unreachable (except a leave, which needs no verdict).
#
# Both servers federate over TLS with certificates from a throwaway CA, which
# each trusts through SSL_CERT_FILE, so certificates are verified in both
# directions. Nothing answers on port 443, so each server's discovery of the
# other misses the well-known and SRV lookups and falls back to port 8448. The
# network is internal: no request leaves it, and those failed lookups fail fast.

# Pinned images (explicit versions, never floating tags).
POLICYSERV_IMAGE="${POLICYSERV_IMAGE:-ghcr.io/matrix-org/policyserv:v1.8.1}"
PG_IMAGE="${PG_IMAGE:-postgres:16}"
NGINX_IMAGE="${NGINX_IMAGE:-nginx:1.31.8-alpine}"
OPENSSL_IMAGE="${OPENSSL_IMAGE:-alpine/openssl:3.5.8}"
CURL_IMAGE="${CURL_IMAGE:-curlimages/curl:8.11.1}"

# Static test values. The token MUST equal the registration_token baked into
# docker/Dockerfile.policyserv.
REGISTRATION_TOKEN="tuwunel-policyserv-registration-token"
API_KEY="tuwunel-policyserv-admin-key"
ALICE="alice"
ALICE_PW="policyserv-test-password-4b7e2d"
POLICYSERV_USER="@policyserv:policyserv"

# What policyserv answers when it refuses to sign, which tuwunel relays.
REFUSAL_MESSAGE="This message is not allowed by the policy server"

# policyserv insists on a notary for server keys. It fetches keys from their
# origin first, so this placeholder is never actually asked for one.
KEY_QUERY_SERVER="tuwunel,ed25519:unused,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

# Matrix cell -> testee image name. The defaults mirror docker/policyserv.sh, so
# the name matches what `docker buildx bake policyserv-testee` produced.
cargo_profile="${cargo_profile:-test}"
feat_set="${feat_set:-all}"
rust_toolchain="${rust_toolchain:-nightly}"
rust_target="${rust_target:-x86_64-unknown-linux-gnu}"
sys_name="${sys_name:-debian}"
sys_target="${sys_target:-x86_64-v1-linux-gnu}"
sys_version="${sys_version:-testing-slim}"

cell="${cargo_profile}--${rust_toolchain}--${rust_target}--${feat_set}--${sys_name}--${sys_version}--${sys_target}"
testee_image="${POLICYSERV_TESTEE_IMAGE:-policyserv-testee--${cell}}"

results_dir="${results_dir:-tests/policyserv}"
results="$results_dir/results.jsonl"

# The check under way. `check` sets these and `pass` clears them again, so if
# the run exits while they are set, that check is the one that failed.
check_id=
check_name=

# Run-id: unique per runner registration, stable across a re-run on the same
# registration, so the prior attempt's resources self-reclaim.
run_seed="${RUNNER_NAME:-local-$$}"
run_id=$(printf '%s' "${run_seed}-policyserv" | sha1sum | cut -c1-12)
prefix="${POLICYSERV_NAME_PREFIX:-policyserv}"

net="${prefix}-net-${run_id}"            # the private network
fixtures="${prefix}-fixtures-${run_id}"  # volume: CA, leaf certificates, front config
keys="${prefix}-keys-${run_id}"          # volume: policyserv's signing keys
pg="${prefix}-pg-${run_id}"              # postgres, for policyserv
ps="${prefix}-app-${run_id}"             # policyserv itself, plain http
front="${prefix}-front-${run_id}"        # nginx, terminating TLS for policyserv
hs="${prefix}-tuwunel-${run_id}"         # tuwunel, the system under test

# Addresses as seen from inside the network.
hs_client="http://tuwunel:8008"
admin="http://policyserv-app:8080/api/v1"

mkdir -p "$results_dir"
: >"$results"

###############################################################################
# Helpers
###############################################################################

log()  { printf '\033[1;36m[policyserv]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;41;37mERROR\033[0m %s\n' "$*" >&2; exit 1; }

# check ID NAME
#
# Begins a check. The checks run in order and each builds on the ones before
# it, so the first failure ends the run, and on_exit records the check that was
# under way as failed. The checks after it write no row at all; the summary
# fills those in as skipped, from the committed baseline.
check() {
	check_id=$1
	check_name=$2
}

# record OUTCOME
#
# Appends a row for the current check to the results; OUTCOME is pass or fail.
record() {
	local row

	row=$(printf '{"check":"%s","name":"%s","outcome":"%s"}' \
		"$check_id" "$check_name" "$1")

	printf '%s\n' "$row" >>"$results"
}

# pass
#
# Ends the current check as passed.
pass() {
	record pass
	log "check $check_id passed: $check_name"

	check_id=
}

capture_logs() {
	docker logs "$hs"    >"$results_dir/tuwunel.log"    2>&1 || true
	docker logs "$ps"    >"$results_dir/policyserv.log" 2>&1 || true
	docker logs "$front" >"$results_dir/front.log"      2>&1 || true
}

# Containers go with their anonymous volumes (-v): postgres declares one for its
# data, which would otherwise outlive every run.
teardown() {
	docker rm -f -v "$hs" "$front" "$ps" "$pg" >/dev/null 2>&1 || true
	docker network rm "$net"                >/dev/null 2>&1 || true
	docker volume rm "$fixtures" "$keys"    >/dev/null 2>&1 || true
}

on_exit() {
	# A check still under way when the run exits is the one that failed.
	if [ -n "$check_id" ]; then
		record fail
	fi

	capture_logs
	teardown
	exit "$1"
}

trap 'on_exit $?' EXIT
trap 'fail "interrupted"' INT

# request METHOD URL [TOKEN] [BODY]
#
# Makes one request from a throwaway curl container on the network and prints
# the response body, then the status code on a line of its own. The CA is the
# run's, so every https request verifies the server's certificate.
request() {
	request_within 90 "$@"
}

# request_within SECONDS METHOD URL [TOKEN] [BODY]
#
# As request, giving up on the whole exchange after SECONDS.
request_within() {
	local limit=$1
	shift

	local args=(-s -X "$1" -w '\n%{http_code}' --max-time "$limit" --cacert /pki/ca.crt)

	if [ -n "$3" ]; then
		args+=(-H "Authorization: Bearer $3")
	fi

	if [ -n "$4" ]; then
		args+=(-H 'Content-Type: application/json' -d "$4")
	fi

	docker run --rm --network "$net" -v "$fixtures:/pki:ro" "$CURL_IMAGE" "${args[@]}" "$2" \
		|| printf '\n000\n'
}

status_of() { printf '%s\n' "$1" | tail -n 1; }
body_of()   { printf '%s\n' "$1" | sed '$d'; }

# field NAME < json
#
# Prints the first string value of a field, top-level or nested. It is no JSON
# parser, but it is enough for the flat answers this test reads.
field() {
	sed -n "s/.*\"$1\" *: *\"\([^\"]*\)\".*/\1/p" | head -n 1
}

# expect_status WANT RESPONSE WHAT
expect_status() {
	local got

	got=$(status_of "$2")
	if [ "$got" != "$1" ]; then
		fail "$3: expected $1, got $got: $(body_of "$2")"
	fi
}

# wait_for WHAT URL
#
# Polls a URL until it answers 200, for a minute at most. Each poll gets five
# seconds, so a server which takes the connection and never answers cannot
# hold the wait past its minute.
wait_for() {
	local deadline=$((SECONDS + 60))

	until [ "$(status_of "$(request_within 5 GET "$2")")" = 200 ]; do
		if [ "$SECONDS" -ge "$deadline" ]; then
			fail "$1 not ready"
		fi

		sleep 1
	done
}

# send_text TXN BODY
#
# Alice sends a text message into the room.
send_text() {
	local url="$hs_client/_matrix/client/v3/rooms/$room/send/m.room.message/$1"
	local content

	content=$(printf '{"msgtype":"m.text","body":"%s"}' "$2")
	request PUT "$url" "$token" "$content"
}

###############################################################################
# Setup
###############################################################################

# Pre-reclaim any prior attempt on this registration.
teardown

# Pull pinned externals (best-effort; a cache hit on re-run).
for image in "$POLICYSERV_IMAGE" "$PG_IMAGE" "$NGINX_IMAGE" "$OPENSSL_IMAGE" "$CURL_IMAGE"; do
	docker pull -q "$image" >/dev/null || true
done

docker network create --internal "$net" >/dev/null
docker volume create "$fixtures"        >/dev/null
docker volume create "$keys"            >/dev/null

# A throwaway CA, a leaf for each server name, and the TLS front's config. The
# keys are world-readable so that any container user can load them.
log "generating the run's certificates"
docker run --rm -i --network none -v "$fixtures:/pki" --entrypoint sh "$OPENSSL_IMAGE" -s <<-'PKI'
	set -e
	cd /pki

	openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
		-keyout ca.key -out ca.crt -days 2 -subj /CN=policyserv-test-ca \
		-addext basicConstraints=critical,CA:TRUE \
		-addext keyUsage=critical,keyCertSign,cRLSign

	for name in tuwunel policyserv; do
		openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
			-keyout "$name.key" -out "$name.csr" -subj "/CN=$name"

		cat >"$name.ext" <<-EXT
			subjectAltName=DNS:$name
			basicConstraints=CA:FALSE
			keyUsage=critical,digitalSignature
			extendedKeyUsage=serverAuth,clientAuth
		EXT

		openssl x509 -req -in "$name.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
			-days 2 -extfile "$name.ext" -out "$name.crt"
	done

	chmod 0644 ./*.key

	cat >nginx.conf <<-'NGINX'
		events {}

		http {
			access_log /dev/stdout;
			error_log  /dev/stderr info;

			server {
				listen 8448 ssl;

				ssl_certificate     /pki/policyserv.crt;
				ssl_certificate_key /pki/policyserv.key;

				# No URI part: policyserv checks X-Matrix over the URI as sent.
				location / {
					proxy_pass http://policyserv-app:8080;
				}
			}
		}
	NGINX
PKI

# tuwunel first, since policyserv joins the room through it.
log "starting tuwunel ($testee_image)"
docker run -d --name "$hs" --network "$net" --network-alias tuwunel \
	-v "$fixtures:/pki:ro" \
	-e SSL_CERT_FILE=/pki/ca.crt \
	-e HTTPS_PROXY= -e https_proxy= -e HTTP_PROXY= -e http_proxy= \
	"$testee_image" >/dev/null

wait_for tuwunel "$hs_client/_matrix/client/versions"

log "starting postgres"
docker run -d --name "$pg" --network "$net" --network-alias postgres \
	-e POSTGRES_USER=policyserv \
	-e POSTGRES_PASSWORD=policyserv \
	-e POSTGRES_DB=policyserv \
	"$PG_IMAGE" >/dev/null

# Ask over TCP: while the image initializes the database it runs a temporary
# server on the Unix socket alone, which would answer too early.
for i in $(seq 1 30); do
	docker exec "$pg" pg_isready -h 127.0.0.1 -U policyserv -d policyserv >/dev/null 2>&1 && break
	[ "$i" = 30 ] && fail "postgres not ready"
	sleep 1
done

# policyserv: its signing keys first, then the app on plain http behind the
# front. Every default that would reach matrix.org points at tuwunel instead,
# and any proxy the docker daemon injects is blanked.
log "starting policyserv ($POLICYSERV_IMAGE)"
docker run --rm --network none -v "$keys:/data" "$POLICYSERV_IMAGE" gen_signing_keys >/dev/null

docker run -d --name "$ps" --network "$net" --network-alias policyserv-app \
	-v "$keys:/data" \
	-v "$fixtures:/pki:ro" \
	-e SSL_CERT_FILE=/pki/ca.crt \
	-e HTTPS_PROXY= -e https_proxy= -e HTTP_PROXY= -e http_proxy= \
	-e PS_DATABASE="postgres://policyserv:policyserv@postgres/policyserv?sslmode=disable" \
	-e PS_HOMESERVER_NAME=policyserv \
	-e PS_API_KEY="$API_KEY" \
	-e PS_JOIN_SERVER=tuwunel \
	-e PS_DIRECT_KEY_FETCHING=true \
	-e PS_KEY_QUERY_SERVER="$KEY_QUERY_SERVER" \
	-e PS_TRUSTED_ORIGINS=tuwunel \
	-e PS_EVENT_FETCH_SERVERS=tuwunel \
	-e PS_HOMESERVER_DENIED_NETWORKS= \
	-e PS_HOMESERVER_MEDIA_CLIENT_URL=https://tuwunel:8448 \
	-e PS_HTTP_PPROF_BIND= \
	-e PS_KEYWORD_FILTER_KEYWORDS=spam \
	-e PS_HELLBAN_POSTFILTER_MINUTES=0 \
	"$POLICYSERV_IMAGE" >/dev/null

wait_for policyserv "http://policyserv-app:8080/ready"

log "starting the TLS front ($NGINX_IMAGE)"
docker run -d --name "$front" --network "$net" --network-alias policyserv \
	-v "$fixtures:/pki:ro" \
	"$NGINX_IMAGE" nginx -c /pki/nginx.conf -g 'daemon off;' >/dev/null

wait_for "the TLS front" "https://policyserv:8448/_matrix/federation/v1/version"

###############################################################################
# Checks
###############################################################################

# A: both servers answer on their federation ports, over verified TLS.
check A "both servers answer over verified TLS"

reply=$(request GET "https://tuwunel:8448/_matrix/federation/v1/version")
expect_status 200 "$reply" "tuwunel federation version"

reply=$(request GET "https://policyserv:8448/_matrix/federation/v1/version")
expect_status 200 "$reply" "policyserv federation version through the front"

pass

# B: Alice registers and opens a public room, and policyserv joins it the way a
# homeserver would, with make_join and send_join against tuwunel.
check B "policyserv joined the room"

credentials=$(printf '"username":"%s","password":"%s"' "$ALICE" "$ALICE_PW")

reply=$(request POST "$hs_client/_matrix/client/v3/register" "" "{$credentials}")
session=$(body_of "$reply" | field session)
[ -n "$session" ] || fail "registration offered no session: $(body_of "$reply")"

auth=$(printf '"auth":{"type":"m.login.registration_token","token":"%s","session":"%s"}' \
	"$REGISTRATION_TOKEN" "$session")

reply=$(request POST "$hs_client/_matrix/client/v3/register" "" "{$credentials,$auth}")
expect_status 200 "$reply" "register $ALICE"
token=$(body_of "$reply" | field access_token)

reply=$(request POST "$hs_client/_matrix/client/v3/createRoom" "$token" '{"preset":"public_chat"}')
expect_status 200 "$reply" "createRoom"
room=$(body_of "$reply" | field room_id)

reply=$(request POST "$admin/communities/new" "$API_KEY" '{"name":"tuwunel-policyserv"}')
expect_status 200 "$reply" "policyserv community"
community=$(body_of "$reply" | field community_id)

reply=$(request POST "$admin/rooms/$room/join" "$API_KEY" "{\"community_id\":\"$community\"}")
expect_status 200 "$reply" "policyserv join"

# Give the join a minute, polling as wait_for does.
joined=0
deadline=$((SECONDS + 60))
until [ "$SECONDS" -ge "$deadline" ]; do
	reply=$(request_within 5 GET "$hs_client/_matrix/client/v3/rooms/$room/joined_members" "$token")

	case "$(body_of "$reply")" in
		*"\"$POLICYSERV_USER\""*)
			joined=1
			break
			;;
	esac

	sleep 1
done

[ "$joined" = 1 ] || fail "$POLICYSERV_USER did not join $room: $(body_of "$reply")"
pass

# C: the room names policyserv its policy server, under the key it publishes.
check C "the room names policyserv"

reply=$(request GET "https://policyserv:8448/.well-known/matrix/policy_server")
expect_status 200 "$reply" "policyserv policy key"

policy_key=$(body_of "$reply" | field ed25519)
[ -n "$policy_key" ] || fail "no ed25519 policy key published: $(body_of "$reply")"

policy=$(printf '{"via":"policyserv","public_keys":{"ed25519":"%s"}}' "$policy_key")
reply=$(request PUT "$hs_client/_matrix/client/v3/rooms/$room/state/m.room.policy/" "$token" "$policy")
expect_status 200 "$reply" "m.room.policy"

pass

# D: a benign message is sent. With policy servers on, tuwunel sends nothing
# policyserv has not signed under the room's key; the stored signature itself
# is checked by the Complement suite, as a client cannot see it.
check D "a benign message is sent"

reply=$(send_text d "hello")
expect_status 200 "$reply" "benign message"
[ -n "$(body_of "$reply" | field event_id)" ] || fail "no event_id: $(body_of "$reply")"

pass

# E: a message policyserv refuses is refused with its answer, errcode and
# message both.
check E "a refused message is refused with policyserv's answer"

reply=$(send_text e "this is spam")
expect_status 400 "$reply" "refused message"

refusal=$(body_of "$reply")
[ "$(printf '%s' "$refusal" | field errcode)" = M_FORBIDDEN ] || fail "refusal not relayed: $refusal"
[ "$(printf '%s' "$refusal" | field error)" = "$REFUSAL_MESSAGE" ] || fail "refusal not relayed: $refusal"

pass

# F: the refusal does not carry over to the next message.
check F "a message after a refusal is sent"

reply=$(send_text f "hello again")
expect_status 200 "$reply" "message after a refusal"

pass

# G: with policyserv unreachable, a message gets no verdict and fails.
check G "a message during the outage fails as unavailable"

docker stop "$front" >/dev/null

started=$(date +%s)
reply=$(send_text g "hello during outage")
elapsed=$(( $(date +%s) - started ))

expect_status 503 "$reply" "message during the outage"
[ "$elapsed" -le 60 ] || fail "the outage took ${elapsed}s to answer"

pass

# H: leaving needs no verdict, so it goes out during the outage.
check H "leaving during the outage succeeds"

reply=$(request POST "$hs_client/_matrix/client/v3/rooms/$room/leave" "$token" '{}')
expect_status 200 "$reply" "leave during the outage"

pass

log "policyserv integration test PASSED"
printf '\033[1;42;30mACCEPT\033[0m\n'
