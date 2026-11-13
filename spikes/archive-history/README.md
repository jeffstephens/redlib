# Archived user-history spike

Status: feasibility prototype only. No Redlib routes, templates, dependencies,
configuration, image policy or production deployment change. Results below were
observed on 2026-11-13 UTC (2026-11-12 Mountain).

## Finding

Feasible for *previously captured public material*, including material absent
from Reddit's current profile listing. This does not unlock private posts or
recover records the archives never indexed, and it cannot establish that an
empty Reddit listing is caused by a hidden-history preference.

Rosint is an MIT-licensed browser frontend, not another archive database or a
public history-proxy API. Its [README](https://github.com/zuxu4n/Rosint) and
[privacy/changelog](https://www.rosint.dev/changelog) explain that user searches
go directly to Arctic Shift and PullPush and are merged/deduplicated. We
inspected the deployed client as well as the published source documentation.

| Option | Result |
| --- | --- |
| Link to `https://rosint.dev/?u=<name>` | Smallest UI change; inherits Rosint's availability, bot checks and own removal rules; takes the reader to another site. |
| Embed/scrape Rosint | Poor integration boundary: its frontend is not a public proxy API, profile links may require a browser challenge, and its restriction service is configured for its own origin. |
| Talk to archive providers directly | Technically viable; gives us source labels and consistent Redlib rendering, but requires our own paging, outage handling, explicit activation and source-removal handling. |
| Maintain an independent archive/index | Outside this spike; materially changes the instance's stateless storage and operational footprint. |

## Live probe

The documented API example account `PresidentObama` was used with a limit of
three. We did not query a chosen hidden account, download media, or commit raw
API responses or archived text.

| Source | Posts | Comments | Observation |
| --- | --- | --- | --- |
| Arctic Shift | HTTP 200, two records | HTTP 200, three records | Exact author match; post/comment IDs, timestamps, body/title fields and captured-at metadata are available. Roughly 1–1.5 seconds per request. |
| PullPush | HTTP 526 | HTTP 526 | Unavailable during this check; no retries or disabled certificate checks. |

The executable prototype still returns Arctic Shift's results when PullPush
fails, and reports `partial: true` plus per-provider errors. It normalizes
metadata, distinguishes post/comment IDs, deduplicates with source provenance,
and selects the newest captured metadata. It deliberately emits no body/title
text, never downloads images, writes no data files and performs no analytics.

```sh
python3 -m unittest discover -s spikes/archive-history -p 'test_*.py'
python3 spikes/archive-history/probe.py PresidentObama --kind comments --limit 3
python3 spikes/archive-history/probe.py PresidentObama --kind posts --provider arctic_shift --limit 3
```

Each request has a 12-second timeout and 2 MiB read limit. Page size is capped at
25 and provider fanout at two. A 429 is returned as a rate-limit state with its
Retry-After value; there is no retry loop. Running the probe is the explicit
user action that sends the account name to the selected providers.

The prototype is not a production adapter: cancellation, a process-wide request
budget, complete timestamp-tie handling, retention/exclusion refresh, response
schema evolution and UI rendering remain to be implemented.

## Integration proposal

Add a distinct **Archived history** action on user pages, activated explicitly.
Keep Reddit profile metadata/live history and archived results separate. Do not
silently fill an empty live history with archive content or label snapshots as
current Reddit data. Before activation, identify the third-party sources that
will receive the username; do not send browser cookies or Reddit tokens.

Use a small server-side provider interface returning records, provider cursors,
availability and source timestamps. Fixed provider hosts, verified TLS, bounded
redirects/timeouts/body sizes and at most two requests in flight should match
the current client's constraints. A bounded in-memory cache is sufficient; no
PVC or database is needed. Do not log response bodies or retain a lookup list.

Render source badges and captured-at dates, preserve deleted/removed markers,
and distinguish provider outage, genuine zero results and partial coverage.
Sanitize text through the existing rendering path. Never auto-load third-party
media, and apply existing NSFW/filter settings; comment NSFW status may be
unknown and needs explicit handling. Keep disagreeing body snapshots separate
rather than manufacturing one combined version.

Arctic Shift supports date-based `before`/`after`; PullPush follows a similar
search shape. Each provider needs its own cursor. Timestamp-only boundaries can
lose records tied at the same second. A real adapter needs overlap plus ID
deduplication and an explicit incomplete/stalled-page state; repeatedly
subtracting one second is insufficient. The CLI's optional `--before` is only
a one-page probe and makes no completeness guarantee.

Source opt-outs and unavailable records must remain unavailable. Rosint has
additional static/dynamic restrictions: a direct-provider integration does not
automatically inherit that separate policy. Do not impersonate Rosint's origin
to access its restriction service. A link-out preserves its own rules; an
independent integration needs a documented exclusion/removal policy before
shipping. Current archive operators provide their own removal mechanisms.

## Should this remain a fork?

Official `redlib-org/redlib` main is still `ba116c1186f6` (2026-10-21), without
the GraphQL comment/profile implementation we now rely on. Switching back to it
would discard working endpoint repairs. Polak14's default branch is
`29f18352cf54` (2026-10-28): it already covers comments, feeds, profiles, user
history, search and more via GraphQL, but has not been adopted/tested here as a
complete replacement.

There is a short-term reason to retain a **thin** maintenance fork: verified
endpoint coverage, bounded/sanitized clients, stable preference/media behavior,
and native immutable AMD64/ARM64 publication consumed by deployment automation.
There is less reason to maintain a separate Reddit API implementation forever.
Evaluate rebasing onto the broader GraphQL fork or upstream once equivalent
changes land, retaining a small reviewed delta. Test saved preferences, media,
NSFW/filter behavior and the deployment tag contract before switching bases.

An optional archive provider feature would be an intentional product difference
and a clearer reason to keep a fork. Recommendation: keep this prototype out of
production for now; ship the profile fix separately, then choose between a
simple Rosint link-out and an explicit direct-provider archive tab.

## Primary references

- [Rosint source and integration/removal architecture](https://github.com/zuxu4n/Rosint)
- [Rosint privacy and operational changes](https://www.rosint.dev/changelog)
- [Arctic Shift API documentation](https://github.com/ArthurHeitmann/arctic_shift/blob/master/api/README.md)
- [Arctic Shift project and source-removal links](https://github.com/ArthurHeitmann/arctic_shift)
- [PullPush](https://pullpush.io/) and [source removal](https://removals.pullpush.io/)
- [Official Redlib source snapshot](https://github.com/redlib-org/redlib/commit/ba116c1186f6b0f9d45f0ae50c827e13eb2f5ea2)
- [Broader GraphQL implementation](https://github.com/polak14/redlib/commit/29f18352cf54be4ffe579f773552acd0c45cdd08)
