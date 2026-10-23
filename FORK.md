# About this fork

This fork builds on [evrial/redlib](https://github.com/evrial/redlib), preserving
its Hyper HTTP client and OAuth implementation. It carries a small workaround
for [upstream issue #591](https://github.com/redlib-org/redlib/issues/591): encode
the separator before `comments` in outbound subreddit thread requests. Credit
to BrunoNRS for reporting the workaround; see also upstream PR #592.

Queries and other routes are unchanged. Non-JSON HTTP 403 responses explicitly
report the block. This is a temporary workaround: Reddit can change its request
filtering, and other endpoints may still be blocked.

The TLS connector explicitly advertises HTTP/1.1 through ALPN for both OAuth
and API requests. Controlled Linux-client tests found this avoided a TLS-profile
block without changing request headers, disabling certificate verification, or
capping the maximum TLS version. As with the path workaround, Reddit may change
its filtering again. Actual image testing remains part of release validation.

## Blocked thread fallback

When Reddit returns a non-JSON 403 for a thread, the fork fetches post metadata
from `/by_id` and comments from Reddit's logged-out web fragments. The regular
JSON path remains preferred. The parser/crawler is adapted from
[upstream PR #599](https://github.com/redlib-org/redlib/pull/599), with a separate
Hyper-based web session, sanitized comment HTML, and proxied comment images.

The fallback preserves nested replies, sorting, comment permalinks/context,
search, and existing rendering/preferences. It loads at most 12 extra fragments
with concurrency two and a 25-second expansion budget; remaining replies retain
links. A shared in-memory cookie jar is separate from OAuth and caller cookies.
Requests have time and body limits, follow only same-origin comment-fragment
redirects, and stop on HTTP 429. Captcha responses get one fresh-session retry.

This costs more requests than the JSON endpoint. Large threads initially show
fewer comments, and author flair, awards, or some rich comment embeds may be
missing from the web representation. Frontend changes and further blocking can
break this fallback too. Rust 1.88 or newer is required for the cookie store.

## Container images

Public images are available from
[GHCR](https://github.com/jeffstephens/redlib/pkgs/container/redlib) for
`linux/arm64` and `linux/amd64`. Each successful main-branch build publishes a
unique `main-<timestamp>-<commit>-<run>-<attempt>` tag. Use the complete tagged
image reference and digest from the release workflow summary or package page;
there is no rolling `latest` tag for these builds.

For example, replace `IMAGE_REFERENCE` below with the published reference:

```sh
docker run -d --name redlib --restart unless-stopped \
  -p 127.0.0.1:8080:8080 IMAGE_REFERENCE
```

Open `http://localhost:8080`. See the main [README](README.md) for application
configuration. Choose access controls appropriate to your own installation.
Browser preferences are stored in cookies; the application has no database.

## Checks and maintenance

GitHub Actions checks formatting, Clippy, compiles the full test suite, and runs
offline tests before publishing images. The workflow explicitly lists live
Reddit tests excluded from this deterministic gate. Passing CI does not prove
Reddit access: check listings, several threads, comment permalinks, additional
replies, and proxied media after an upgrade. `/settings` checks only the process.

Keep the previous image reference for rollback. Deployments and upgrades are
managed by each operator; the image workflow only builds and publishes.

Review changes from evrial/redlib and redlib-org/redlib before incorporating
them, keep local patches small, and remove workarounds when equivalent fixes
land upstream. Dependency updates require review as well. Two existing warnings
under current Clippy were fixed by removing a deprecated `core::f64` import
and a redundant formatting borrow.
