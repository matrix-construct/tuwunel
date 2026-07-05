#!/bin/bash
# Provision-then-exec wrapper for the Complement testee.
#
# Complement generates a fresh CA per run and copies it into every homeserver
# container at /complement/ca/{ca.crt,ca.key}. Each homeserver is expected to
# present a federation certificate signed by that CA so peers which validate
# certificates (e.g. Synapse) accept the connection. We sign one here for
# $SERVER_NAME and overwrite the baked self-signed certificate the image ships,
# then exec the server passed as our arguments.
#
# When the CA is absent (running outside Complement) the baked certificate is
# left in place, so certificate provisioning is a no-op in that case.
#
# Provisioning runs first, then the arguments are exec'd verbatim. The testee
# entrypoint places sched_wrap.sh at the head of those arguments (ahead of
# perf_wrap.sh and the server), so scheduling covers only the server tree and
# not this provisioning step.
set -eo pipefail

ca_crt="/complement/ca/ca.crt"
ca_key="/complement/ca/ca.key"
tls_crt="/complement/certificate.crt"
tls_key="/complement/private_key.pem"
tls_conf="/complement/server.tls.conf"
tls_csr="/complement/server.tls.csr"

provision() {
	test -f "$ca_crt" && test -f "$ca_key" && test -n "${SERVER_NAME:-}" || return 1

	printf '.include /etc/ssl/openssl.cnf\n\n[SAN]\nsubjectAltName=DNS:%s\n' \
		"$SERVER_NAME" > "$tls_conf"

	openssl genrsa -out "$tls_key" 2048
	openssl req -new -config "$tls_conf" -key "$tls_key" -out "$tls_csr" \
		-subj "/CN=$SERVER_NAME" -reqexts SAN
	openssl x509 -req -in "$tls_csr" \
		-CA "$ca_crt" -CAkey "$ca_key" -set_serial 1 \
		-out "$tls_crt" -extfile "$tls_conf" -extensions SAN
}

if provision 2>/tmp/provision_complement.log; then
	echo "complement: federation certificate signed by run CA for ${SERVER_NAME}"
else
	echo "complement: no run CA, keeping baked certificate" >&2
fi

# Hand off to the provisioned command line verbatim (sched_wrap.sh leads it).
exec "$@"
