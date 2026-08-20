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

	cachix_push.sh result
EOF


FROM nix-base AS smoke-nix
ARG sched_policy="--rr"
ARG sched_prio=1
ARG cachix_cache="tuwunel"
ARG cachix_push=0

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV TUWUNEL_DATABASE_PATH="/tmp/tuwunel/smoketest.db"
ENV TUWUNEL_LOG="info"
ENV sched_policy=${sched_policy}
ENV sched_prio=${sched_prio}

# This is the only nix target that runs on an ordinary branch push, since the
# distro packages are gated on tags and main, so it carries the upload that
# keeps the cache warm between releases.
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
--mount=type=secret,id=cachix_auth_token,env=CACHIX_AUTH_TOKEN \
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
            -Oerror_on_unknown_config_opts=true

    # Already realised by the run above, so this only resolves the path.
    nix \
        --extra-experimental-features nix-command \
        --extra-experimental-features flakes \
        build --no-link --print-out-paths .#all-features > /tmp/smoke-out

    cachix_push.sh $(cat /tmp/smoke-out)
EOF


FROM nix-base AS nix-pkg
ARG sched_policy="--rr"
ARG sched_prio=1
ARG cachix_cache="tuwunel"
ARG cachix_push=0

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV sched_policy=${sched_policy}
ENV sched_prio=${sched_prio}
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
--mount=type=secret,id=cachix_auth_token,env=CACHIX_AUTH_TOKEN \
<<EOF
    set -eux
    alias nix="nix --extra-experimental-features nix-command --extra-experimental-features flakes"

    ID=$(sched_wrap.sh nix-store --realise $(nix path-info --derivation))

    mkdir -p tuwunel
    nix-store --export $ID > tuwunel/tuwunel.drv
    tar -cvf /opt/tuwunel.nix.tar tuwunel

    cachix_push.sh $ID
EOF
