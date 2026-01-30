{
  inputs,

  # Dependencies
  dockerTools,
  lib,
  main,
  stdenv,
  tini,
}:

dockerTools.buildLayeredImage {
  name = main.pname;
  tag = "main";
  created = "@${toString inputs.self.lastModified}";
  contents = [
    dockerTools.caCertificates
    main
  ];
  config = {
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
    Cmd = [
      "${lib.getExe main}"
    ];
    Env = [
      "RUST_BACKTRACE=full"
    ];
    Labels = {
      "org.opencontainers.image.authors" =
        "June Clementine Strawberry <june@girlboss.ceo> and Jason Volk
      <jason@zemos.net>";
      "org.opencontainers.image.created" = "@${toString inputs.self.lastModified}";
      "org.opencontainers.image.description" = "a very cool Matrix chat homeserver written in Rust";
      "org.opencontainers.image.documentation" = "https://conduwuit.puppyirl.gay/";
      "org.opencontainers.image.licenses" = "Apache-2.0";
      "org.opencontainers.image.revision" = inputs.self.rev or inputs.self.dirtyRev or "";
      "org.opencontainers.image.source" = "https://github.com/girlbossceo/conduwuit";
      "org.opencontainers.image.title" = main.pname;
      "org.opencontainers.image.url" = "https://conduwuit.puppyirl.gay/";
      "org.opencontainers.image.vendor" = "girlbossceo";
      "org.opencontainers.image.version" = main.version;
    };
  };
}
