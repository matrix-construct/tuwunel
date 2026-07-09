# Security Policy

## Reporting a Vulnerability

Please do not report security vulnerabilities through public GitHub issues,
pull requests, or the public Matrix rooms.

Report them through one of these private channels instead:

1. **GitHub private vulnerability reporting** (preferred):
   [Report a vulnerability](https://github.com/matrix-construct/tuwunel/security/advisories/new).
   This opens a private advisory visible only to you and the maintainers.

2. **Matrix DM**: open a direct chat with the repository admins. The room
   must have end-to-end encryption enabled, and please include all of the
   following admins rather than messaging one individually:

   - [@jason:tuwunel.me](https://matrix.to/#/@jason:tuwunel.me)
   - [@june:woof.gay](https://matrix.to/#/@june:woof.gay)
   - [@june:vern.cc](https://matrix.to/#/@june:vern.cc)
   - [@dasha_uwu:linuxping.win](https://matrix.to/#/@dasha_uwu:linuxping.win)

   Do not post details in a public room.

3. **Email** (discouraged; use one of the channels above if at all possible):
   [jasonzemos@gmail.com](mailto:jasonzemos@gmail.com) or
   [june@girlboss.ceo](mailto:june@girlboss.ceo). If you must report by
   email, encrypt it with PGP, S/MIME, or another form of email encryption.
   June's PGP key is `0x665FE73077489DB0`, available at
   [girlboss.ceo/~strawberry/june.asc](https://girlboss.ceo/~strawberry/june.asc).

Include as much of the following as you can:

- A description of the vulnerability and its impact.
- The affected component (client API, federation, media, appservices,
  authentication, admin room, database, etc.).
- The version or commit you tested against, and relevant configuration.
- Steps to reproduce, or a proof of concept if you have one.
- A proposed patch or fix, if you have one. This is by no means expected, but
  it is much appreciated and helps expedite getting the vulnerability patched.

You will receive an acknowledgment as soon as possible. Please allow time for
the issue to be investigated and fixed before any public disclosure; we will
coordinate the disclosure timeline with you.

## Supported Versions

Security fixes are made against the latest release and the `main` branch.
Older releases do not receive backports; if you are running an affected
version, the fix will arrive in the next release, and upgrading promptly is
the expected remedy.

## Scope

This policy covers the Tuwunel homeserver itself: the code in this repository
and the release artifacts and container images built from it.

Vulnerabilities in the Matrix protocol or specification, or in other Matrix
implementations, should be reported to the [Matrix.org Foundation](https://matrix.org/security-disclosure-policy/)
rather than here. Issues in Tuwunel's dependencies are best reported upstream,
though we appreciate a heads-up if Tuwunel is affected.

## Advisories

Fixed vulnerabilities are published as
[GitHub security advisories](https://github.com/matrix-construct/tuwunel/security/advisories)
alongside the release containing the fix, with credit to the reporter unless
anonymity is requested.

## Verifying This Document

This file is signed with June's PGP key (`0x665FE73077489DB0`, above). The
detached signature lives alongside it as
[`SECURITY.md.asc`](./SECURITY.md.asc):

```sh
curl -O https://girlboss.ceo/~strawberry/june.asc
gpg --import june.asc
gpg --verify SECURITY.md.asc SECURITY.md
```
