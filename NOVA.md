# Nova maintenance fork

This fork preserves evrial/redlib for Nova, originally deployed from source
4713eff6f71293161ea3a9eb12cf6bb3a3a3b372. Its parent main differs only by deletion
of the unused scripts/gen-credits.sh. The application still uses evrial's Hyper
client and OAuth implementation.

## Local patch

The outbound JSON client encodes the slash between a subreddit and `comments`
for redlib-org/redlib#591 (reported by BrunoNRS; upstream PR #592). Queries and
other routes are unchanged. This temporary workaround does not fix every
blocked Reddit endpoint. Non-JSON 403 responses explicitly report the block.

Two behavior-preserving compatibility fixes remove a deprecated `core::f64`
module import and a redundant formatting borrow flagged by current Clippy.

## Checks and publication

GitHub Actions checks formatting, Clippy, compiles the whole test suite, and runs
all offline tests before publishing ARM64 and AMD64 images to GHCR. The workflow
lists live Reddit tests excluded from the deterministic gate. Functional checks
against Reddit are required separately; hosted CI passing does not prove Reddit
access. No Kubernetes credentials or deployment commands belong in this repo.

Only main-branch builds publish. Tags include timestamp, commit, run ID, and
attempt; each publication has a distinct tag. The workflow summary records the
full image reference and digest. The GHCR package must be public so Nova can
pull without a registry credential.

## Upgrade and rollback

1. Make a focused patch branch; run checks and review changes in a PR.
2. Merge to main and wait for both architectures to publish.
3. In jeffstephens/nova, update apps/redlib/deployment.yaml to that image and
   OCI index digest; render manifests and open a deployment PR.
4. Merging the Nova PR authorizes Flux to deploy. Check listings, several
   threads, comment permalinks, additional comments, and proxied media over
   the tailnet. /settings probes only check the process.
5. Revert the Nova image change to roll back. There is no application database.

Keep evrial/redlib and redlib-org/redlib as upstream remotes. Review their
changes before merging and remove equivalent local workarounds when practical.
Do not automatically merge upstream or dependency PRs. Re-test the actual
request paths when Reddit changes its block; another workaround is not assured.
