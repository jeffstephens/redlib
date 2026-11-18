//! Recover blocked post searches from Reddit's logged-out web search page,
//! then load post metadata by ID like the feed fallback.
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const MAX_RESULTS: usize = 25;

static POST_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?:data-thingid|post-id|thingid|id)="(t3_[a-z0-9]{1,16})""#).unwrap());
static POST_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="(?:https://www\.reddit\.com)?/r/[A-Za-z0-9_]{1,21}/comments/([a-z0-9]{1,16})/"#).unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<title>(.{0,200}?)</title>").unwrap());

/// Map a blocked `search.json` API path to the equivalent web search page.
fn web_path(path: &str) -> Option<String> {
	let (route, query) = path.split_once('?').unwrap_or((path, ""));
	let sub = if route == "/search.json" {
		None
	} else {
		let sub = route.strip_prefix("/r/")?.strip_suffix("/search.json")?;
		if sub.is_empty() || sub.len() > 21 || !sub.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
			return None;
		}
		Some(sub)
	};
	let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
	// Only the first page of post results is available from the web page.
	if ["after", "before"].iter().any(|key| params.get(*key).is_some_and(|v| !v.is_empty())) {
		return None;
	}
	if params.get("type").is_some_and(|t| !t.is_empty() && t != "link") {
		return None;
	}
	let q = params.get("q").filter(|q| !q.trim().is_empty())?;
	let mut out = url::form_urlencoded::Serializer::new(String::new());
	out.append_pair("q", q).append_pair("type", "posts");
	if let Some(sort) = params.get("sort").filter(|s| matches!(s.as_str(), "relevance" | "hot" | "top" | "new" | "comments")) {
		out.append_pair("sort", sort);
	}
	if let Some(t) = params.get("t").filter(|t| matches!(t.as_str(), "hour" | "day" | "week" | "month" | "year" | "all")) {
		out.append_pair("t", t);
	}
	let restrict = params.get("restrict_sr").is_some_and(|v| matches!(v.as_str(), "on" | "1" | "true"));
	Some(match sub.filter(|_| restrict) {
		Some(sub) => {
			out.append_pair("restrict_sr", "1");
			format!("/r/{sub}/search/?{}", out.finish())
		}
		None => format!("/search/?{}", out.finish()),
	})
}

pub fn supported(path: &str) -> bool {
	web_path(path).is_some()
}

fn page_title(html: &str) -> String {
	TITLE.captures(html).map(|c| c[1].trim().to_owned()).unwrap_or_default()
}

/// Post IDs in page order, skipping ads and duplicates.
fn post_ids(html: &str) -> Vec<String> {
	let mut found: Vec<(usize, String)> = POST_ATTR
		.captures_iter(html)
		.map(|c| (c.get(1).unwrap().start(), c[1].to_owned()))
		.chain(POST_LINK.captures_iter(html).map(|c| (c.get(1).unwrap().start(), format!("t3_{}", &c[1]))))
		.collect();
	found.sort_by_key(|(pos, _)| *pos);
	let mut seen = HashSet::new();
	found.into_iter().map(|(_, id)| id).filter(|id| seen.insert(id.clone())).take(MAX_RESULTS).collect()
}

pub async fn listing(path: &str, quarantine: bool) -> Result<Value, String> {
	let web = web_path(path).ok_or("This search cannot use the web fallback")?;
	let html = match crate::web::search_page(web.clone()).await {
		Ok(html) => html,
		Err(e) => {
			log::warn!("Web search fallback failed for {web}: {e}");
			return Err(format!("Reddit blocked this search, and the web fallback failed: {e}"));
		}
	};
	let ids = post_ids(&html);
	if ids.is_empty() {
		let title = page_title(&html);
		log::warn!("Web search fallback found no posts for {web} ({} bytes, title {title:?})", html.len());
		return Ok(json!({"kind":"Listing", "data":{"children":[], "after":null}}));
	}
	let metadata = tokio::time::timeout(std::time::Duration::from_secs(30), crate::feed::post_metadata(&ids, quarantine))
		.await
		.map_err(|_| "Reddit search metadata timed out")??;
	let listing = crate::feed::ordered_listing(&ids, None, metadata)?;
	let count = listing["data"]["children"].as_array().map_or(0, Vec::len);
	log::info!("Web search fallback for {web}: {count} posts");
	Ok(listing)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_api_search_paths_to_web_pages() {
		assert_eq!(
			web_path("/r/anime/search.json?q=frieren+art&restrict_sr=on&sort=new&t=week&raw_json=1").unwrap(),
			"/r/anime/search/?q=frieren+art&type=posts&sort=new&t=week&restrict_sr=1"
		);
		assert_eq!(web_path("/search.json?q=rust&include_over_18=on").unwrap(), "/search/?q=rust&type=posts");
		assert_eq!(web_path("/r/anime/search.json?q=rust").unwrap(), "/search/?q=rust&type=posts");
		for path in [
			"/search.json?q=",
			"/search.json?q=rust&after=t3_abc",
			"/search.json?q=rust&type=sr",
			"/r/a%2Fb/search.json?q=x",
			"/r/anime/new.json?q=x",
			"/user/x/search.json?q=x",
		] {
			assert!(!supported(path), "{path}");
		}
	}

	#[test]
	fn extracts_post_ids_in_order_without_duplicates() {
		let html = r#"<title> Search results </title>
		<search-telemetry-tracker data-thingid="t3_bbb"><a href="/r/anime/comments/bbb/x/">B</a></search-telemetry-tracker>
		<a href="https://www.reddit.com/r/rust/comments/aaa/y/">A</a>
		<shreddit-post id="t3_ccc"></shreddit-post><div id="t3_bad!"></div>"#;
		assert_eq!(post_ids(html), ["t3_bbb", "t3_aaa", "t3_ccc"]);
		assert_eq!(page_title(html), "Search results");
	}
}
