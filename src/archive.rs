//! Explicit, bounded access to independent snapshots of public Reddit records.
use crate::{
	server::RequestExt,
	utils::{self, Preferences},
};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cached::proc_macro::cached;
use chrono::DateTime;
use hyper::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
	collections::{HashMap, HashSet},
	sync::{LazyLock, Mutex},
	time::{Duration, Instant},
};

const FETCH_LIMIT: usize = 50;
const PAGE_LIMIT: usize = 25;
static REQUESTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static BACKOFF: LazyLock<Mutex<HashMap<Source, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CLIENT: LazyLock<wreq::Client> = LazyLock::new(|| {
	wreq::Client::builder()
		.redirect(wreq::redirect::Policy::none())
		.timeout(Duration::from_secs(12))
		.build()
		.expect("archive client")
});

pub fn enabled() -> bool {
	crate::config::get_setting("REDLIB_ENABLE_ARCHIVES").as_deref() == Some("on")
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
enum Source {
	Arctic,
	Pullpush,
}
impl Source {
	fn label(self) -> &'static str {
		match self {
			Self::Arctic => "Arctic Shift",
			Self::Pullpush => "PullPush",
		}
	}
	fn endpoint(self, kind: &str) -> &'static str {
		match (self, kind) {
			(Self::Arctic, "posts") => "https://arctic-shift.photon-reddit.com/api/posts/search",
			(Self::Arctic, _) => "https://arctic-shift.photon-reddit.com/api/comments/search",
			(Self::Pullpush, "posts") => "https://api.pullpush.io/reddit/search/submission/",
			(Self::Pullpush, _) => "https://api.pullpush.io/reddit/search/comment/",
		}
	}
}
#[derive(Clone, Default, Hash, Eq, PartialEq, Serialize, Deserialize)]
struct Cursor {
	before: Option<i64>,
	seen: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Paging {
	user: String,
	kind: String,
	source: String,
	arctic: Option<Cursor>,
	pullpush: Option<Cursor>,
}
#[derive(Clone)]
struct Record {
	id: String,
	title: String,
	body: String,
	subreddit: String,
	created: i64,
	captured: Option<i64>,
	created_label: String,
	captured_label: String,
	link: String,
	sources: Vec<String>,
	marker: String,
	nsfw: Option<bool>,
	hidden: bool,
	conflict: bool,
}
#[derive(Clone)]
struct ProviderPage {
	records: Vec<Record>,
	next: Option<Cursor>,
	stalled: bool,
}
struct Status {
	name: String,
	message: String,
	ok: bool,
}
#[derive(Template)]
#[template(path = "archive.html")]
struct ArchiveTemplate {
	username: String,
	kind: String,
	source: String,
	loaded: bool,
	unrated: bool,
	records: Vec<Record>,
	statuses: Vec<Status>,
	next: String,
	filtered: usize,
	prefs: Preferences,
	url: String,
}
fn valid_name(name: &str) -> bool {
	!name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn valid_id(id: &str) -> bool {
	!id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn timestamp(value: &Value) -> Option<i64> {
	value.as_i64().filter(|v| *v > 0 && *v < 253_402_300_799)
}
fn date(value: i64) -> String {
	DateTime::from_timestamp(value, 0)
		.map(|v| v.format("%Y-%m-%d %H:%M UTC").to_string())
		.unwrap_or_else(|| "Unavailable".into())
}
fn bounded(value: &str, max: usize) -> String {
	let mut end = value.len().min(max);
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	value[..end].to_owned()
}
fn normalize(row: &Value, source: Source, kind: &str, user: &str) -> Option<Record> {
	if !row["author"].as_str()?.eq_ignore_ascii_case(user) {
		return None;
	}
	let id = row["id"].as_str()?.to_owned();
	if !valid_id(&id) {
		return None;
	}
	let created = timestamp(&row["created_utc"])?;
	let captured = timestamp(&row["retrieved_on"]).or_else(|| timestamp(&row["retrieved_utc"]));
	let subreddit = bounded(row["subreddit"].as_str().unwrap_or("unknown"), 64);
	let sub_valid = !subreddit.is_empty() && subreddit.len() <= 32 && subreddit.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
	let text = row[if kind == "comments" { "body" } else { "selftext" }].as_str().unwrap_or_default();
	let marker = if matches!(text, "[deleted]" | "[removed]") {
		text.to_owned()
	} else if !row["removed_by_category"].is_null() {
		"Removed in this snapshot".into()
	} else {
		String::new()
	};
	let link = if !sub_valid {
		String::new()
	} else if kind == "posts" {
		format!("/r/{subreddit}/comments/{id}/archived/")
	} else {
		row["link_id"]
			.as_str()
			.and_then(|v| v.strip_prefix("t3_"))
			.filter(|v| valid_id(v))
			.map(|post| format!("/r/{subreddit}/comments/{post}/archived/{id}/"))
			.unwrap_or_default()
	};
	Some(Record {
		id,
		title: bounded(row["title"].as_str().unwrap_or("Archived comment"), 1024),
		body: bounded(text, 8192),
		subreddit,
		created,
		captured,
		created_label: date(created),
		captured_label: captured.map(date).unwrap_or_else(|| "Capture time unavailable".into()),
		link,
		sources: vec![source.label().into()],
		marker,
		nsfw: row["over_18"].as_bool(),
		hidden: false,
		conflict: false,
	})
}
fn window(mut records: Vec<Record>, cursor: &Cursor, raw_count: usize) -> ProviderPage {
	records.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
	let mut ids = HashSet::new();
	records.retain(|r| ids.insert(r.id.clone()) && !(cursor.before == Some(r.created + 1) && cursor.seen.contains(&r.id)));
	let more = raw_count >= FETCH_LIMIT || records.len() > PAGE_LIMIT;
	records.truncate(PAGE_LIMIT);
	let mut stalled = more && records.is_empty();
	let next = if more && !records.is_empty() {
		let oldest = records.last().unwrap().created;
		let mut seen = if cursor.before == Some(oldest + 1) { cursor.seen.clone() } else { Vec::new() };
		for record in &records {
			if record.created == oldest && !seen.contains(&record.id) {
				seen.push(record.id.clone());
			}
		}
		if seen.len() > 100 {
			stalled = true;
			None
		} else {
			Some(Cursor { before: Some(oldest + 1), seen })
		}
	} else {
		None
	};
	ProviderPage { records, next, stalled }
}
fn merge(pages: Vec<Record>) -> Vec<Record> {
	let mut records: HashMap<String, Record> = HashMap::new();
	for item in pages {
		if let Some(existing) = records.get_mut(&item.id) {
			let conflict = existing.conflict || existing.body != item.body || existing.title != item.title || existing.marker != item.marker;
			let mut sources = existing.sources.clone();
			for source in &item.sources {
				if !sources.contains(source) {
					sources.push(source.clone());
				}
			}
			if item.captured.unwrap_or(0) > existing.captured.unwrap_or(0) {
				*existing = item;
			}
			existing.sources = sources;
			existing.conflict = conflict;
		} else {
			records.insert(item.id.clone(), item);
		}
	}
	let mut result: Vec<_> = records.into_values().collect();
	result.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
	result
}
#[cached(size = 64, time = 60, result = true)]
async fn fetch(user: String, kind: String, source: Source, cursor: Cursor) -> Result<ProviderPage, String> {
	let started = Instant::now();
	let result = tokio::time::timeout(Duration::from_secs(15), fetch_inner(&user, &kind, source, &cursor))
		.await
		.unwrap_or_else(|_| Err("Provider timed out; try again later".to_string()));
	let elapsed = started.elapsed().as_millis();
	match &result {
		Ok(page) => log::info!(
			"Archive {} {kind} for u/{user} (before={:?}): {} records in {elapsed}ms{}",
			source.label(),
			cursor.before,
			page.records.len(),
			if page.stalled { ", paging stalled" } else { "" }
		),
		Err(e) => log::warn!("Archive {} {kind} for u/{user} (before={:?}) failed after {elapsed}ms: {e}", source.label(), cursor.before),
	}
	result
}
/// Reads at most `max` bytes of a response body.
async fn read_capped(response: wreq::Response, max: usize) -> Result<Vec<u8>, &'static str> {
	use futures_lite::StreamExt;
	let mut stream = std::pin::pin!(response.bytes_stream());
	let mut bytes = Vec::new();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.map_err(|_| "Provider response failed")?;
		if bytes.len() + chunk.len() > max {
			return Err("Provider response is too large");
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok(bytes)
}
/// Extracts a short, loggable reason from a provider error body (JSON `error` field or raw text).
fn error_detail(body: &[u8]) -> String {
	let text = String::from_utf8_lossy(body);
	let detail = serde_json::from_str::<Value>(&text)
		.ok()
		.and_then(|v| v["error"].as_str().map(str::to_owned))
		.unwrap_or_else(|| text.split_whitespace().collect::<Vec<_>>().join(" "));
	bounded(&detail, 200)
}
/// User-facing message for a non-success provider status.
fn status_message(status: u16, detail: &str) -> String {
	match status {
		422 if detail.to_ascii_lowercase().contains("timeout") => format!("Provider query timed out (HTTP {status}); try again in a minute"),
		520..=530 => format!("Provider is down (HTTP {status}); this is an outage on their side"),
		_ => format!("Unavailable (HTTP {status})"),
	}
}
async fn fetch_inner(user: &str, kind: &str, source: Source, cursor: &Cursor) -> Result<ProviderPage, String> {
	if let Some(until) = BACKOFF.lock().map_err(|_| "Provider is unavailable")?.get(&source).copied() {
		if until > Instant::now() {
			return Err(format!("Rate limited; retry in {} seconds", until.saturating_duration_since(Instant::now()).as_secs() + 1));
		}
	}
	let _permit = REQUESTS.acquire().await.map_err(|_| "Provider is unavailable")?;
	let query = {
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("author", user).append_pair("limit", &FETCH_LIMIT.to_string()).append_pair("sort", "desc");
		if let Some(before) = cursor.before {
			query.append_pair("before", &before.to_string());
		}
		query.finish()
	};
	let response = CLIENT
		.get(format!("{}?{}", source.endpoint(kind), query))
		.header("User-Agent", "RedlibArchive/0.1")
		.send()
		.await
		.map_err(|e| {
			log::warn!("Archive {} {kind} request error: {e}", source.label());
			"Provider connection failed; try again later"
		})?;
	if response.status().as_u16() == 429 {
		let seconds = response
			.headers()
			.get("retry-after")
			.and_then(|v| v.to_str().ok())
			.and_then(|v| v.parse::<u64>().ok())
			.unwrap_or(60)
			.clamp(1, 3600);
		BACKOFF
			.lock()
			.map_err(|_| "Provider is unavailable")?
			.insert(source, Instant::now() + Duration::from_secs(seconds));
		log::warn!("Archive {} rate limited; backing off {seconds}s", source.label());
		return Err(format!("Rate limited; retry in {seconds} seconds"));
	}
	let status = response.status().as_u16();
	if !response.status().is_success() {
		let detail = read_capped(response, 4096).await.map(|b| error_detail(&b)).unwrap_or_default();
		log::warn!("Archive {} {kind} returned HTTP {status}: {detail}", source.label());
		return Err(status_message(status, &detail));
	}
	let bytes = read_capped(response, 2 * 1024 * 1024).await?;
	let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
		log::warn!("Archive {} {kind} returned invalid JSON: {}", source.label(), error_detail(&bytes));
		"Provider returned an invalid response"
	})?;
	let rows = value["data"].as_array().ok_or("Provider returned an invalid listing")?;
	if rows.len() > FETCH_LIMIT {
		return Err("Provider returned an oversized listing".into());
	}
	let records: Vec<_> = rows.iter().filter_map(|row| normalize(row, source, kind, user)).collect();
	if rows.len() != records.len() {
		log::debug!(
			"Archive {} {kind}: dropped {} of {} rows during normalization",
			source.label(),
			rows.len() - records.len(),
			rows.len()
		);
	}
	if !rows.is_empty() && records.is_empty() {
		return Err("Provider returned no matching readable records".into());
	}
	Ok(window(records, cursor, rows.len()))
}
fn parse_paging(encoded: Option<String>, user: &str, kind: &str, source: &str) -> Result<Paging, String> {
	let Some(encoded) = encoded else {
		return Ok(Paging {
			user: user.to_lowercase(),
			kind: kind.into(),
			source: source.into(),
			arctic: Some(Cursor::default()),
			pullpush: Some(Cursor::default()),
		});
	};
	if encoded.len() > 8192 {
		return Err("Invalid archive cursor".into());
	}
	let data = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| "Invalid archive cursor")?;
	let page: Paging = serde_json::from_slice(&data).map_err(|_| "Invalid archive cursor")?;
	if page.user != user.to_lowercase() || page.kind != kind || page.source != source {
		return Err("Archive cursor belongs to another search".into());
	}
	for cursor in [&page.arctic, &page.pullpush].into_iter().flatten() {
		if cursor.seen.len() > 100 || cursor.seen.iter().any(|id| !valid_id(id)) || cursor.before.is_some_and(|t| t <= 0 || t >= 253_402_300_799) {
			return Err("Invalid archive cursor".into());
		}
	}
	Ok(page)
}
fn excluded(user: &str) -> bool {
	std::env::var("REDLIB_ARCHIVE_EXCLUDED_USERS")
		.unwrap_or_default()
		.split([',', '+'])
		.any(|name| name.trim().trim_start_matches("u/").eq_ignore_ascii_case(user))
}
fn bad_request(message: &str) -> Response<Body> {
	Response::builder()
		.status(400)
		.header("content-type", "text/plain; charset=utf-8")
		.body(Body::from(message.to_owned()))
		.unwrap()
}

