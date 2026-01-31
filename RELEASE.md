# Tuwunel 1.5.0

January 31, 2025

### New Features & Enhancements

- SSO/OIDC support. This feature allows users to register and login via authorizations from OIDC Identity Providers. For example, you can now use your GitHub account to register on the server. Tuwunel implements the OIDC client protocol directly. This is referred to as "legacy SSO" in the Matrix specification; Matrix client support is widespread. Credit to @samip5 for opening the feature-issue (#7), the most 👍 feature of the project.

- [MSC2815](https://github.com/matrix-org/matrix-spec-proposals/pull/2815) has been implemented, allowing configurable redacted event retention and retrieval by room admins. The content of redacted events is persisted for sixty days by default. Redacted events can be viewed using Gomuks.

- Secure limited-use registration token support was implemented by @dasha-uwu building off earlier work by @gingershaped in (56f3f5ea154). Use this feature with the new `!admin token` set of commands.

- An outstanding major rework of the presence system by @lhjt in (#264) coordinates conflicting updates from multiple devices and further builds on push suppression features first introduced by @tototomate123.

- [MSC3706](https://github.com/matrix-org/matrix-spec-proposals/pull/3706) has been implemented, improving the performance and reliability of joining rooms over federation (b33e73672b).

- @VlaDexa implemented reading the `client_secret` configuration for an SSO Identity Provider from a separate file; a recommended secure practice (#256).

- Special thanks to @winyadepla for adding highly sought Matrix RTC (Element Call) documentation for Tuwunel in (#265) and for having a kind heart to follow up with maintenance in (#270).

- Thank you @Xerusion for documenting Traefik for deploying Tuwunel in (#259). This will save a lot of time and headache for many new users!

- At the request of @ChronosXYZ in (#260), @dasha-uwu implemented a configurable feature to include all local users in search results, rather than limiting to those in public or shared rooms (95121ad905fb).

- Thanks to a collaboration by @x86pup and @VlaDexa working through Nix maintenance we can now upgrade the MSRV to 1.91.1 (#275).

- Thank you @scvalex for updating the README indicating Tuwunel is in stable NixOS (#233).

- Thank you @divideableZero for updating the README with great news about an [Alpine Package](https://pkgs.alpinelinux.org/package/edge/testing/x86_64/tuwunel) (#248).

- Storage hardware characteristics for mdraid devices on Linux are now detected. On these systems we can now shape database requests to increase performance above generic defaults.

- EdDSA is now a supported algorithm for JWT logins. Thank you @vnhdx for the excellent report in (#258).

- Optimizations were made to maximize concurrency and cache performance when gathering the `auth_chain`.

- An admin command to manually remove a pusher is available (note: not intended for normal use).

- An admin command to list local users by recent activity was added.

### Bug Fixes

- LDAP users are now auto-joined to configured rooms upon creation. Thank you @yefimg for (#234), we especially appreciate help from domain-experts on these features.

- A surgical fix by @kuhnchris in (#254) addressed a pesky bug where LDAP logins would result in admin privileges being removed for the user. Thank you @foxing-quietly for reporting in (#236).

- @OptimoSupreme fixed issues with unread notification counting, including eliminating one of the last remaining non-async database calls in the codebase in (#253).

- @x86pup fixed linker issues for platforms without static builds of `io_uring`. Thanks @darix for reporting in (#238).

- @x86pup fixed compatibility for our optimized jemalloc build on macOS (#239).

- @dasha-uwu made Livekit operate properly even when federation is disabled (b5f50c3fda3). Thank you @apodavalov for reporting in (#240).

- Thank you @VlaDexa for updating the `Cache-Control` header to cache media as `private` which is more appropriate now in the Authenticated Media era.

- Appservices now receive events properly matching on the sender MXID's localpart thanks to @dasha-uwu (c5508bba58d0).

- Additional PDU format and compliance checks were added by @dasha-uwu (7b2079f71499).

- Codepaths in sync systems which assumed `device_id` from appservices were fixed by @dasha-uwu.

- Auto-joining version 12 rooms was inhibited from a bug fixed by @dasha-uwu in (7115fb2796f).

- Thank you @x86pup for updating our ldap3 dependency with SSL/TLS enhancements in (#243) and fixing errors reported by @fruzitent in (#108).

- Thanks to @x86pup `join_rule` is now properly defaulted in `/publicRooms` responses in (#244); additional compliance tests now pass!

- Thank you @bdfd9 for reporting a regression where tracing spans around registrations did not filter out passwords from the list of fields.

- The timezone and extended profile features were not correctly stabilized last summer and the `m.tz` field was incorrectly labeled `tz`. Thank you @bunnyblack:matrix.org for reporting in #tuwunel:matrix.org.

- @dasha-uwu fixed git tags not being pulled and applied to CI builds (eadc9e782d8).

- @dasha-uwu fixed a bug in sliding-sync which may result in lost invites (fd519ff7f174).

- `since` tokens in legacy sync are now clamped to a maximum when the client sends a value greater than expected, preventing a possibility of missing events during the request.

- Media deletion commands which are time-based suffered a bug from incorrect creation timestamps on some filesystems. This was resolved by exclusively using the `mtime` attribute, which is acceptable because Matrix media is immutable.

- Queries for the deprecated `_matrix._tcp` SRV record have been reactivated due to an ineffective and unenforced sunset by the specification and other implementations.

- Thank you @x86pup and @dasha-uwu for various maintenance and linting efforts for the latest rustc versions and in general.

### Honorable Mentions

- Please take a moment to recognize how lucky we are to have @scvalex as our NixOS package maintainer. From having the wherewithal to rise above the noise and lend this project trust from the very first days, time and again this gentleman has gone above and beyond on our behalf. Thank you @symphorien at NixOS as well for the patch applied surgically in https://github.com/NixOS/nixpkgs/pull/462394.
