# syntax = docker/dockerfile:1.11-labs

FROM input AS nix-base
ARG nix_substituter="https://tuwunel.cachix.org"
ARG nix_public_key="tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc="

# The substituter is recorded in nix.conf rather than relying on the flake's
# nixConfig, because build-nix realises the tree through default.nix, where
# nixConfig does not apply. Builds run as root, so root's trusted-user status
# makes these settings effective without a daemon restart.
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
	set -eux
	curl --proto '=https' --tlsv1.2 -L https://nixos.org/nix/install > nix-install
	sh ./nix-install --daemon
	rm nix-install

	mkdir -p /etc/nix
	printf '%s\n' \
		"extra-substituters = ${nix_substituter}" \
		"extra-trusted-public-keys = ${nix_public_key}" \
		>> /etc/nix/nix.conf
EOF


FROM nix-base AS build-nix
ARG sched_policy="--batch"
ARG sched_prio=0
ARG cachix_cache="tuwunel"
ARG cachix_push=0

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV sched_policy=${sched_policy}
ENV sched_prio=${sched_prio}

# cachix_push participates in the layer cache key on purpose: without it a
# tokenless build could populate the cache entry and suppress the push on the
# next tokened build of the same tree.
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
--mount=type=secret,id=cachix_auth_token,env=CACHIX_AUTH_TOKEN \
<<EOF
	set -eux

	sched_wrap.sh nix-build \
		--verbose \
		--cores 0 \
		--max-jobs $(nproc) \
		--log-format raw \
		.

	cp -afRL --copy-contents result /opt/tuwunel

	# Upload the realised output together with its build closure, so a later
	# run substitutes the toolchain and rocksdb instead of rebuilding them.
	# A failed upload must never fail the build. Tracing is suspended across
	# the token test alone, which is the only place it would be expanded into
	# the build log; cachix itself reads it from the environment.
	set +x
	if test "${cachix_push}" = "1" && test -n "${CACHIX_AUTH_TOKEN:-}"; then
		push_paths=1
	else
		push_paths=0
	fi
	set -x

	if test "$push_paths" = "1"; then
		nix-store \
			--query --requisites --include-outputs \
			"$(nix-store --query --deriver result)" \
			> /tmp/cachix-paths

		nix \
			--extra-experimental-features nix-command \
			--extra-experimental-features flakes \
			shell --inputs-from . cachix \
			-c xargs -r -a /tmp/cachix-paths \
			cachix push "${cachix_cache}" || true
	fi
EOF


FROM nix-base AS smoke-nix
ARG sched_policy="--rr"
ARG sched_prio=1

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV TUWUNEL_DATABASE_PATH="/tmp/tuwunel/smoketest.db"
ENV TUWUNEL_LOG="info"
ENV sched_policy=${sched_policy}
ENV sched_prio=${sched_prio}
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
    set -eux

    sched_wrap.sh nix \
        --extra-experimental-features nix-command \
        --extra-experimental-features flakes \
        run \
        --verbose \
        --cores 0 \
        --max-jobs $(nproc) \
        --log-format raw \
        .#all-features \
            -- \
            -Otest='["smoke", "fresh"]' \
            -Oserver_name=\"localhost\" \
            -Oerror_on_unknown_config_opts=true \
EOF


FROM nix-base AS nix-pkg
ARG sched_policy="--rr"
ARG sched_prio=1

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV sched_policy=${sched_policy}
ENV sched_prio=${sched_prio}
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
    set -eux
    alias nix="nix --extra-experimental-features nix-command --extra-experimental-features flakes"

    ID=$(sched_wrap.sh nix-store --realise $(nix path-info --derivation))

    mkdir -p tuwunel
    nix-store --export $ID > tuwunel/tuwunel.drv
    tar -cvf /opt/tuwunel.nix.tar tuwunel
EOF
