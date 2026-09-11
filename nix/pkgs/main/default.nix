# Dependencies (keep sorted)
{
  craneLib,
  inputs,
  jq,
  lib,
  libiconv,
  liburing,
  pkgsBuildHost,
  rocksdb,
  removeReferencesTo,
  rust,
  autoPatchelfHook,
  stdenv,

  # Options (keep sorted)
  all_features ? false,
  default_features ? true,
  # default list of disabled features
  disable_features ? [
    # jemalloc profiling/stats features are expensive and shouldn't
    # be expected on non-debug builds.
    "jemalloc_prof"
    "jemalloc_stats"
    # tuwunel_mods is a development-only hot reload feature
    "tuwunel_mods"
  ],
  disable_release_max_log_level ? false,
  features ? [ ],
  profile ? "release",
  # rocksdb compiled with -march=haswell and target-cpu=haswell rustflag
  # haswell is pretty much any x86 cpu made in the last 12 years, and
  # supports modern CPU extensions that rocksdb can make use of.
  # disable if trying to make a portable x86_64 build for very old hardware
  x86_64_haswell_target_optimised ? false,
}:

let
  # We perform default-feature unification in nix, because some of the dependencies
  # on the nix side depend on feature values.
  crateFeatures =
    path:
    let
      manifest = lib.importTOML "${path}/Cargo.toml";
    in
    lib.remove "default" (lib.attrNames manifest.features);
  crateDefaultFeatures = path: (lib.importTOML "${path}/Cargo.toml").features.default;
  allDefaultFeatures = crateDefaultFeatures "${inputs.self}/src/main";
  allFeatures = crateFeatures "${inputs.self}/src/main";
  features' = lib.unique (
    features
    ++ lib.optionals default_features allDefaultFeatures
    ++ lib.optionals all_features allFeatures
  );
  disable_features' =
    disable_features ++ lib.optionals disable_release_max_log_level [ "release_max_log_level" ];
  features'' = lib.subtractLists disable_features' features';

  featureEnabled = feature: builtins.elem feature features'';

  enableLiburing = featureEnabled "io_uring" && !stdenv.hostPlatform.isDarwin;

  rocksdb' =
    (rocksdb.override {
      # RocksDB's C++ allocations reach jemalloc by symbol interposition
      # from the unprefixed allocator the Rust build links, so it needs
      # none of its own. A second jemalloc here would serve only the
      # opt-in nodump allocator and malloc-stats, out of a separate heap,
      # and tuwunel uses neither.
      enableJemalloc = false;

      # for some reason enableLiburing in nixpkgs rocksdb is default true
      # which breaks Darwin entirely
      enableLiburing = enableLiburing;
    }).overrideAttrs
      (old: {
        enableLiburing = enableLiburing;
        cmakeFlags =
          (
            if x86_64_haswell_target_optimised then
              (
                lib.subtractLists [
                  # dont make a portable build if x86_64_haswell_target_optimised is enabled
                  "-DPORTABLE=1"
                ] old.cmakeFlags
                ++ [ "-DPORTABLE=haswell" ]
              )
            else
              ([ "-DPORTABLE=1" ])
          )
          ++ old.cmakeFlags;

        # outputs has "tools" which we dont need or use
        outputs = [ "out" ];

        # preInstall hooks has stuff for messing with ldb/sst_dump which we dont need or use
        preInstall = "";
      });

  buildDepsOnlyEnv = {
    # https://crane.dev/faq/rebuilds-bindgen.html
    NIX_OUTPATH_USED_AS_RANDOM_SEED = "aaaaaaaaaa";

    CARGO_PROFILE = profile;
    ROCKSDB_INCLUDE_DIR = "${rocksdb'}/include";
    ROCKSDB_LIB_DIR = "${rocksdb'}/lib";
  }
  // (import ./cross-compilation-env.nix {
    # Keep sorted
    inherit
      lib
      pkgsBuildHost
      rust
      stdenv
      ;
  });

  buildPackageEnv = {
    TUWUNEL_VERSION_EXTRA = inputs.self.shortRev or inputs.self.dirtyShortRev or "";
    TUWUNEL_DATABASE_PATH = "/var/tmp/tuwunel.db";
  }
  // buildDepsOnlyEnv
  // {
    # Only needed in static stdenv because these are transitive dependencies of rocksdb
    CARGO_BUILD_RUSTFLAGS =
      buildDepsOnlyEnv.CARGO_BUILD_RUSTFLAGS
      + lib.optionalString (
        enableLiburing && stdenv.hostPlatform.isStatic
      ) " -L${lib.getLib liburing}/lib -luring"
      + lib.optionalString x86_64_haswell_target_optimised " -Ctarget-cpu=haswell";
  };

  commonAttrs = {
    inherit
      (craneLib.crateNameFromCargoToml {
        cargoToml = "${inputs.self}/Cargo.toml";
      })
      pname
      version
      ;

    src =
      let
        filter = inputs.nix-filter.lib;
      in
      filter {
        root = inputs.self;

        # Keep sorted
        include = [
          ".cargo"
          "Cargo.lock"
          "Cargo.toml"
          "src"
        ];
      };

    # The check phase reaches /etc/resolv.conf through libredirect, which works
    # by LD_PRELOAD and is therefore inert in a statically linked binary. A
    # static build would run the tests with no resolver configuration at all and
    # fail before reaching them, so it packages without checking; the unit and
    # integ jobs cover that code on the dynamic path.
    doCheck = !stdenv.hostPlatform.isStatic;

    # Cargo applies the selected profile to every target, so checking under
    # release links each test binary with thin LTO. Tests build under the test
    # profile instead, matching the profile the unit and integ jobs use.
    # Full-server integration binaries are large enough that concurrent links
    # can exhaust memory and drive the builder into swap.
    #
    # Only the unit tests run here. The 41 integration targets under
    # src/main/tests each boot a server, which a nix builder cannot support: it
    # has no network, denies io_uring, and offers no resolver configuration. The
    # unit and integ CI jobs run them with those things available. What is left
    # is what the check phase is actually useful for, confirming that the
    # nixpkgs-linked build of our own crates works.
    cargoTestCommand = "cargo test --lib --bins -j 1";
    RUST_TEST_THREADS = "1";

    cargoExtraArgs =
      "--no-default-features --locked "
      + lib.optionalString (features'' != [ ]) "--features "
      + (builtins.concatStringsSep "," features'');

    dontStrip = profile == "dev" || profile == "test";
    dontPatchELF = profile == "dev" || profile == "test";

    buildInputs =
      # needed to build Rust applications on macOS
      lib.optionals stdenv.hostPlatform.isDarwin [
        # https://github.com/NixOS/nixpkgs/issues/206242
        # ld: library not found for -liconv
        libiconv
      ];

    nativeBuildInputs = [
      # bindgen needs the build platform's libclang. Apparently due to "splicing
      # weirdness", pkgs.rustPlatform.bindgenHook on its own doesn't quite do the
      # right thing here.
      pkgsBuildHost.rustPlatform.bindgenHook

      # We don't actually depend on `jq`, but crane's `buildPackage` does, but
      # its `buildDepsOnly` doesn't. This causes those two derivations to have
      # differing values for `NIX_CFLAGS_COMPILE`, which contributes to spurious
      # rebuilds of bindgen and its depedents.
      jq
    ];
  };
