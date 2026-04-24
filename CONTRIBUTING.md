# Contributing guide

If you would like to work on an [issue][issues] that is not assigned, preferably
ask in the Matrix room first at [#tuwunel:grin.hu][tuwunel-chat],
and comment on it.


## Inclusivity and Diversity

All **MUST** code and write with inclusivity and diversity in mind. See the
[following page by Google on writing inclusive code and
documentation](https://developers.google.com/style/inclusive-documentation).

This **EXPLICITLY** forbids usage of terms like "blacklist"/"whitelist" and
"master"/"slave", [forbids gender-specific words and
phrases](https://developers.google.com/style/pronouns#gender-neutral-pronouns),
forbids ableist language like "sanity-check", "cripple", or "insane", and
forbids culture-specific language (e.g. US-only holidays or cultures).

No exceptions are allowed. Dependencies that may use these terms are allowed but
[do not replicate the name in your functions or
variables](https://developers.google.com/style/inclusive-documentation#write-around).

In addition to language, write and code with the user experience in mind. This
is software that intends to be used by everyone, so make it easy and comfortable
for everyone to use. 🏳️‍⚧️


## Linting and Formatting

It is mandatory all your changes satisfy the lints (clippy, rustc, rustdoc, etc)
and your code is formatted via the **nightly** `cargo fmt`. A lot of the
`rustfmt.toml` features depend on nightly toolchain. It would be ideal if they
weren't nightly-exclusive features, but they currently still are. CI's rustfmt
uses nightly.

If you need to allow a lint, please make sure it's either obvious as to why
(e.g. clippy saying redundant clone but it's actually required) or it has a
comment saying why. Do not write inefficient code for the sake of satisfying
lints. If a lint is wrong and provides a more inefficient solution or
suggestion, allow the lint and mention that in a comment.


### Variable, comment, function, etc standards

Rust's default style and standards with regards to [function names, variable
names, comments](https://rust-lang.github.io/api-guidelines/naming.html), etc
applies here.


## Software testing

Continuous integration runs [Complement][complement] protocol compliance tests
against Tuwunel. The results are compared against a stored baseline via
`git diff` — both new failures and new passes are flagged. If your changes
affect compliance, note it in your pull request and review the result diff
uploaded as an artifact.

See [Complement Testing](development/testing/complement.md) for details on how
the test harness works and how to run Complement locally against a debug or
release build.


## Writing documentation

Tuwunel's website uses [`mdbook`][mdbook] containing [`rustdoc`][rustdoc]
which are deployed via CI pipeline using GitHub Pages. All documentation is
in the `docs/` directory at the top level. The compiled mdbook website is
also uploaded as an artifact.

- To build the book locally run `mdbook build -d <outdir> .` in the project
root.

- To build the book using local stages of the CI pipeline run
`docker/bake.sh book`; the produced docker image will contain it.

- To build the book using Nix, run: `bin/nix-build-and-cache just .#book`

Rust API documentation (rustdoc) is generated from the sourcecode contained
in `src/` and deployed via CI to a directory within GitHub Pages adjacent to
the book. In other contexts mdbook and rustdoc are independent.

- To build the API documents locally run `cargo doc` and browse to
`target/doc/tuwunel/`.

- To build the API documents using local stages of the CI pipeline run
`docker/bake.sh docs`; the produced docker image will contain results in
`/usr/src/tuwunel/target/x86_64-unknown-linux-gnu/doc` (and one can
extrapolate for other platforms).


## Creating pull requests

Please try to keep contributions to the GitHub. While the mirrors of Tuwunel
allow for pull/merge requests, there is no guarantee I will see them in a timely
manner. Additionally, please mark WIP or unfinished or incomplete PRs as drafts.
This prevents me from having to ping once in a while to double check the status
of it, especially when the CI completed successfully and everything so it
*looks* done.

If you open a pull request on one of the mirrors, it is your responsibility to
inform me about its existence. In the future I may try to solve this with more
repo bots in the Tuwunel Matrix room. There is no mailing list or email-patch
support on the sr.ht mirror, but if you'd like to email me a git patch you can
do so at `jasonzemos@gmail.com`.

Direct all PRs/MRs to the `main` branch.

By sending a pull request or patch, you are agreeing that your changes are
allowed to be licenced under the Apache-2.0 licence and all of your conduct is
in line with the Contributor's Covenant, and Tuwunel's Code of Conduct.

Contribution by users who violate either of these code of conducts will not have
their contributions accepted. This includes users who have been banned from
Tuwunel Matrix rooms for Code of Conduct violations.


## Stale Branch Policy

_This section applies to Matrix-Construct members and Tuwunel maintainers_

Branches on the matrix-construct/tuwunel repository are _centrally maintained_.
They may be rebased without your consent. Trivial conflicts may be resolved by
another maintainer. Please resolve more difficult conflicts as soon as possible.
Stale branches may be deleted in some cases; personal repositories are advised
for avoiding any such complications.

[issues]: https://github.com/matrix-construct/tuwunel/issues
[tuwunel-chat]: https://matrix.to/#/#tuwunel:grin.hu
[complement]: https://github.com/matrix-org/complement/
[sytest]: https://github.com/matrix-org/sytest/
[cargo-deb]: https://github.com/kornelski/cargo-deb
[lychee]: https://github.com/lycheeverse/lychee
[markdownlint-cli]: https://github.com/igorshubovych/markdownlint-cli
[cargo-audit]: https://github.com/RustSec/rustsec/tree/main/cargo-audit
[direnv]: https://direnv.net/
[mdbook]: https://rust-lang.github.io/mdBook/
[rustdoc]: https://doc.rust-lang.org/rustdoc/what-is-rustdoc.html
[documentation.yml]: https://github.com/matrix-construct/tuwunel/blob/main/.github/workflows/docs.yml
[rustsdk]: https://github.com/matrix-org/matrix-rust-sdk