fn filter_records(records: &mut Vec<Record>, filters: &HashSet<String>, kind: &str, show_nsfw: bool, unrated: bool, sfw_only: bool) -> usize {
	let before = records.len();
	let allow_nsfw = show_nsfw && !sfw_only;
	records.retain(|r| !filters.iter().any(|f| f.eq_ignore_ascii_case(&r.subreddit)) && (r.nsfw != Some(true) || allow_nsfw));
	for record in records.iter_mut() {
		if record.nsfw.is_none() && !allow_nsfw && (!unrated || sfw_only) {
			record.body.clear();
			if kind == "posts" {
				record.title = "Archived post (unrated)".into();
			}
			record.hidden = true;
		}
	}
	before - records.len()
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	if !enabled() {
		return utils::error(req, "Archived history is disabled on this instance.").await;
	}
	let username = req.param("name").unwrap_or_default();
	if !valid_name(&username) {
		return Ok(bad_request("Invalid Reddit username"));
	}
	let url = req.uri().to_string();
	let kind = utils::param(&url, "kind").unwrap_or_else(|| "posts".into());
	let source = utils::param(&url, "source").unwrap_or_else(|| "both".into());
	if !matches!(kind.as_str(), "posts" | "comments") || !matches!(source.as_str(), "both" | "arctic" | "pullpush") {
		return Ok(bad_request("Invalid archive search options"));
	}
	let loaded = utils::param(&url, "load").as_deref() == Some("1");
	let unrated = utils::param(&url, "unrated").as_deref() == Some("1");
	let filters = utils::get_filters(&req);
	let blocked = excluded(&username) || filters.iter().any(|f| f.eq_ignore_ascii_case(&format!("u_{username}")));
	let mut paging = match parse_paging(utils::param(&url, "after"), &username, &kind, &source) {
		Ok(p) => p,
		Err(e) => return Ok(bad_request(&e)),
	};
	let mut statuses = Vec::new();
	let mut records = Vec::new();
	let mut has_next = false;
	if loaded && !blocked {
		let a = async {
			if matches!(source.as_str(), "both" | "arctic") {
				if let Some(c) = paging.arctic.clone() {
					return Some(fetch(username.clone(), kind.clone(), Source::Arctic, c).await);
				}
			}
			None
		};
		let b = async {
			if matches!(source.as_str(), "both" | "pullpush") {
				if let Some(c) = paging.pullpush.clone() {
					return Some(fetch(username.clone(), kind.clone(), Source::Pullpush, c).await);
				}
			}
			None
		};
		let (a, b) = tokio::join!(a, b);
		for (provider, result) in [(Source::Arctic, a), (Source::Pullpush, b)] {
			if !matches!((source.as_str(), provider), ("both", _) | ("arctic", Source::Arctic) | ("pullpush", Source::Pullpush)) {
				continue;
			}
			match result {
				Some(Ok(page)) => {
					has_next |= page.next.is_some();
					let message = if page.stalled {
						"Paging stopped at a timestamp boundary; some records may be missing".into()
					} else {
						format!("{} records on this page", page.records.len())
					};
					statuses.push(Status {
						name: provider.label().into(),
						message,
						ok: true,
					});
					records.extend(page.records);
					match provider {
						Source::Arctic => paging.arctic = page.next,
						Source::Pullpush => paging.pullpush = page.next,
					};
				}
				Some(Err(message)) => statuses.push(Status {
					name: provider.label().into(),
					message,
					ok: false,
				}),
				None => statuses.push(Status {
					name: provider.label().into(),
					message: "End of available results".into(),
					ok: true,
				}),
			}
		}
	}
	let next = if has_next {
		let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&paging).map_err(|_| "Couldn't create archive cursor")?);
		let mut q = url::form_urlencoded::Serializer::new(String::new());
		q.append_pair("load", "1")
			.append_pair("kind", &kind)
			.append_pair("source", &source)
			.append_pair("after", &encoded);
		if unrated {
			q.append_pair("unrated", "1");
		}
		format!("?{}", q.finish())
	} else {
		String::new()
	};
	let mut records = merge(records);
	let allow_nsfw = utils::setting(&req, "show_nsfw") == "on";
	let filtered = filter_records(&mut records, &filters, &kind, allow_nsfw, unrated, utils::sfw_only());
	let reddit_url = format!("/user/{username}");
	let view = ArchiveTemplate {
		username,
		kind,
		source,
		loaded,
		unrated,
		records,
		statuses,
		next,
		filtered,
		prefs: Preferences::new(&req),
		url: reddit_url,
	};
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "text/html; charset=utf-8")
			.header("cache-control", "private, no-store")
			.header("x-robots-tag", "noindex, nofollow")
			.body(Body::from(view.render().map_err(|_| "Couldn't render archived history")?))
			.unwrap(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	fn record(id: &str, created: i64) -> Record {
		normalize(
			&json!({"id":id,"author":"example","created_utc":created,"retrieved_on":created+1,"subreddit":"rust","selftext":"hello","title":"Title","over_18":false}),
			Source::Arctic,
			"posts",
			"example",
		)
		.unwrap()
	}
	#[test]
	fn filters_and_sfw_rules_apply_to_archive_text() {
		let make = |rating: Value| {
			normalize(
				&serde_json::json!({"id":"abc","author":"demo","created_utc":100,"subreddit":"test","title":"secret title","selftext":"secret body","over_18":rating}),
				Source::Arctic,
				"posts",
				"demo",
			)
			.unwrap()
		};
		let mut records = vec![make(Value::Null), make(Value::Bool(true))];
		assert_eq!(filter_records(&mut records, &HashSet::new(), "posts", false, false, false), 1);
		assert!(records[0].hidden && records[0].body.is_empty());
		assert_eq!(records[0].title, "Archived post (unrated)");
		let mut records = vec![make(Value::Null)];
		filter_records(&mut records, &HashSet::new(), "posts", false, true, false);
		assert_eq!(records[0].body, "secret body");
		let mut records = vec![make(Value::Null), make(Value::Bool(true))];
		assert_eq!(filter_records(&mut records, &HashSet::new(), "posts", true, true, true), 1);
		assert!(records[0].hidden);
		let mut records = vec![make(Value::Bool(false))];
		assert_eq!(filter_records(&mut records, &HashSet::from(["TEST".into()]), "posts", false, true, false), 1);
		assert!(records.is_empty());
	}

	#[test]
	fn author_identity_and_links_are_constrained() {
		assert!(normalize(&json!({"id":"abc","author":"other","created_utc":100}), Source::Arctic, "posts", "example").is_none());
		assert!(normalize(&json!({"id":"../bad","author":"example","created_utc":100}), Source::Arctic, "posts", "example").is_none());
		let mut value = json!({"id":"abc","author":"EXAMPLE","created_utc":100,"subreddit":"rust","permalink":"https://evil.invalid/"});
		let post = normalize(&value, Source::Arctic, "posts", "example").unwrap();
		assert_eq!(post.link, "/r/rust/comments/abc/archived/");
		value["subreddit"] = "../evil".into();
		assert!(normalize(&value, Source::Arctic, "posts", "example").unwrap().link.is_empty());
	}
	#[test]
	fn metadata_keeps_unknown_ratings_and_tombstones() {
		let r = normalize(
			&json!({"id":"abc","author":"example","created_utc":100,"body":"[deleted]"}),
			Source::Arctic,
			"comments",
			"example",
		)
		.unwrap();
		assert_eq!(r.nsfw, None);
		assert_eq!(r.marker, "[deleted]");
		assert!(r.captured.is_none());
		assert!(r.link.is_empty());
	}
	#[test]
	fn snapshot_conflicts_are_visible_and_sources_are_preserved() {
		let a = record("abc", 100);
		let mut b = a.clone();
		b.sources = vec!["PullPush".into()];
		b.captured = Some(200);
		b.body = "Different".into();
		let items = merge(vec![a, b]);
		assert_eq!(items.len(), 1);
		assert!(items[0].conflict);
		assert_eq!(items[0].sources.len(), 2);
		assert_eq!(items[0].body, "Different");
	}
	#[test]
	fn paging_overlaps_timestamp_ties_without_repeating_ids() {
		let rows: Vec<_> = (0..50).map(|i| record(&format!("id{i:02}"), 100)).collect();
		let first = window(rows.clone(), &Cursor::default(), 50);
		assert_eq!(first.records.len(), 25);
		let c = first.next.unwrap();
		assert_eq!(c.before, Some(101));
		let second = window(rows.clone(), &c, 50);
		assert_eq!(second.records.len(), 25);
		assert!(!second.records.iter().any(|r| first.records.iter().any(|old| old.id == r.id)));
		let third = window(rows, &second.next.unwrap(), 50);
		assert!(third.stalled);
		assert!(third.next.is_none());
	}
	#[test]
	fn final_short_page_is_terminal() {
		assert!(window(vec![record("abc", 100)], &Cursor::default(), 1).next.is_none());
	}
	#[test]
	fn cursor_cannot_be_reused_for_another_search() {
		let paging = Paging {
			user: "example".into(),
			kind: "posts".into(),
			source: "both".into(),
			arctic: Some(Cursor::default()),
			pullpush: None,
		};
		let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&paging).unwrap());
		assert!(parse_paging(Some(encoded.clone()), "example", "posts", "both").is_ok());
		assert!(parse_paging(Some(encoded.clone()), "other", "posts", "both").is_err());
		assert!(parse_paging(Some(encoded), "example", "comments", "both").is_err());
		assert!(parse_paging(Some("x".repeat(9000)), "example", "posts", "both").is_err());
	}
	#[test]
	fn archive_text_is_escaped_and_never_embeds_media() {
		let mut item = record("abc", 100);
		item.title = "<script>alert(1)</script>".into();
		item.body = "<img src=\"https://evil.invalid/track\">".into();
		let view = ArchiveTemplate {
			username: "example".into(),
			kind: "posts".into(),
			source: "both".into(),
			loaded: true,
			unrated: false,
			records: vec![item],
			statuses: vec![Status {
				name: "PullPush".into(),
				message: "Unavailable (HTTP 526)".into(),
				ok: false,
			}],
			next: String::new(),
			filtered: 0,
			prefs: Preferences::default(),
			url: "/user/example".into(),
		};
		let html = view.render().unwrap();
		assert!(!html.contains("<script>alert(1)</script>"));
		assert!(!html.contains("<img src=\"https://evil.invalid/track\">"));
		assert!(html.contains("HTTP 526"));
		assert!(html.contains("Arctic Shift"));
		assert!(html.contains("Archived snapshot"));
	}
	#[test]
	fn provider_errors_are_explained() {
		let detail = error_detail(br#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#);
		assert_eq!(detail, "Timeout. Maybe slow down a bit");
		assert!(status_message(422, &detail).contains("timed out"));
		assert_eq!(error_detail(b"error code: 526\n"), "error code: 526");
		assert!(status_message(526, "").contains("outage"));
		assert_eq!(status_message(500, ""), "Unavailable (HTTP 500)");
	}
	#[test]
	fn text_bounds_preserve_unicode() {
		assert_eq!(bounded("ééé", 5), "éé");
		assert!(!valid_name("../bad"));
	}
}
