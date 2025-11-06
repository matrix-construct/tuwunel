# Tuwunel 1.4.6

November 6, 2025

### New Features

- Element Call discovery support was implemented by @tototomate123 in (#209). Adding a `[[global.well_known.rtc_transports]]` section in your [config file](https://github.com/matrix-construct/tuwunel/blob/e1f89b69ea117f166be423f035a5a34f4c0e7366/tuwunel-example.toml#L1835-L1851) enables discovery. More information on setting up Element Call can be found at [Spaetzblog](https://sspaeth.de/2024/11/sfu/), skipping step one, and performing step three in your Tuwunel config.

- Dehydrated Device support (MSC3814) is now available (#200). This feature allows users to receive encrypted messages without being logged in. Supporting clients will setup the dehydrated device automatically and it will "just work" behind the scenes; in fact, these clients will also hide it from the sessions list to avoid confusion. Support is not widespread yet but it has been tested with matrix-js-sdk clients such as Element-web. This feature was commissioned and made public by an enterprise sponsor.

- Notification panel (the 🔔 button) has been implemented in (#201). Even though Element-web now requires enabling it in the Labs menu, the underlying support (`GET /_matrix/client/v3/notifications`) enhances the push-notification handling of other clients.

- Live room previews are now available. This support (`GET /_matrix/client/v3/events`) allows users, including guests, to sync updates for a public room without joining (4afd6f347b1).

- Thanks to a suggestion by @cyberdoors in (#29), the configuration option `encryption_enabled_by_default_for_room_type` is now available. This feature can enable encryption for a room even when the client does not. The values `all` or `invite` are accepted, the latter roughly meaning DM's only. Neither are enabled by default.

### Enhancements

- Thank you @tototomate123 for improving the reverse-proxy docs, adding dedicated pages for both [Caddy](https://github.com/matrix-construct/tuwunel/blob/e0a997c22784b453735b24907e607412b153ba56/docs/deploying/reverse-proxy-caddy.md) and [Nginx](https://github.com/matrix-construct/tuwunel/blob/e0a997c22784b453735b24907e607412b153ba56/docs/deploying/reverse-proxy-nginx.md) in (#209). Thanks to @tycode for pointing out the docs were missing for alternative proxies in (#197).

- Thanks to an observation by @iwalkalone69 in (#40), the last-seen time for a device in the session list is now updated acceptably. This function piggybacks on the presence system to prevent writing too frequently; testing has never shown it more than a minute or few out of date.

- Thanks to an inquiry by @EntityinArray in (#189) guest-accounts can now be enabled while registration tokens are also enabled to prevent fully open account registration. Note that registration tokens don't apply to guest-accounts and those are still fully open.

- Courtesy of @dasha-uwu the list of servers attempted when joining a room is now properly shuffled to increase the odds of finding a viable server, especially if an additional join attempt is made.

### Bug Fixes

- Special thanks to @BVollmerhaus for finding the TURN secret file configured by `turn_secret_file` was broken in (#211), forcing users to configure `turn_secret` directly. Thank you for fixing this in (#212).

- Thank you @scvalex for updating the nix build for Tuwunel's integration tests and re-enabling all checks. (#215)

- Thanks to a report by @Anagastes in (#146) **Nheko and NeoChat users can now enjoy properly verified devices.** Special thanks for the assistance of @deepbluev7 with diagnosing the cross-signing signature issue.

- Database columns intended for deletion, notably `roomsynctoken_shortstatehash`, never had the deletion command actually invoked on them 😭 explaining the lack of enthusiasm after the 1.4.3 release introduced stateless sync. **Users will now see the free disk space they were promised.** This was uncovered during an unrelated issue investigation courtesy of @frebib.

- Thanks to investigation by @dasha-uwu the pagination tokens in the `/relations` endpoint were buggy and now operate correctly.

- Thanks to @Polve for identifying the `DynamicUser=yes` directive in the systemd files was invalid and advising a replacement in (#207).

- Thanks to @daudix for reporting an edge-case where the server will refuse to start rather than robustly reporting errors during startup checks and recreate a missing media directory (#213).

- Push rule evaluation was never implemented for invites arriving over federation. Notifications are now properly sent in this case.

- Sliding-sync handlers were susceptible to errors under rare circumstances escaping to cause an HTTP 500, which wreaks havoc on the rust-sdk. This has now been prevented.

- Federating with Conduit over several non-essential endpoints was broken. It is unclear whether this affected an actual Conduit release version, but thanks to @kladki a fix is scheduled and we have included a workaround now on this end.
