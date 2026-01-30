# Dependencies
{
  bashInteractive,
  buildEnv,
  coreutils,
  dockerTools,
  lib,
  main,
  stdenv,
  tini,
  writeShellScriptBin,
}:

let
  main' = main.override {
    profile = "test";
    all_features = true;
    disable_release_max_log_level = true;
    disable_features = [
      # console/CLI stuff isn't used or relevant for complement
      "console"
      "tokio_console"
      # sentry telemetry isn't useful for complement, disabled by default anyways
      "sentry_telemetry"
      "perf_measurements"
      # this is non-functional on nix for some reason
      "hardened_malloc"
      # dont include experimental features
      "experimental"
      # compression isn't needed for complement
      "brotli_compression"
      "gzip_compression"
      "zstd_compression"
      # complement doesn't need hot reloading
      "tuwunel_mods"
      # complement doesn't have URL preview media tests
      "url_preview"
    ];
  };

  start = writeShellScriptBin "start" ''
    set -euxo pipefail

    ${lib.getExe' coreutils "env"} \
      TUWUNEL_SERVER_NAME="$SERVER_NAME" \
      ${lib.getExe main'}
  '';
in

dockerTools.buildImage {
  name = "complement-tuwunel";
  tag = "main";

  copyToRoot = buildEnv {
    name = "root";
    pathsToLink = [
      "/nix/pkgs/complement/bin"
    ];
    paths = [
      bashInteractive
      coreutils
      main'
      start
    ];
  };

  config = {
    Cmd = [
      "${lib.getExe start}"
    ];

    Entrypoint =
      if
        !stdenv.hostPlatform.isDarwin
      # Use the `tini` init system so that signals (e.g. ctrl+c/SIGINT)
      # are handled as expected
      then
        [
          "${lib.getExe' tini "tini"}"
          "--"
        ]
      else
        [ ];

    Env = [
      "TUWUNEL_TLS__KEY=${./private_key.key}"
      "TUWUNEL_TLS__CERTS=${./certificate.crt}"
      "TUWUNEL_CONFIG=${./config.toml}"
      "RUST_BACKTRACE=full"
    ];

    ExposedPorts = {
      "8008/tcp" = { };
      "8448/tcp" = { };
    };
  };
}
