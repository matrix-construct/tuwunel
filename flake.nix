{
  inputs = {
    attic.url = "github:zhaofengli/attic?ref=main";
    cachix.url = "github:cachix/cachix?ref=master";
    complement = {
      url = "github:matrix-construct/complement?ref=main";
      flake = false;
    };
    crane = {
      url = "github:ipetkov/crane?ref=master";
    };
    fenix = {
      url = "github:nix-community/fenix?ref=main";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-compat = {
      url = "github:edolstra/flake-compat?ref=master";
      flake = false;
    };
    flake-utils.url = "github:numtide/flake-utils?ref=main";
    nix-filter.url = "github:numtide/nix-filter?ref=main";
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixpkgs-unstable";
    rocksdb = {
      url = "github:matrix-construct/rocksdb?ref=tuwunel-changes";
      flake = false;
    };
    liburing = {
      url = "github:axboe/liburing?ref=master";
      flake = false;
    };
  };

  outputs =
    inputs:
    inputs.flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgsHost = import inputs.nixpkgs {
          inherit system;
        };
        pkgsHostStatic = pkgsHost.pkgsStatic;

        # The Rust toolchain to use
        toolchain = inputs.fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;

          # See also `rust-toolchain.toml`
          sha256 = "sha256-SDu4snEWjuZU475PERvu+iO50Mi39KVjqCeJeNvpguU=";
        };

        mkScope =
          pkgs:
          pkgs.lib.makeScope pkgs.newScope (self: {
            inherit pkgs;
            book = self.callPackage ./nix/pkgs/book { };
            complement = self.callPackage ./nix/pkgs/complement { };
            craneLib = ((inputs.crane.mkLib pkgs).overrideToolchain (_: toolchain));
            inherit inputs;
            main = self.callPackage ./nix/pkgs/main { };
            oci-image = self.callPackage ./nix/pkgs/oci-image { };
            tini = pkgs.tini.overrideAttrs {
              # newer clang/gcc is unhappy with tini-static: <https://3.dog/~strawberry/pb/c8y4>
              patches = [
                (pkgs.fetchpatch {
                  url = "https://patch-diff.githubusercontent.com/raw/krallin/tini/pull/224.patch";
                  hash = "sha256-4bTfAhRyIT71VALhHY13hUgbjLEUyvgkIJMt3w9ag3k=";
                })
              ];
            };
            liburing = pkgs.liburing.overrideAttrs {
              # Tests weren't building
              outputs = [
                "out"
                "dev"
                "man"
              ];
              buildFlags = [ "library" ];
              src = inputs.liburing;
            };
            rocksdb =
              (pkgs.rocksdb.override {
                liburing = self.liburing;
              }).overrideAttrs
                (
                  final: old: {
                    src = inputs.rocksdb;
                    version = pkgs.lib.removePrefix "v" (builtins.fromJSON (builtins.readFile ./flake.lock))
                    .nodes.rocksdb.original.ref;
                    # we have this already at https://github.com/girlbossceo/rocksdb/commit/a935c0273e1ba44eacf88ce3685a9b9831486155
                    # unsetting this so i don't have to revert it and make this nix exclusive
                    patches = [ ];
                    postPatch = pkgs.lib.optionalString (pkgs.lib.versionOlder final.version "8") ''
                      # Fix gcc-13 build failures due to missing <cstdint> and
                      # <system_error> includes, fixed upstream since 8.x
                      sed -e '1i #include <cstdint>' -i db/compaction/compaction_iteration_stats.h
                      sed -e '1i #include <cstdint>' -i table/block_based/data_block_hash_index.h
                      sed -e '1i #include <cstdint>' -i util/string_util.h
                      sed -e '1i #include <cstdint>' -i include/rocksdb/utilities/checkpoint.h
                    '';
                    cmakeFlags =
                      pkgs.lib.subtractLists [
                        # no real reason to have snappy or zlib, no one uses this
                        "-DWITH_SNAPPY=1"
                        "-DZLIB=1"
                        "-DWITH_ZLIB=1"
                        # we dont need to use ldb or sst_dump (core_tools)
                        "-DWITH_CORE_TOOLS=1"
                        # we dont need to build rocksdb tests
                        "-DWITH_TESTS=1"
                        # we use rust-rocksdb via C interface and dont need C++ RTTI
                        "-DUSE_RTTI=1"
                        # this doesn't exist in RocksDB, and USE_SSE is deprecated for
                        # PORTABLE=$(march)
                        "-DFORCE_SSE42=1"
                        # PORTABLE will get set in main/default.nix
                        "-DPORTABLE=1"
                      ] old.cmakeFlags
                      ++ [
                        # no real reason to have snappy, no one uses this
                        "-DWITH_SNAPPY=0"
                        "-DZLIB=0"
                        "-DWITH_ZLIB=0"
                        # we dont need to use ldb or sst_dump (core_tools)
                        "-DWITH_CORE_TOOLS=0"
                        # we dont need trace tools
                        "-DWITH_TRACE_TOOLS=0"
                        # we dont need to build rocksdb tests
                        "-DWITH_TESTS=0"
                        # we use rust-rocksdb via C interface and dont need C++ RTTI
                        "-DUSE_RTTI=0"
                      ];

                    # outputs has "tools" which we dont need or use
                    outputs = [ "out" ];

                    # preInstall hooks has stuff for messing with ldb/sst_dump which we dont need or use
                    preInstall = "";
                  }
                );
          });

        scopeHost = mkScope pkgsHost;
        scopeHostStatic = mkScope pkgsHostStatic;
        scopeCrossLinux = mkScope pkgsHost.pkgsLinux.pkgsStatic;
        mkCrossScope =
          crossSystem:
          let
            pkgsCrossStatic =
              (import inputs.nixpkgs {
                inherit system;
                crossSystem = {
                  config = crossSystem;
                };
              }).pkgsStatic;
          in
          mkScope pkgsCrossStatic;

        mkDevShell =
          scope:
          scope.pkgs.mkShell {
            env = scope.main.env // {
              # Rust Analyzer needs to be able to find the path to default crate
              # sources, and it can read this environment variable to do so. The
              # `rust-src` component is required in order for this to work.
              RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";

              # Convenient way to access a pinned version of Complement's source
              # code.
              COMPLEMENT_SRC = inputs.complement.outPath;

              # Needed for Complement: <https://github.com/golang/go/issues/52690>
              CGO_CFLAGS = "-Wl,--no-gc-sections";
              CGO_LDFLAGS = "-Wl,--no-gc-sections";
            };

            # Development tools
            packages = [
              # Always use nightly rustfmt because most of its options are unstable
              #
              # This needs to come before `toolchain` in this list, otherwise
              # `$PATH` will have stable rustfmt instead.
              inputs.fenix.packages.${system}.latest.rustfmt

              toolchain
            ]
            ++ (
              with pkgsHost.pkgs;
              [
                # Required by hardened-malloc.rs dep
                binutils

                cargo-audit
                cargo-auditable

                # Needed for producing Debian packages
                cargo-deb

                # Needed for CI to check validity of produced Debian packages (dpkg-deb)
                dpkg

                engage

                # Needed for Complement
                go

                # Needed for our script for Complement
                jq
                gotestfmt

                # Needed for finding broken markdown links
                lychee

                # Needed for linting markdown files
                markdownlint-cli

                # Useful for editing the book locally
                mdbook

                # used for rust caching in CI to speed it up
                sccache
              ]
              # liburing is Linux-exclusive
              ++ lib.optional stdenv.hostPlatform.isLinux liburing
              ++ lib.optional stdenv.hostPlatform.isLinux numactl
            )
            ++ scope.main.buildInputs
            ++ scope.main.propagatedBuildInputs
            ++ scope.main.nativeBuildInputs;
          };
      in
      {
        packages = {
          default = scopeHost.main.override {
            disable_features = [
              # dont include experimental features
              "experimental"
              # jemalloc profiling/stats features are expensive and shouldn't
              # be expected on non-debug builds.
              "jemalloc_prof"
              "jemalloc_stats"
              # tuwunel_mods is a development-only hot reload feature
              "tuwunel_mods"
            ];
          };
          default-debug = scopeHost.main.override {
            profile = "dev";
            # debug build users expect full logs
            disable_release_max_log_level = true;
            disable_features = [
              # dont include experimental features
              "experimental"
              # this is non-functional on nix for some reason
              "tuwunel_mods"
            ];
          };
          # just a test profile used for things like CI and complement
          default-test = scopeHost.main.override {
            profile = "test";
            disable_release_max_log_level = true;
            disable_features = [
              # dont include experimental features
              "experimental"
              # this is non-functional on nix for some reason
              "tuwunel_mods"
            ];
          };
          all-features = scopeHost.main.override {
            all_features = true;
            disable_features = [
              # dont include experimental features
              "experimental"
              # jemalloc profiling/stats features are expensive and shouldn't
              # be expected on non-debug builds.
              "jemalloc_prof"
              "jemalloc_stats"
              # this is non-functional on nix for some reason
              "tuwunel_mods"
            ];
          };
          all-features-debug = scopeHost.main.override {
            profile = "dev";
            all_features = true;
            # debug build users expect full logs
            disable_release_max_log_level = true;
            disable_features = [
              # dont include experimental features
              "experimental"
              # this is non-functional on nix for some reason
              "tuwunel_mods"
            ];
          };

          oci-image = scopeHost.oci-image;
          oci-image-all-features = scopeHost.oci-image.override {
            main = scopeHost.main.override {
              all_features = true;
              disable_features = [
                # dont include experimental features
                "experimental"
                # jemalloc profiling/stats features are expensive and shouldn't
                # be expected on non-debug builds.
                "jemalloc_prof"
                "jemalloc_stats"
                # tuwunel_mods is a development-only hot reload feature
                "tuwunel_mods"
              ];
            };
          };
          oci-image-all-features-debug = scopeHost.oci-image.override {
            main = scopeHost.main.override {
              profile = "dev";
              all_features = true;
              # debug build users expect full logs
              disable_release_max_log_level = true;
              disable_features = [
                # dont include experimental features
                "experimental"
                # tuwunel_mods is a development-only hot reload feature
                "tuwunel_mods"
              ];
            };
          };

          book = scopeHost.book;

          complement = scopeHost.complement;
          static-complement = scopeHostStatic.complement;
          # macOS containers don't exist, so the complement images must be forced to linux
          linux-complement = (mkCrossScope "${pkgsHost.hostPlatform.qemuArch}-linux-musl").complement;
        }
        // builtins.listToAttrs (
          builtins.concatLists (
            builtins.map
              (
                crossSystem:
                let
                  binaryName = "static-${crossSystem}";
                  scopeCrossStatic = mkCrossScope crossSystem;
                in
                [
                  # An output for a statically-linked binary
                  {
                    name = binaryName;
                    value = scopeCrossStatic.main;
                  }

                  # An output for a statically-linked binary with x86_64 haswell
                  # target optimisations
                  {
                    name = "${binaryName}-x86_64-haswell-optimised";
                    value = scopeCrossStatic.main.override {
                      x86_64_haswell_target_optimised = (
                        if (crossSystem == "x86_64-linux-gnu" || crossSystem == "x86_64-linux-musl") then true else false
                      );
                    };
                  }

                  # An output for a statically-linked unstripped debug ("dev") binary
                  {
                    name = "${binaryName}-debug";
                    value = scopeCrossStatic.main.override {
                      profile = "dev";
                      # debug build users expect full logs
                      disable_release_max_log_level = true;
                    };
                  }

                  # An output for a statically-linked unstripped debug binary with the
                  # "test" profile (for CI usage only)
                  {
                    name = "${binaryName}-test";
                    value = scopeCrossStatic.main.override {
                      profile = "test";
                      disable_release_max_log_level = true;
                      disable_features = [
                        # dont include experimental features
                        "experimental"
                        # tuwunel_mods is a development-only hot reload feature
                        "tuwunel_mods"
                      ];
                    };
                  }

                  # An output for a statically-linked binary with `--all-features`
                  {
                    name = "${binaryName}-all-features";
                    value = scopeCrossStatic.main.override {
                      all_features = true;
                      disable_features = [
                        # dont include experimental features
                        "experimental"
                        # jemalloc profiling/stats features are expensive and shouldn't
                        # be expected on non-debug builds.
                        "jemalloc_prof"
                        "jemalloc_stats"
                        # tuwunel_mods is a development-only hot reload feature
                        "tuwunel_mods"
                      ];
                    };
                  }

                  # An output for a statically-linked binary with `--all-features` and with x86_64 haswell
                  # target optimisations
                  {
                    name = "${binaryName}-all-features-x86_64-haswell-optimised";
                    value = scopeCrossStatic.main.override {
                      all_features = true;
                      disable_features = [
                        # dont include experimental features
                        "experimental"
                        # jemalloc profiling/stats features are expensive and shouldn't
                        # be expected on non-debug builds.
                        "jemalloc_prof"
                        "jemalloc_stats"
                        # tuwunel_mods is a development-only hot reload feature
                        "tuwunel_mods"
                      ];
                      x86_64_haswell_target_optimised = (
                        if (crossSystem == "x86_64-linux-gnu" || crossSystem == "x86_64-linux-musl") then true else false
                      );
                    };
                  }

                  # An output for a statically-linked unstripped debug ("dev") binary with `--all-features`
                  {
                    name = "${binaryName}-all-features-debug";
                    value = scopeCrossStatic.main.override {
                      profile = "dev";
                      all_features = true;
                      # debug build users expect full logs
                      disable_release_max_log_level = true;
                      disable_features = [
                        # dont include experimental features
                        "experimental"
                        # tuwunel_mods is a development-only hot reload feature
                        "tuwunel_mods"
                      ];
                    };
                  }

                  # An output for an OCI image based on that binary
                  {
                    name = "oci-image-${crossSystem}";
                    value = scopeCrossStatic.oci-image;
                  }

                  # An output for an OCI image based on that binary with x86_64 haswell
                  # target optimisations
                  {
                    name = "oci-image-${crossSystem}-x86_64-haswell-optimised";
                    value = scopeCrossStatic.oci-image.override {
                      main = scopeCrossStatic.main.override {
                        x86_64_haswell_target_optimised = (
                          if (crossSystem == "x86_64-linux-gnu" || crossSystem == "x86_64-linux-musl") then true else false
                        );
                      };
                    };
                  }

                  # An output for an OCI image based on that unstripped debug ("dev") binary
                  {
                    name = "oci-image-${crossSystem}-debug";
                    value = scopeCrossStatic.oci-image.override {
                      main = scopeCrossStatic.main.override {
                        profile = "dev";
                        # debug build users expect full logs
                        disable_release_max_log_level = true;
                      };
                    };
                  }

                  # An output for an OCI image based on that binary with `--all-features`
                  {
                    name = "oci-image-${crossSystem}-all-features";
                    value = scopeCrossStatic.oci-image.override {
                      main = scopeCrossStatic.main.override {
                        all_features = true;
                        disable_features = [
                          # dont include experimental features
                          "experimental"
                          # jemalloc profiling/stats features are expensive and shouldn't
                          # be expected on non-debug builds.
                          "jemalloc_prof"
                          "jemalloc_stats"
                          # tuwunel_mods is a development-only hot reload feature
                          "tuwunel_mods"
                        ];
                      };
                    };
                  }

                  # An output for an OCI image based on that binary with `--all-features` and with x86_64 haswell
                  # target optimisations
                  {
                    name = "oci-image-${crossSystem}-all-features-x86_64-haswell-optimised";
                    value = scopeCrossStatic.oci-image.override {
                      main = scopeCrossStatic.main.override {
                        all_features = true;
                        disable_features = [
                          # dont include experimental features
                          "experimental"
                          # jemalloc profiling/stats features are expensive and shouldn't
                          # be expected on non-debug builds.
                          "jemalloc_prof"
                          "jemalloc_stats"
                          # tuwunel_mods is a development-only hot reload feature
                          "tuwunel_mods"
                        ];
                        x86_64_haswell_target_optimised = (
                          if (crossSystem == "x86_64-linux-gnu" || crossSystem == "x86_64-linux-musl") then true else false
                        );
                      };
                    };
                  }

                  # An output for an OCI image based on that unstripped debug ("dev") binary with `--all-features`
                  {
                    name = "oci-image-${crossSystem}-all-features-debug";
                    value = scopeCrossStatic.oci-image.override {
                      main = scopeCrossStatic.main.override {
                        profile = "dev";
                        all_features = true;
                        # debug build users expect full logs
                        disable_release_max_log_level = true;
                        disable_features = [
                          # dont include experimental features
                          "experimental"
                          # tuwunel_mods is a development-only hot reload feature
                          "tuwunel_mods"
                        ];
                      };
                    };
                  }

                  # An output for a complement OCI image for the specified platform
                  {
                    name = "complement-${crossSystem}";
                    value = scopeCrossStatic.complement;
                  }
                ]
              )
              [
                #"x86_64-apple-darwin"
                #"aarch64-apple-darwin"
                "x86_64-linux-gnu"
                "x86_64-linux-musl"
                "aarch64-linux-musl"
              ]
          )
        );

        devShells.default = mkDevShell scopeHostStatic;
        devShells.all-features = mkDevShell (
          scopeHostStatic.overrideScope (
            final: prev: {
              main = prev.main.override {
                all_features = true;
                disable_features = [
                  # dont include experimental features
                  "experimental"
                  # jemalloc profiling/stats features are expensive and shouldn't
                  # be expected on non-debug builds.
                  "jemalloc_prof"
                  "jemalloc_stats"
                  # tuwunel_mods is a development-only hot reload feature
                  "tuwunel_mods"
                ];
              };
            }
          )
        );
        devShells.no-features = mkDevShell (
          scopeHostStatic.overrideScope (
            final: prev: {
              main = prev.main.override { default_features = false; };
            }
          )
        );
        devShells.dynamic = mkDevShell scopeHost;
      }
    );
}
