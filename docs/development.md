# Development

Information about developing the project. If you plan on contributing, see the
[contributor's guide](./contributing.md). _If you are only interested in using
it, you can safely ignore this page._

### Rust Documentation

Tuwunel's rustdocs are hosted within this book under the [`/docs`][rustdocs]
directory. Developers may build the same documentation locally using `cargo doc`.

### Tuwunel project layout

Tuwunel uses a collection of sub-crates, packages, or workspace members
that indicate what each general area of code is for. All of the workspace
members are under `src/`. The workspace definition is at the top level
`Cargo.toml`. See the Rust documentation on [Workspaces][workspaces] for
general questions and information on Cargo workspaces.

Tuwunel's crates form a directed acyclic-graph without circular dependencies.
Listed here from the top are the most abstract down to the most dependent at
the bottom. Crates only have visibility into other crates listed above them;
they cannot see structs or call functions in any crate listed below them.

- `tuwunel_macros` are Tuwunel Rust [macros][macros] like general helper macros,
logging and error handling macros, and [syn][syn] and
[procedural macros][proc-macro].

- [`tuwunel_core`][tuwunel-core] is core Tuwunel functionality like config
loading, error definitions, global utilities, logging infrastructure, etc.

- [`tuwunel_database`][tuwunel-database] is RocksDB encapsulation, interface
wrappers, configurations, and our opinionated asynchronous database frontend.

- [`tuwunel_service`][tuwunel-service] is stateful runtime functionality
at the heart of the application. This crate is divided into "services" each
with "workers" and queues and all of the moving parts that attend to the
tasks of sending messages and notifications, etc. Each service attempts to
encapsulate any database tables it requires for its persistent state.
Services call other services and they do not form an acyclic graph, for now.

- [`tuwunel_api`][tuwunel-api] is the stateless runtime functionality which
implements the Matrix C-S and S-S API's in a broad set of http handlers. These
handlers call various services to query or update their state as necessary.
They do not interface with raw data or database functions except through a
service.

- [`tuwunel_admin`][tuwunel-admin] is a module that implements the admin
room as a broad set of command API handlers. Similar to `tuwunel_api` these
handlers also interface with various services as necessary. Currently the
admin crate does not call into `tuwunel_api` as a dependency, but this is
not intentional and subject to change.

- [`tuwunel_router`][tuwunel-router] is the webserver and request handling bits,
using axum, tower, tower-http, hyper, etc, and the [server state][state] to
drive the `tuwunel_api` handlers.

- [`main`][tuwunel-main] is the binary executable. This is where the `main()`
function lives, tokio worker and async initialisation, Sentry initialisation,
[clap][clap] init, and signal handling. If you are adding new
[Rust features][features], they *must* go here. This crate is also capable of
compiling as a library for integration testing and embedding.

#### Notes

It is highly unlikely you will ever need to add a new workspace member, instead
look to create a new Service to implement distinct or unique functionality. If
you truly find yourself needing another crate, we recommend reaching out to us in
the Matrix room for discussions about it beforehand.

The primary inspiration for this design was apart of hot reloadable development,
to support "Tuwunel as a library" where specific parts can simply be swapped out.
There is evidence Conduit wanted to go this route too as `axum` is technically an
optional feature in Conduit, and can be compiled without the binary or axum library
for handling inbound web requests; but it was never completed or worked.

## Adding compile-time [features][features]

If you'd like to add a compile-time feature, you must first define it in
the `main` workspace crate located in `src/main/Cargo.toml`. The feature must
enable a feature in the other workspace crate(s) you intend to use it in. Then
the said workspace crate(s) must define the feature there in its `Cargo.toml`.

So, if this is adding a feature to the API such as `woof`, you define the feature
in the `api` crate's `Cargo.toml` as `woof = []`. The feature definition in `main`'s
`Cargo.toml` will be `woof = ["tuwunel-api/woof"]`.

The rationale for this is due to Rust / Cargo not supporting
["workspace level features"][9], we must make a choice of; either scattering
features all over the workspace crates, making it difficult for anyone to add
or remove default features; or define all the features in one central workspace
crate that propagate down/up to the other workspace crates. It is a Cargo pitfall,
and we'd like to see better developer UX in Rust's Workspaces.

Additionally, the definition of one single place makes "feature collection" in our
Nix flake a million times easier instead of collecting and deduping them all from
searching in all the workspace crates' `Cargo.toml`s. Though we wouldn't need to
do this if Rust supported workspace-level features to begin with.

## List of forked dependencies

During Tuwunel development, we have had to fork
some dependencies to support our use-cases in some areas. This ranges from
things said upstream project won't accept for any reason, faster-paced
development (unresponsive or slow upstream), Tuwunel-specific usecases, or
lack of time to upstream some things.

- [ruma/ruma][1]: <https://github.com/matrix-construct/ruma> - various performance
improvements, more features, faster-paced development, better client/server interop
hacks upstream won't accept, etc
- [facebook/rocksdb][2]: <https://github.com/matrix-construct/rocksdb> - liburing
build fixes and GCC debug build fix
- [tikv/jemallocator][3]: <https://github.com/matrix-construct/jemallocator> - musl
builds seem to be broken on upstream, fixes some broken/suspicious code in
places, additional safety measures, and support redzones for Valgrind
- [zyansheep/rustyline-async][4]:
<https://github.com/matrix-construct/rustyline-async> - tab completion callback and
`CTRL+\` signal quit event for Tuwunel console CLI
- [rust-rocksdb/rust-rocksdb][5]:
<https://github.com/matrix-construct/rust-rocksdb-zaidoon1> - [`@zaidoon1`][8]'s fork
has quicker updates, more up to date dependencies, etc. Our fork fixes musl build
issues, removes unnecessary `gtest` include, and uses our RocksDB and jemallocator
forks.
- [tokio-rs/tracing][6]: <https://github.com/matrix-construct/tracing> - Implements
`Clone` for `EnvFilter` to support dynamically changing tracing envfilter's
alongside other logging/metrics things

## Debugging with `tokio-console`

[`tokio-console`][7] can be a useful tool for debugging and profiling. To make a
`tokio-console`-enabled build of Tuwunel, enable the `tokio_console` feature,
disable the default `release_max_log_level` feature, and set the `--cfg
tokio_unstable` flag to enable experimental tokio APIs. A build might look like
this:

```bash
RUSTFLAGS="--cfg tokio_unstable" cargo +nightly build \
    --release \
    --no-default-features \
    --features=systemd,element_hacks,gzip_compression,brotli_compression,zstd_compression,tokio_console
```

You will also need to enable the `tokio_console` config option in Tuwunel when
starting it. This was due to tokio-console causing gradual memory leak/usage
if left enabled.

[1]: https://github.com/ruma/ruma/
[2]: https://github.com/facebook/rocksdb/
[3]: https://github.com/tikv/jemallocator/
[4]: https://github.com/zyansheep/rustyline-async/
[5]: https://github.com/rust-rocksdb/rust-rocksdb/
[6]: https://github.com/tokio-rs/tracing/
[7]: https://docs.rs/tokio-console/latest/tokio_console/
[8]: https://github.com/zaidoon1/
[9]: https://github.com/rust-lang/cargo/issues/12162
[workspaces]: https://doc.rust-lang.org/cargo/reference/workspaces.html
[macros]: https://doc.rust-lang.org/book/ch19-06-macros.html
[syn]: https://docs.rs/syn/latest/syn/
[proc-macro]: https://doc.rust-lang.org/reference/procedural-macros.html
[clap]: https://docs.rs/clap/latest/clap/
[features]: https://doc.rust-lang.org/cargo/reference/features.html
[state]: https://docs.rs/axum/latest/axum/extract/struct.State.html
[rustdocs]: https://matrix-construct.github.io/tuwunel/docs/tuwunel
[tuwunel-macros]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_macros
[tuwunel-core]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_core
[tuwunel-database]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_database
[tuwunel-service]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_service
[tuwunel-api]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_api
[tuwunel-admin]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_admin
[tuwunel-router]: https://matrix-construct.github.io/tuwunel/docs/tuwunel_router
[tuwunel-main]: https://matrix-construct.github.io/tuwunel/docs/tuwunel