in

craneLib.buildPackage (
  commonAttrs
  // rec {
    cargoArtifacts = craneLib.buildDepsOnly (
      commonAttrs
      // {
        env = buildDepsOnlyEnv;
      }
    );

    # Adds runpath settings to the resulting binary
    buildInputs = (commonAttrs.buildInputs or [ ]) ++ [
      rocksdb'
    ];
    nativeBuildInputs = (commonAttrs.nativeBuildInputs or [ ]) ++ [
      autoPatchelfHook
    ];
    # The check phase runs the freshly built test binaries before
    # autoPatchelfHook rewrites their RPATH, so every shared library they
    # load must be reachable through LD_LIBRARY_PATH. rocksdb' covers the
    # system backend; stdenv.cc.cc supplies libstdc++.so.6, which the
    # rust-rocksdb system backend links directly (rustc-link-lib=dylib=stdc++)
    # rather than transitively through librocksdb.
    LD_LIBRARY_PATH = lib.makeLibraryPath (buildInputs ++ [ stdenv.cc.cc ]);

    nativeCheckInputs = [
      pkgsBuildHost.libredirect.hook
    ];

    preCheck =
      let
        fakeResolvConf = pkgsBuildHost.writeTextFile {
          name = "resolv.conf";
          text = ''
            nameserver 0.0.0.0
          '';
        };
      in
      ''
        export NIX_REDIRECTS="/etc/resolv.conf=${fakeResolvConf}"
        export TUWUNEL_DATABASE_PATH="$(mktemp -d)/smoketest.db"
        export SSL_CERT_FILE="${pkgsBuildHost.cacert}/etc/ssl/certs/ca-bundle.crt"
      '';
    doCheck = !stdenv.hostPlatform.isStatic;

    doBenchmark = false;

    cargoExtraArgs =
      "--no-default-features --locked "
      + lib.optionalString (features'' != [ ]) "--features "
      + (builtins.concatStringsSep "," features'');

    env = buildPackageEnv;

    passthru = {
      env = buildPackageEnv;
    };

    meta.mainProgram = commonAttrs.pname;
  }
)
