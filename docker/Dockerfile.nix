# syntax = docker/dockerfile:1.11-labs

FROM input AS nix-base
ARG sys_name
ARG sys_version
ARG sys_target

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
ARG sys_name
ARG sys_version
ARG sys_target

WORKDIR /usr/src/tuwunel
COPY --link --from=source /usr/src/tuwunel .
ENV TUWUNEL_DATABASE_PATH="/tmp/buildtest.db"
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
	set -eux
	nix-build \
		--cores 0 \
		--max-jobs $(nproc) \
		--log-format raw \
		.

	cp -afRL --copy-contents result /opt/tuwunel
EOF


FROM build-nix AS smoke-nix
ARG sys_name
ARG sys_version
ARG sys_target

WORKDIR /
COPY --link --from=build-nix . .

WORKDIR /opt/tuwunel
ENV TUWUNEL_DATABASE_PATH="/tmp/smoketest.db"
ENV TUWUNEL_LOG="info"
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
    set -eux
    bin/tuwunel \
        -Otest='["smoke"]' \
        -Oserver_name=\"localhost\" \
        -Odatabase_path=\"${TUWUNEL_DATABASE_PATH}\"

    rm -rf "${TUWUNEL_DATABASE_PATH}"
EOF


FROM build-nix AS nix-pkg
ARG sys_name
ARG sys_version
ARG sys_target

WORKDIR /
COPY --link --from=build-nix . .

WORKDIR /usr/src/tuwunel
RUN \
--mount=type=cache,dst=/nix,sharing=shared \
--mount=type=cache,dst=/root/.cache/nix,sharing=shared \
--mount=type=cache,dst=/root/.local/state/nix,sharing=shared \
<<EOF
	set -eux
    #TODO: extract derivation?
EOF
