//! Recover blocked subreddit feed sorts using web ordering and API metadata.
use base64::{engine::general_purpose::STANDARD, Engine as _};
use cached::proc_macro::cached;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

struct FeedRequest {
	fragment: String,
	limit: usize,
}

impl FeedRequest {
	fn parse(path: &str) -> Option<Self> {
		let (route, query) = path.split_once('?').unwrap_or((path, ""));
		let (subreddit, sort) = route.strip_prefix("/r/")?.split_once('/')?;
		let sort = sort.strip_suffix(".json")?;
		if !matches!(sort, "new" | "rising")
			|| subreddit.is_empty()
			|| subreddit.len() > 21
			|| !subreddit.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
			|| matches!(subreddit.to_ascii_lowercase().as_str(), "all" | "popular")
		{
			return None;
		}
		let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
		if params.get("before").is_some_and(|v| !v.is_empty()) {
			return None;
		}
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("name", subreddit).append_pair("sort", sort);
		if let Some(after) = params.get("after").filter(|v| !v.is_empty()) {
			if !post_id(after) {
				return None;
			}
			query.append_pair("after", &STANDARD.encode(after));
		}
		Some(Self {
			fragment: format!("/svc/shreddit/community-more-posts/{sort}/?{}", query.finish()),
			limit: params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(25).clamp(1, 25),
		})
	}
}

