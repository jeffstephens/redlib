#!/usr/bin/env python3
"""One-page archive feasibility probe. No app routes, storage, or content output."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import re
import time
import urllib.error
import urllib.parse
import urllib.request

PROVIDERS = {
    "arctic_shift": {
        "posts": "https://arctic-shift.photon-reddit.com/api/posts/search",
        "comments": "https://arctic-shift.photon-reddit.com/api/comments/search",
    },
    "pullpush": {
        "posts": "https://api.pullpush.io/reddit/search/submission/",
        "comments": "https://api.pullpush.io/reddit/search/comment/",
    },
}
MAX_BODY = 2 * 1024 * 1024


def validate(username, limit):
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,32}", username):
        raise ValueError("Invalid Reddit username")
    if not 1 <= limit <= 25:
        raise ValueError("Limit must be between 1 and 25")


def normalize(row, source, kind, username):
    if str(row.get("author", "")).casefold() != username.casefold():
        return None
    id_ = str(row.get("id", ""))
    if not re.fullmatch(r"[A-Za-z0-9]{1,16}", id_):
        return None
    created = row.get("created_utc")
    if not isinstance(created, (int, float)) or isinstance(created, bool):
        return None
    retrieved = row.get("retrieved_on", row.get("retrieved_utc"))
    if not isinstance(retrieved, (int, float)) or isinstance(retrieved, bool):
        retrieved = None
    content = row.get("body" if kind == "comments" else "selftext", "")
    # Deliberately do not emit archived body/title text or download any media.
    return {
        "id": id_, "kind": kind, "author": row["author"],
        "created_utc": created, "archived_at": retrieved,
        "subreddit": row.get("subreddit"), "sources": [source],
        "has_archived_text": bool(content) and content not in ("[deleted]", "[removed]"),
        "content_marker": content if content in ("[deleted]", "[removed]") else None,
        "nsfw": row.get("over_18") if isinstance(row.get("over_18"), bool) else None,
    }


def merge(records):
    merged = {}
    for item in records:
        key = (item["kind"], item["id"])
        previous = merged.get(key)
        if previous is None:
            merged[key] = dict(item)
            continue
        sources = sorted(set(previous["sources"] + item["sources"]))
        # Keep the newest captured metadata; never silently merge body variants.
        if (item["archived_at"] or 0) > (previous["archived_at"] or 0):
            merged[key] = dict(item)
        merged[key]["sources"] = sources
    return sorted(merged.values(), key=lambda item: (item["created_utc"], item["id"]), reverse=True)


def fetch(source, kind, username, limit, before=None):
    params = {"author": username, "limit": limit, "sort": "desc"}
    if before is not None:
        params["before"] = before
    url = PROVIDERS[source][kind] + "?" + urllib.parse.urlencode(params)
    request = urllib.request.Request(url, headers={"User-Agent": "RedlibArchiveSpike/0.1"})
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=12) as response:
            if urllib.parse.urlsplit(response.geturl()).netloc != urllib.parse.urlsplit(url).netloc:
                raise ValueError("Provider redirected to another host")
            payload = response.read(MAX_BODY + 1)
            if len(payload) > MAX_BODY:
                raise ValueError("Archive response exceeds 2 MiB")
            listing = json.loads(payload)
            if not isinstance(listing, dict):
                raise ValueError("Archive returned an invalid listing")
            rows = listing.get("data")
            if not isinstance(rows, list):
                raise ValueError("Archive returned an invalid listing")
            records = [item for row in rows[:limit] if isinstance(row, dict)
                       and (item := normalize(row, source, kind, username)) is not None]
            result = {"provider": source, "status": "ok", "http_status": response.status,
                      "returned_rows": len(rows), "accepted_rows": len(records),
                      "records": records,
                      "oldest_timestamp": min((r["created_utc"] for r in records), default=None),
                      "may_have_more": len(rows) >= limit}
    except urllib.error.HTTPError as error:
        result = {"provider": source, "status": "rate_limited" if error.code == 429 else "unavailable",
                  "http_status": error.code, "retry_after": error.headers.get("Retry-After"), "records": []}
    except (OSError, ValueError, TimeoutError) as error:
        result = {"provider": source, "status": "unavailable", "error": str(error)[:200], "records": []}
    result["seconds"] = round(time.monotonic() - started, 3)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("username")
    parser.add_argument("--kind", choices=("posts", "comments"), default="comments")
    parser.add_argument("--limit", type=int, default=3)
    parser.add_argument("--before", type=int)
    parser.add_argument("--provider", choices=("both", *PROVIDERS), default="both")
    args = parser.parse_args()
    try:
        validate(args.username, args.limit)
        if args.before is not None and args.before < 0:
            raise ValueError("before must be a positive Unix timestamp")
    except ValueError as error:
        parser.error(str(error))
    sources = list(PROVIDERS) if args.provider == "both" else [args.provider]
    with ThreadPoolExecutor(max_workers=2) as executor:
        results = list(executor.map(lambda source: fetch(source, args.kind, args.username, args.limit, args.before), sources))
    records = merge([record for result in results for record in result.pop("records")])
    print(json.dumps({"username": args.username, "kind": args.kind, "records": records,
                      "providers": results, "partial": any(r["status"] != "ok" for r in results),
                      "note": "Archived snapshots, not complete/current Reddit history. Timestamp ties need production pagination handling."}, indent=2))


if __name__ == "__main__":
    main()
