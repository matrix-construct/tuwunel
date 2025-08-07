# Troubleshooting Tuwunel

> ## Docker users ⚠️
>
> Docker is extremely UX unfriendly. Because of this, a ton of issues or support
> is actually Docker support, not tuwunel support. We also cannot document the
> ever-growing list of Docker issues here.
>
> If you intend on asking for support and you are using Docker, **PLEASE**
> triple validate your issues are **NOT** because you have a misconfiguration in
> your Docker setup.
>
> If there are things like Compose file issues or Dockerhub image issues, those
> can still be mentioned as long as they're something we can fix.

## Tuwunel and Matrix issues

#### Lost access to admin room

You can reinvite yourself to the admin room through the following methods:
- Use the `--execute "users make_user_admin <username>"` Tuwunel binary
argument once to invite yourslf to the admin room on startup
- Use the Tuwunel console/CLI to run the `users make_user_admin` command
- Or specify the `emergency_password` config option to allow you to temporarily
log into the server account (`@conduit` or `@tuwunel`) from a web client

## General potential issues

#### Potential DNS issues when using Docker

Docker has issues with its default DNS setup that may cause DNS to not be
properly functional when running Tuwunel, resulting in federation issues. The
symptoms of this have shown in excessively long room joins (30+ minutes) from
very long DNS timeouts, log entries of "mismatching responding nameservers",
and/or partial or non-functional inbound/outbound federation.

This is **not** a Tuwunel issue, and is purely a Docker issue. It is not
sustainable for heavy DNS activity which is normal for Matrix federation. The
workarounds for this are:
- Use DNS over TCP via the config option `query_over_tcp_only = true`
- Don't use Docker's default DNS setup and instead allow the container to use
and communicate with your host's DNS servers (host's `/etc/resolv.conf`)

#### DNS No connections available error message

If you receive spurious amounts of error logs saying "DNS No connections
available", this is due to your DNS server (servers from `/etc/resolv.conf`)
being overloaded and unable to handle typical Matrix federation volume. Some
users have reported that the upstream servers are rate-limiting them as well
when they get this error (e.g. popular upstreams like Google DNS).

Matrix federation is extremely heavy and sends wild amounts of DNS requests.
Unfortunately this is by design and has only gotten worse with more
server/destination resolution steps. Synapse also expects a very perfect DNS
setup.

There are some ways you can reduce the amount of DNS queries, but ultimately
the best solution/fix is selfhosting a high quality caching DNS server like
[Unbound][unbound-arch] without any upstream resolvers, and without DNSSEC
validation enabled.

DNSSEC validation is highly recommended to be **disabled** due to DNSSEC being
very computationally expensive, and is extremely susceptible to denial of
service, especially on Matrix. Many servers also strangely have broken DNSSEC
setups and will result in non-functional federation.

Tuwunel cannot provide a "works-for-everyone" Unbound DNS setup guide, but
the [official Unbound tuning guide][unbound-tuning] and the [Unbound Arch Linux wiki page][unbound-arch]
may be of interest. Disabling DNSSEC on Unbound is commenting out trust-anchors
config options and removing the `validator` module.

**Avoid** using `systemd-resolved` as it does **not** perform very well under
high load, and we have identified its DNS caching to not be very effective.

dnsmasq can possibly work, but it does **not** support TCP fallback which can be
problematic when receiving large DNS responses such as from large SRV records.
If you still want to use dnsmasq, make sure you **disable** `dns_tcp_fallback`
in Tuwunel config.

Raising `dns_cache_entries` in Tuwunel config from the default can also assist
in DNS caching, but a full-fledged external caching resolver is better and more
reliable.

If you don't have IPv6 connectivity, changing `ip_lookup_strategy` to match
your setup can help reduce unnecessary AAAA queries
(`1 - Ipv4Only (Only query for A records, no AAAA/IPv6)`).

If your DNS server supports it, some users have reported enabling
`query_over_tcp_only` to force only TCP querying by default has improved DNS
reliability at a slight performance cost due to TCP overhead.

## RocksDB / database issues

