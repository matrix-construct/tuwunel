# URL previews

When a client asks for a preview of a link, the server fetches the page,
reads its OpenGraph tags, and returns the title, description, and image it
found. This page covers turning that on, the request the server makes, and
what to do when a site returns nothing useful.

The configuration options themselves are listed in the
[Multimedia and Storage](../media.md#url-previews) chapter.

## Enabling previews

Previews are off until at least one allowlist names something. With
`url_preview_domain_explicit_allowlist`, `url_preview_domain_contains_allowlist`
and `url_preview_url_contains_allowlist` all empty, every request is refused
and the server advertises the `io.element.msc4452.preview_url` capability as
disabled, which well-behaved clients read as "do not ask".

```toml
[global]
url_preview_domain_explicit_allowlist = ["youtube.com", "www.youtube.com", "youtu.be"]
```

Setting any allowlist to `["*"]` allows every URL. That is a large amount of
attack surface: a client can then ask the server to fetch anything the CIDR
denylist does not already block. Prefer naming the domains you want.

## How a preview is produced

1. The URL is checked against the allowlists and the CIDR denylist.
2. The page is fetched with the preview client, sending
   `url_preview_user_agent`.
3. The response body is read up to `url_preview_max_spider_size` and parsed
   for `og:` tags, falling back to `twitter:` card tags and then to the
   document `<title>`.
4. `og:image` is fetched once to measure its dimensions, then staged.
5. The result is cached for `url_preview_cache_ttl` seconds.

Only the first `url_preview_max_spider_size` bytes are parsed, so a page that
puts a large script block ahead of its `<head>` metadata can be cut off
before the tags appear. The parse still succeeds, so the outcome is an empty
preview rather than an error, and clients render a card with nothing in it.
The default is 768 KiB, which covers the sites that bury their metadata
deepest among those worth previewing. Raising it further costs bandwidth on
every page that gets truncated, so treat it as a memory and bandwidth bound
rather than a per-site compatibility knob.

## User agents

Some sites serve their OpenGraph tags only to an agent they recognise as a
link-preview crawler, and serve everyone else a page whose tags sit far past
any reasonable spider budget. YouTube is the most common example.

If a site previews correctly from other chat platforms but not from Tuwunel,
try presenting as one of the crawlers it already knows:

```toml
[global]
url_preview_user_agent = "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)"
```

The match is on the literal token `Discordbot/2.0`, case sensitive, anywhere
in the header. A string that merely mentions the name does not work:

| Value | Recognised |
|---|---|
| `Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)` | yes |
| `Tuwunel/1.8.3 preview Discordbot/2.0` | yes |
| `Tuwunel (like Discordbot)` | no, the version is missing |
| `Tuwunel (like Discordbot)/2.0` | no, the version is outside the parentheses |
| `discordbot/2.0` | no, the token is case sensitive |

`url_preview_media_user_agent` sets the agent for the media files themselves
(`og:image`, `og:video`, `og:audio`, and direct links to media), which some
origins gate differently from their pages. It falls back to
`url_preview_user_agent` when unset.

Both options take effect on `systemctl reload tuwunel`; see
[Reloading Configuration](../deploying/configuration-reload.md).

## YouTube

Tuwunel handles two YouTube specifics without configuration.

When a page fetch yields nothing usable and the link points at `youtube.com`,
`www.youtube.com`, `m.youtube.com`, `music.youtube.com` or `youtu.be`, the
server retries against YouTube's oEmbed endpoint, which answers any agent and
returns the title, channel name, and thumbnail in under a kilobyte. This is
why YouTube links preview on a stock configuration even though the page
itself does not expose its tags.

The channel name lands in `og:description`, since oEmbed carries no
description field of its own.

The endpoint lives on `www.youtube.com`, and the server picks it rather than
finding it named on the page, so it is checked against the allowlists like
any other URL. Allowlisting only `youtu.be` therefore previews the link but
skips the retry; include `www.youtube.com` for the fallback to work.

Requests to those hosts also carry a consent cookie, which suppresses the
interstitial Google serves in place of the page in some regions.

Setting a recognised crawler user agent is still worthwhile: the page carries
the video description, which oEmbed does not, so previews gain a real
description rather than the channel name.

## Refreshing a cached preview

A preview is cached for `url_preview_cache_ttl` seconds, 24 hours by default,
including an empty one. Changing the user agent or the spider size does not
clear what is already stored, so a link that previewed badly keeps doing so
until the entry ages out.

Raising the setting spares origins in a room whose history is read often, at
the cost of serving metadata a page has since changed; lowering it does the
reverse. It costs no unbounded disk either way: previews are metadata living
in a column capped at 128 MiB that drops its oldest entries once full, and a
longer lifetime does not copy any more media into storage.

Each entry carries the lifetime in force when it was written, so a change
applies to previews generated after it, and the column retaining them picks
up a longer lifetime at the next restart.

To refetch one URL immediately and overwrite its cache entry:

```
!admin media preview <url> --no-cache
```

The command prints the preview the server would return, which makes it the
fastest way to tell a fetch problem from a client problem.

## Troubleshooting

Work down this list when a link previews as an empty card.

1. **Is the domain allowlisted?** An unlisted domain is refused before any
   request is made.
2. **Is a stale entry cached?** Run `!admin media preview <url> --no-cache`.
   If that returns a good preview and the client still shows an empty card,
   the cache was the problem and the client will catch up.
3. **Does the site need a crawler user agent?** If `--no-cache` also returns
   an empty result, set `url_preview_user_agent` as above and try again.
4. **Is the page larger than the spider budget?** Raise
   `url_preview_max_spider_size`, reload, and run the `--no-cache` command
   again; if the preview fills in, the budget was the problem. A debug build
   reports this directly, logging a warning that names the option when a
   truncated page yielded nothing, but that warning is compiled out of
   release builds.
5. **Can the server reach the host at all?** A failed fetch returns an error
   rather than an empty preview, and is logged with the status.

## What the server relays

`og:image`, `og:video` and `og:audio` resolve to an `mxc://` URI on this
server rather than the third-party URL, so the third party sees the server's
address rather than the client's.

An image is fetched once while the preview is generated, to measure its
dimensions, and those bytes are staged so the first client download does not
refetch the origin. Video and audio are not fetched at preview time at all:
only the URL is recorded, and the source is fetched on the first request for
that `mxc://` URI, subject to the same address checks as every other outbound
request.

An `og:video` or `og:audio` that declares a non-media type is not relayed.
Those point at an embed player page rather than a file, so handing back an
`mxc://` URI would give the client markup where it expected media.

What the page declared about that video is still reported: `og:video:type`
alongside `og:video:width` and `og:video:height`, whether or not the URL
itself could be relayed. Clients read the type to show a preview as
playable, so the card offers to open the video at its source instead of
appearing to be a still image.

YouTube arrives by both routes. Its `og:video` is an embed page rather
than a video file, so a preview taken from the page reports the type the
page declares. A preview recovered through oEmbed has no page metadata to
report, so for a document oEmbed marks as a video the server supplies the
type itself, `text/html`, describing the player that document embeds. That
value is the server's, not the origin's, and it is the only thing on that
route that tells a client the link is playable.

Because the source is fetched rather than copied, a preview's `mxc://` URI is
only as durable as the URL behind it. If the source changes or expires, later
fetches reflect that, unlike uploaded media.
