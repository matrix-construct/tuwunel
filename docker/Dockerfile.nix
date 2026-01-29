# syntax = docker/dockerfile:1.11-labs

FROM input AS nix-base

WORKDIR /
COPY --link --from=input . .

RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
	set -eux
	curl --proto '=https' --tlsv1.2 -L https://nixos.org/nix/install > nix-install
	sh ./nix-install --daemon
	rm nix-install
EOF


FROM nix-base AS build-nix

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
	set -eux

	nix-build \
		--verbose \
		--cores 0 \
		--max-jobs $(nproc) \
		--log-format raw \
		.

	cp -afRL --copy-contents result /opt/tuwunel
EOF


FROM input AS smoke-nix

WORKDIR /
COPY --link --from=nix-base . .

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV TUWUNEL_DATABASE_PATH="/tmp/tuwunel/smoketest.db"
ENV TUWUNEL_LOG="info"
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
    set -eux
    alias nix="nix --extra-experimental-features nix-command --extra-experimental-features flakes"

    nix run \
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


FROM input AS nix-pkg

WORKDIR /
COPY --link --from=nix-base . .

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
    set -eux
    alias nix="nix --extra-experimental-features nix-command --extra-experimental-features flakes"

    ID=$(nix-store --realise $(nix path-info --derivation))

    mkdir -p tuwunel
    nix-store --export $ID > tuwunel/tuwunel.drv
    tar -cvf /opt/tuwunel.nix.tar tuwunel
EOF
