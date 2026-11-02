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
  # Minimum CPU the build may require, given to rustc as -Ctarget-cpu and to
  # rocksdb as -march; null targets the baseline. x86_64 takes any name both
  # compilers accept (e.g. "x86-64-v3"). aarch64 takes only "native": a name
  # there replaces the crc and crypto extensions rocksdb's build adds itself.
  target_cpu ? null,
  # Allow any target_cpu off x86_64. The value must then be a -march string
  # gcc and rustc both accept, and must keep crc and crypto on aarch64.
  target_cpu_unchecked ? false,
}:

assert lib.assertMsg (
  target_cpu == null
  || target_cpu == "native"
  || stdenv.hostPlatform.isx86_64
  || target_cpu_unchecked
) ''
  target_cpu = "${toString target_cpu}": off x86_64 only "native" is checked.
  Here -march replaces the crc/crypto flags rocksdb adds, so a narrower value
  silently drops its hardware crc32c. Set target_cpu_unchecked = true to
  override.'';

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
        cmakeFlags = [ "-DPORTABLE=1" ] ++ old.cmakeFlags;

        # Not PORTABLE=<cpu>: cmake reads any value containing 0, 1, N or Y
        # as a boolean, so names like skylake-avx512 would silently stay
        # baseline. NIX_CFLAGS_COMPILE follows cmake's flags, so this wins.
        # The psABI levels omit pclmul, which rocksdb's crc32c assumes from
        # AVX and then fails to compile without.
        env =
          (old.env or { })
          // lib.optionalAttrs (target_cpu != null) {
            NIX_CFLAGS_COMPILE =
              toString (old.env.NIX_CFLAGS_COMPILE or "")
              + " -march=${target_cpu}"
              + lib.optionalString (builtins.match "x86-64-v[2-4]" target_cpu != null) " -mpclmul";
          };

        # outputs has "tools" which we dont need or use
        outputs = [ "out" ];

        # preInstall hooks has stuff for messing with ldb/sst_dump which we dont need or use
        preInstall = "";
      });

  crossEnv = import ./cross-compilation-env.nix {
    # Keep sorted
    inherit
      lib
      pkgsBuildHost
      rust
      stdenv
      ;
  };

  buildDepsOnlyEnv = {
    # https://crane.dev/faq/rebuilds-bindgen.html
    NIX_OUTPATH_USED_AS_RANDOM_SEED = "aaaaaaaaaa";

    CARGO_PROFILE = profile;
    ROCKSDB_INCLUDE_DIR = "${rocksdb'}/include";
    ROCKSDB_LIB_DIR = "${rocksdb'}/lib";
  }
  // crossEnv
  // {
    # Set on the deps-only build too, so the final build reuses its artifacts.
    CARGO_BUILD_RUSTFLAGS =
      crossEnv.CARGO_BUILD_RUSTFLAGS
      + lib.optionalString (target_cpu != null) " -Ctarget-cpu=${target_cpu}";
  };

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
      ) " -L${lib.getLib liburing}/lib -luring";
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
