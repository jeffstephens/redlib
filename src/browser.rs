//! Browser-compatible transport shared by GraphQL and web fragments.
use std::sync::LazyLock;
use std::time::Duration;
use wreq_util::Emulation;

pub static CLIENT: LazyLock<wreq::Client> = LazyLock::new(|| {
	wreq::Client::builder()
		.emulation(Emulation::Chrome147)
		.redirect(wreq::redirect::Policy::none())
		.timeout(Duration::from_secs(20))
		.build()
		.expect("browser TLS client")
});

pub async fn body(response: wreq::Response) -> Result<Vec<u8>, String> {
	const LIMIT: usize = 8 * 1024 * 1024;
	let mut data = Vec::new();
	let mut stream = std::pin::pin!(response.bytes_stream());
	use futures_lite::StreamExt;
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.map_err(|_| "Couldn't read Reddit response")?;
		if data.len() + chunk.len() > LIMIT {
			return Err("Reddit response is too large".into());
		}
		data.extend_from_slice(&chunk);
	}
	Ok(data)
}