fn post_id(id: &str) -> bool {
	id.strip_prefix("t3_")
		.is_some_and(|id| !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub fn supported(path: &str) -> bool {
	FeedRequest::parse(path).is_some()
}

#[cached(size = 32, time = 30, result = true)]
pub async fn listing(path: String, quarantine: bool) -> Result<Value, String> {
	let request = FeedRequest::parse(&path).ok_or("This feed cannot use the web fallback")?;
	let (mut ids, mut after) = match crate::client::shreddit_fragment(request.fragment.clone(), None).await {
		Ok(html) => parse_fragment(&html)?,
		Err(_) => graphql_ids(&request).await?,
	};
	if ids.len() > request.limit {
		ids.truncate(request.limit);
		after = ids.last().cloned();
	}
	if ids.is_empty() {
		return Ok(json!({"kind":"Listing", "data":{"children":[], "after":after}}));
	}
	let metadata = tokio::time::timeout(std::time::Duration::from_secs(30), post_metadata(&ids, quarantine))
		.await
		.map_err(|_| "Reddit feed metadata timed out")??;
	ordered_listing(&ids, after, metadata)
}

// SubredditFeedElements persisted operation from polak14's upstream-linked
// GraphQL patch (29f18352cf54be4ffe579f773552acd0c45cdd08).
async fn graphql_ids(request: &FeedRequest) -> Result<(Vec<String>, Option<String>), String> {
	let (_, query) = request.fragment.split_once('?').ok_or("Invalid feed request")?;
	let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
	let mut variables = json!({"subredditName":params["name"],"sort":params["sort"].to_uppercase(),"pageSize":request.limit,"includeSubredditInPosts":true});
	if let Some(after) = params.get("after") {
		variables["after"] = after.clone().into();
	}
	let body = json!({"operationName":"SubredditFeedElements","variables":variables,"extensions":{"persistedQuery":{"version":1,"sha256Hash":"97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a"}}});
	let response = crate::gql::query(&body).await?;
	graphql_page(&response)
}

fn graphql_page(response: &Value) -> Result<(Vec<String>, Option<String>), String> {
	let feed = &response["data"]["postFeed"];
	if feed["__typename"] != "Subreddit" {
		return Err("Reddit did not provide the requested subreddit feed".into());
	}
	let elements = &feed["elements"];
	let edges = elements["edges"].as_array().ok_or("Reddit returned an invalid feed")?;
	let mut seen = HashSet::new();
	let ids = edges
		.iter()
		.filter_map(|e| e["node"]["id"].as_str())
		.filter(|id| post_id(id) && seen.insert((*id).to_owned()))
		.take(100)
		.map(str::to_owned)
		.collect();
	let after = if elements["pageInfo"]["hasNextPage"] == false {
		None
	} else {
		elements["pageInfo"]["endCursor"]
			.as_str()
			.and_then(|c| STANDARD.decode(c).ok())
			.and_then(|b| String::from_utf8(b).ok())
			.filter(|id| post_id(id))
	};
	Ok((ids, after))
}

// Limit metadata concurrency across simultaneous feed pages as well as within one page.
static METADATA: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
async fn post_metadata(ids: &[String], quarantine: bool) -> Result<Value, String> {
	let mut posts = Vec::new();
	for batch in ids.chunks(2) {
		let mut tasks = tokio::task::JoinSet::new();
		for id in batch {
			let path = format!("/by_id/{id}.json?raw_json=1");
			tasks.spawn(async move {
				let _permit = METADATA.acquire().await.map_err(|_| "Feed metadata transport stopped")?;
				crate::client::json(path, quarantine).await
			});
		}
		while let Some(result) = tasks.join_next().await {
			let value = result.map_err(|_| "Feed metadata task failed")??;
			let children = value["data"]["children"].as_array().ok_or("Reddit returned invalid feed metadata")?;
			posts.extend(children.iter().cloned());
		}
	}
	Ok(json!({"data":{"children":posts}}))
}

fn parse_fragment(html: &str) -> Result<(Vec<String>, Option<String>), String> {
	let doc = Html::parse_fragment(html);
	// A generic Reddit error/challenge page must not become an empty listing.
	if doc.select(&Selector::parse("shreddit-feed-page-loading, shreddit-post[id]").unwrap()).next().is_none() {
		return Err("Reddit returned an unexpected web feed response".into());
	}
	let mut seen = HashSet::new();
	let ids: Vec<_> = doc
		.select(&Selector::parse("shreddit-post[id]").unwrap())
		.filter(|post| post.value().attr("promoted").is_none())
		.filter_map(|post| post.value().attr("id"))
		.filter(|id| post_id(id) && seen.insert((*id).to_owned()))
		.take(101)
		.map(str::to_owned)
		.collect();
	if ids.len() > 100 {
		return Err("Reddit web feed contains too many posts".into());
	}
	let mut after = None;
	for more in doc.select(&Selector::parse("faceplate-partial[slot=\"load-after\"][src]").unwrap()) {
		let src = more.value().attr("src").unwrap_or_default();
		if !src.starts_with("/svc/shreddit/community-more-posts/") {
			continue;
		}
		let Some((_, query)) = src.split_once('?') else { continue };
		for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
			if key == "after" {
				let decoded = STANDARD.decode(value.as_bytes()).ok().and_then(|v| String::from_utf8(v).ok());
				after = decoded.filter(|id| post_id(id));
			}
		}
	}
	Ok((ids, after))
}

fn ordered_listing(ids: &[String], after: Option<String>, metadata: Value) -> Result<Value, String> {
	let posts = metadata["data"]["children"].as_array().ok_or("Reddit returned invalid feed metadata")?;
	let by_id: HashMap<_, _> = posts.iter().filter_map(|post| post["data"]["name"].as_str().map(|id| (id, post))).collect();
	let children: Vec<_> = ids.iter().filter_map(|id| by_id.get(id.as_str()).copied()).collect();
	if children.is_empty() {
		return Err("Reddit did not provide metadata for the web feed posts".into());
	}
	Ok(json!({"kind":"Listing", "data":{"children":children,"after":after}}))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn graphql_feed_keeps_order_skips_recommendations_and_decodes_cursor() {
		let response = json!({"data":{"postFeed":{"__typename":"Subreddit","elements":{"edges":[{"node":{"id":"t3_b"}},{"node":{"id":"recommendation"}},{"node":{"id":"t3_a"}},{"node":{"id":"t3_b"}}],"pageInfo":{"hasNextPage":true,"endCursor":"dDNfYQ=="}}}}});
		assert_eq!(graphql_page(&response).unwrap(), (vec!["t3_b".into(), "t3_a".into()], Some("t3_a".into())));
		assert!(graphql_page(&json!({"data":{"postFeed":{"__typename":"UnavailableSubreddit"}}})).is_err());
	}

	#[test]
	fn routes_and_cursors_are_bounded_and_preserve_sort() {
		let request = FeedRequest::parse("/r/rust/new.json?after=t3_abc&raw_json=1").unwrap();
		assert_eq!(request.fragment, "/svc/shreddit/community-more-posts/new/?name=rust&sort=new&after=dDNfYWJj");
		assert!(FeedRequest::parse("/r/rust/rising.json").unwrap().fragment.contains("sort=rising"));
		assert_eq!(FeedRequest::parse("/r/rust/new.json?limit=100").unwrap().limit, 25);
		assert_eq!(FeedRequest::parse("/r/rust/new.json?limit=1").unwrap().limit, 1);
		for path in [
			"/r/rust/top.json",
			"/user/example/new.json",
			"/r/all/new.json",
			"/r/rust%2Bpics/new.json",
			"/r/rust/new.json?after=bad",
			"/r/rust/new.json?before=t3_abc",
		] {
			assert!(!supported(path), "{path}");
		}
	}
	#[test]
	fn extracts_real_posts_and_pagination_without_ads_or_duplicates() {
		let html = r#"<shreddit-feed-page-loading></shreddit-feed-page-loading>
		<shreddit-post id="t3_b"></shreddit-post><shreddit-post id="t3_a"></shreddit-post>
		<shreddit-post id="t3_b"></shreddit-post><shreddit-post id="t3_ad" promoted></shreddit-post>
		<shreddit-post id="bad/path"></shreddit-post>
		<faceplate-partial slot="load-after" src="/svc/shreddit/community-more-posts/new/?after=dDNfYQ%3D%3D"></faceplate-partial>"#;
		let (ids, after) = parse_fragment(html).unwrap();
		assert_eq!(ids, ["t3_b", "t3_a"]);
		assert_eq!(after.as_deref(), Some("t3_a"));
		let listing = ordered_listing(
			&ids,
			after,
			json!({"data":{"children":[{"data":{"name":"t3_a","title":"A"}},{"data":{"name":"t3_b","title":"B"}}]}}),
		)
		.unwrap();
		assert_eq!(listing["data"]["children"][0]["data"]["title"], "B");
		assert_eq!(listing["data"]["after"], "t3_a");
	}
	#[test]
	fn accepts_posts_when_reddit_omits_its_feed_loading_marker() {
		assert_eq!(parse_fragment("<shreddit-post id=\"t3_abc\"></shreddit-post>").unwrap(), (vec!["t3_abc".into()], None));
	}
	#[test]
	fn distinguishes_empty_feeds_from_errors_and_missing_metadata() {
		assert!(parse_fragment("<html>You've been blocked</html>").is_err());
		assert_eq!(parse_fragment("<shreddit-feed-page-loading></shreddit-feed-page-loading>").unwrap(), (vec![], None));
		assert!(ordered_listing(&["t3_a".into()], None, json!({"data":{"children":[]}})).is_err());
	}
}