#### Database corruption

If your database is corrupted *and* is failing to start (e.g. checksum
mismatch), it may be recoverable but careful steps must be taken, and there is
no guarantee it may be recoverable.

The first thing that can be done is launching Tuwunel with the
`rocksdb_repair` config option set to true. This will tell RocksDB to attempt to
repair itself at launch. If this does not work, disable the option and continue
reading.

RocksDB has the following recovery modes:

- `TolerateCorruptedTailRecords`
- `AbsoluteConsistency`
- `PointInTime`
- `SkipAnyCorruptedRecord`

By default, Tuwunel uses `TolerateCorruptedTailRecords` as generally these may
be due to bad federation and we can re-fetch the correct data over federation.
The RocksDB default is `PointInTime` which will attempt to restore a "snapshot"
of the data when it was last known to be good. This data can be either a few
seconds old, or multiple minutes prior. `PointInTime` may not be suitable for
default usage due to clients and servers possibly not being able to handle
sudden "backwards time travels", and `AbsoluteConsistency` may be too strict.

`AbsoluteConsistency` will fail to start the database if any sign of corruption
is detected. `SkipAnyCorruptedRecord` will skip all forms of corruption unless
it forbids the database from opening (e.g. too severe). Usage of
`SkipAnyCorruptedRecord` voids any support as this may cause more damage and/or
leave your database in a permanently inconsistent state, but it may do something
if `PointInTime` does not work as a last ditch effort.

With this in mind:

- First start Tuwunel with the `PointInTime` recovery method. See the [example
config](configuration/examples.md) for how to do this using
`rocksdb_recovery_mode`
- If your database successfully opens, clients are recommended to clear their
client cache to account for the rollback
- Leave your Tuwunel running in `PointInTime` for at least 30-60 minutes so as
much possible corruption is restored
- If all goes will, you should be able to restore back to using
`TolerateCorruptedTailRecords` and you have successfully recovered your database

## Debugging

Note that users should not really be debugging things. If you find yourself
debugging and find the issue, please let us know and/or how we can fix it.
Various debug commands can be found in `!admin debug`.

#### Debug/Trace log level

Tuwunel builds without debug or trace log levels at compile time by default
for substantial performance gains in CPU usage and improved compile times. If
you need to access debug/trace log levels, you will need to build without the
`release_max_log_level` feature or use our provided static debug binaries.

#### Changing log level dynamically

Tuwunel supports changing the tracing log environment filter on-the-fly using
the admin command `!admin debug change-log-level <log env filter>`. This accepts
a string **without quotes** the same format as the `log` config option.

Example: `!admin debug change-log-level debug`

This can also accept complex filters such as:
`!admin debug change-log-level info,conduit_service[{dest="example.com"}]=trace,ruma_state_res=trace`
`!admin debug change-log-level info,conduit_service[{dest="example.com"}]=trace,conduit_service[send{dest="example.org"}]=trace`

And to reset the log level to the one that was set at startup / last config
load, simply pass the `--reset` flag.

`!admin debug change-log-level --reset`

#### Pinging servers

Tuwunel can ping other servers using `!admin debug ping <server>`. This takes
a server name and goes through the server discovery process and queries
`/_matrix/federation/v1/version`. Errors are outputted.

While it does measure the latency of the request, it is not indicative of
server performance on either side as that endpoint is completely unauthenticated
and simply fetches a string on a static JSON endpoint. It is very low cost both
bandwidth and computationally.

#### Allocator memory stats

When using jemalloc with jemallocator's `stats` feature (`--enable-stats`), you
can see Tuwunel's high-level allocator stats by using
`!admin server memory-usage` at the bottom.

If you are a developer, you can also view the raw jemalloc statistics with
`!admin debug memory-stats`. Please note that this output is extremely large
which may only be visible in the Tuwunel console CLI due to PDU size limits,
and is not easy for non-developers to understand.

[unbound-tuning]: https://unbound.docs.nlnetlabs.nl/en/latest/topics/core/performance.html
[unbound-arch]: https://wiki.archlinux.org/title/Unbound
