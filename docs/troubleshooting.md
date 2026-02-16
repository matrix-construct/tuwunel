# Troubleshooting Tuwunel

> [!IMPORTANT]
> If you intend on asking for support and you are using Docker, **PLEASE**
> triple validate your issues are **NOT** because you have a misconfiguration in
> your Docker setup. We must remain focused on supporting Tuwunel issues and
> cannot budget our time for generic Docker support. Compose file issues or
> Dockerhub image issues are okay if they are something we can fix.

## Tuwunel and Matrix issues

#### Lost access to admin room

You can reinvite yourself to the admin room through the following methods:
- Use the `--execute "users make_user_admin <username>"` Tuwunel binary
argument once to invite yourself to the admin room on startup
- Use the Tuwunel console/CLI to run the `users make_user_admin` command
- Or specify the `emergency_password` config option to allow you to temporarily
log into the server account (`@conduit`) from a web client

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

There are many causes and varieties of database corruption. There are several
methods for mitigation, each with outcomes ranging from a recovered state down
to a savage state. This guide has been simplified into a set of universal steps
which everyone can follow from the top until they have recovered or reach the
end. The details and implications will be explained within each step.

> [!TIP]
> All command-line `-O` options can be expressed as environment variables or in
> the config file based on your deployment's requirements. Note that
> `--maintenance` is equivalent to configuring `startup_netburst = false` and
> `listening = false`.

> [!IMPORTANT]
> Always create a backup of the database before running any operation. This is
> critical for steps 3 and above.

**0. Start the server with the following options:**

`tuwunel --maintenance -O rocksdb_recovery_mode=0`

This is actually a "control" and not a method of recovery. If the server starts
you either do not have corruption or have deep corruption indicated by very
specific errors from rocksdb citing corruption during runtime. If you are
certain there is deep corruption skip to step 4, otherwise you are finished
without any modifications.

**1. Start the server in Tolerate-Corrupted-Tail-Records mode:**

`tuwunel --maintenance -O rocksdb_recovery_mode=1`

The most common corruption scenario is from a loss of power to the hardware
(not an application crash, though it is still possible). This is remediated
by dropping the most recently written record. It is highly unlikely there will
be any impact on the application from this loss. In the best-case the same data
is often re-requested over the federation or replaced by a client. In the
worst-case clients may need to clear-cache & reload to guarantee correctness.
If the server starts you are finished.

**2. Start the server in Point-In-Time mode:**

`tuwunel --maintenance -O rocksdb_recovery_mode=2`

Similar to the corruption scenario above but for more severe cases. The most
recent records are discarded back to the point where there is no corruption.
It is highly unlikely there will be any impact on the application from this
loss, but it is more likely than above that clients may need to clear-cache
& reload to correctly resynchronize with the server.

**3. Start the server in Skip-Any-Corrupted-Record mode:**

> [!WARNING]
> Salvage mode potentially impacting the application's ability to function.
> We cannot provide support for users who have entered this mode.

`tuwunel --maintenance -O rocksdb_recovery_mode=3`

Similar to the prior corruption scenarios but for the most severe cases.
The database will be inconsistent. It is theoretically possible for the
server to continue functioning without notable issue in the best case, but
it is completely uncertain what the effect of this operation will be. If
the server starts you should immediately export your messages, encryption
keys, etc, in a salvage effort and prepare to reinstall.

**4. Start the server in repair mode.**

> [!WARNING]
> Salvage mode potentially impacting the application's ability to function.
> We cannot provide support for users who have entered this mode.

> [!CAUTION]
> Always create a backup of the database before entering this mode. The repair
> is not configurable and not interactive. It may automatically remove more
> data than anticipated, preventing further salvage efforts.

`tuwunel --maintenance -O rocksdb_repair=true`

For corruption affecting the bulk database tables not covered by any journal.
This will leave the database in an inconsistent and unpredictable state. It
is theoretically possible to continue operating the server depending on which
records were dropped, such as some historical records which are no longer
essential. Nevertheless the impact of this operation is impossible to assess
and a successful recovery should be used to salvage data prior to reinstall.

Once finished, restart the server without `rocksdb_repair`. If no errors
persist, restart the server again without maintenance mode.

**5. Utilize an external repair tool.**

> [!WARNING]
> Salvage mode potentially impacting the application's ability to function.
> We cannot provide support for users who have entered this mode.

```
git clone https://github.com/facebook/rocksdb
cd rocksdb
make -j$(nproc) ldb
./ldb repair --db=/var/lib/tuwunel/ 2>./repair-log.txt
```

For situations when the repair mode in step 4 failed or produced unexpected
results.

## Debugging

Note that users should not really be debugging things. If you find yourself
debugging and find the issue, please let us know and/or how we can fix it.
Various debug commands can be found in `!admin debug`.

#### Debug/Trace log level

Tuwunel builds without debug or trace log levels at compile time by default
for substantial performance gains in CPU usage and improved compile times. If
you need to access debug/trace log levels, you will need to build without the
`release_max_log_level` feature or use our provided release-logging binaries
and images.

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
