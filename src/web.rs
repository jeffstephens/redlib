//! Bounded, logged-out web requests for comment fragments. No API bearer or
//! caller cookies are forwarded to this separate in-memory session.
use crate::client::CLIENT;
use cookie_store::CookieStore;
use hyper::{body::HttpBody, header, Body, Request};
use std::io::Read;
use std::sync::{LazyLock, Mutex};
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use url::Url;

const BASE: &str = "https://www.reddit.com";
const MAX_BODY: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
static COOKIES: LazyLock<Mutex<CookieStore>> = LazyLock::new(|| Mutex::new(CookieStore::default()));
static REQUESTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

fn fragment_url(path: &str) -> Result<Url, String> {
	if !path.starts_with('/') || path.starts_with("//") {
		return Err("Refusing non-fragment URL".into());
	}
	let url = Url::parse(&format!("{BASE}{path}")).map_err(|_| "Invalid comment fragment URL")?;
	if !allowed_url(&url) {
		return Err("Refusing non-fragment URL".into());
	}
	Ok(url)
}

fn allowed_url(url: &Url) -> bool {
	url.scheme() == "https"
		&& url.host_str() == Some("www.reddit.com")
		&& url.port_or_known_default() == Some(443)
		&& url.username().is_empty()
		&& url.password().is_none()
		&& ["/svc/shreddit/comments/", "/svc/shreddit/more-comments/"]
			.iter()
			.any(|prefix| url.path().starts_with(prefix))
}

pub async fn shreddit_fragment(path: String, cursor: Option<String>) -> Result<String, String> {
	let url = fragment_url(&path)?;
	let _permit = REQUESTS.acquire().await.map_err(|_| "Comment transport stopped")?;
	for attempt in 0..2 {
		let result = timeout(Duration::from_secs(20), fetch(url.clone(), cursor.clone()))
			.await
			.map_err(|_| "Reddit comment request timed out")??;
		if !result.contains("<title>Reddit - Prove your humanity</title>") {
			if !result.contains("shreddit-comment") && !result.contains("faceplate-partial") {
				return Err("Reddit returned an unexpected comment response".into());
			}
			return Ok(result);
		}
		if attempt == 0 {
			*COOKIES.lock().map_err(|_| "Comment session lock failed")? = CookieStore::default();
		}
	}
	Err("Reddit challenged the logged-out comments session; try again later".into())
}

async fn fetch(mut url: Url, mut cursor: Option<String>) -> Result<String, String> {
	for _ in 0..4 {
		let csrf = format!("{:032x}", fastrand::u128(..));
		let stored = COOKIES
			.lock()
			.map_err(|_| "Comment session lock failed")?
			.get_request_values(&url)
			.filter(|(name, _)| *name != "over18" && (cursor.is_none() || *name != "csrf_token"))
			.map(|(name, value)| format!("{name}={value}"))
			.collect::<Vec<_>>()
			.join("; ");
		let mut cookies = format!("over18=1; {stored}");
		let body = if let Some(cursor) = &cursor {
			cookies.push_str(&format!("; csrf_token={csrf}"));
			serde_urlencoded::to_string([("cursor", cursor.as_str()), ("csrf_token", csrf.as_str())]).map_err(|_| "Invalid comment cursor")?
		} else {
			String::new()
		};
		let mut request = Request::builder()
			.method(if cursor.is_some() { "POST" } else { "GET" })
			.uri(url.as_str())
			.header(header::USER_AGENT, USER_AGENT)
			.header(header::ACCEPT, "text/vnd.reddit.partial+html, text/html;q=0.9")
			.header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
			.header(header::ACCEPT_ENCODING, "gzip")
			.header(header::COOKIE, cookies)
			.header(header::REFERER, "https://www.reddit.com/")
			.header("Sec-Fetch-Dest", "empty")
			.header("Sec-Fetch-Mode", "cors")
			.header("Sec-Fetch-Site", "same-origin");
		if cursor.is_some() {
			request = request.header(header::ORIGIN, BASE).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
		}
		let request = request
			.body(if cursor.is_some() { Body::from(body) } else { Body::empty() })
			.map_err(|_| "Invalid comment request")?;
		let mut response = CLIENT.request(request).await.map_err(|error| format!("Couldn't fetch Reddit comments: {error}"))?;
		let received = response
			.headers()
			.get_all(header::SET_COOKIE)
			.iter()
			.filter_map(|value| value.to_str().ok())
			.filter_map(|value| cookie::Cookie::parse(value.to_owned()).ok())
			.collect::<Vec<_>>();
		{
			let mut store = COOKIES.lock().map_err(|_| "Comment session lock failed")?;
			store.store_response_cookies(received.into_iter(), &url);
			if store.iter_any().count() > 128 {
				*store = CookieStore::default();
			}
		}
		let status = response.status();
		if status.is_redirection() {
			let location = response
				.headers()
				.get(header::LOCATION)
				.and_then(|value| value.to_str().ok())
				.ok_or("Missing Reddit redirect")?;
			url = url.join(location).map_err(|_| "Invalid Reddit redirect")?;
			if !allowed_url(&url) {
				return Err("Refusing redirect outside Reddit comment fragments".into());
			}
			if matches!(status.as_u16(), 301..=303) {
				cursor = None;
			}
			continue;
		}
		if status.as_u16() == 429 {
			return Err("Reddit is rate limiting comment requests; try again later".into());
		}
		if !status.is_success() {
			return Err(format!("Reddit returned {status} for web comments"));
		}
		let encoding = response
			.headers()
			.get(header::CONTENT_ENCODING)
			.and_then(|value| value.to_str().ok())
			.unwrap_or("identity")
			.to_owned();
		let mut data = Vec::new();
		while let Some(chunk) = response.body_mut().data().await {
			let chunk = chunk.map_err(|_| "Couldn't read Reddit comments")?;
			if data.len() + chunk.len() > MAX_BODY {
				return Err("Reddit comment response is too large".into());
			}
			data.extend_from_slice(&chunk);
		}
		if encoding == "gzip" {
			let decoder = libflate::gzip::Decoder::new(data.as_slice()).map_err(|_| "Invalid compressed comment response")?;
			let mut decoded = Vec::new();
			decoder.take(MAX_BODY as u64 + 1).read_to_end(&mut decoded).map_err(|_| "Couldn't decompress comments")?;
			if decoded.len() > MAX_BODY {
				return Err("Reddit comment response is too large".into());
			}
			data = decoded;
		} else if encoding != "identity" {
			return Err("Unsupported comment response compression".into());
		}
		return String::from_utf8(data).map_err(|_| "Invalid text in Reddit comments".into());
	}
	Err("Too many Reddit comment redirects".into())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn restricts_fragment_destinations_and_redirects() {
		for path in [
			"/api/v1/me",
			"https://evil.example/svc/shreddit/comments/x",
			"//evil.example/svc/shreddit/comments/x",
			"/svc/shreddit/comments/../../api",
			"/svc/shreddit/comments/%2e%2e/%2e%2e/api",
		] {
			assert!(fragment_url(path).is_err(), "{path}");
		}
		assert!(fragment_url("/svc/shreddit/comments/r/rust/t3_abc?sort=TOP").is_ok());
		assert!(!allowed_url(&Url::parse("https://evil.example/svc/shreddit/comments/x").unwrap()));
		assert!(!allowed_url(&Url::parse("http://www.reddit.com/svc/shreddit/comments/x").unwrap()));
	}
}
