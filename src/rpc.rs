//! Minimal Substrate JSON-RPC client over HTTP (midnight-node serves HTTP and
//! WebSocket on the same port, 9944 by default).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{config::Finality, plan::ChainStatus};

#[derive(Clone)]
pub struct RpcClient {
	http: reqwest::Client,
	url: String,
}

#[derive(Deserialize)]
struct Response {
	result: Option<Value>,
	error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
	code: i64,
	message: String,
}

#[derive(Deserialize)]
struct Header {
	number: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeVersion {
	spec_version: u32,
}

impl RpcClient {
	pub fn new(url: &str, timeout: Duration) -> Result<Self> {
		let url = if let Some(rest) = url.strip_prefix("ws://") {
			format!("http://{rest}")
		} else if let Some(rest) = url.strip_prefix("wss://") {
			format!("https://{rest}")
		} else {
			url.to_string()
		};
		url::Url::parse(&url).with_context(|| format!("invalid RPC URL {url:?}"))?;
		let http = reqwest::Client::builder().timeout(timeout).build()?;
		Ok(Self { http, url })
	}

	pub fn url(&self) -> &str {
		&self.url
	}

	async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
		let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
		let resp: Response = self
			.http
			.post(&self.url)
			.json(&body)
			.send()
			.await
			.with_context(|| format!("{method}: request to {} failed", self.url))?
			.error_for_status()?
			.json()
			.await
			.with_context(|| format!("{method}: invalid JSON-RPC response"))?;
		if let Some(e) = resp.error {
			bail!("{method}: RPC error {}: {}", e.code, e.message);
		}
		let result = resp.result.filter(|v| !v.is_null()).ok_or_else(|| anyhow!("{method}: empty result"))?;
		serde_json::from_value(result).with_context(|| format!("{method}: unexpected result shape"))
	}

	/// Block number and runtime `spec_version` at the best or finalized block.
	/// Both are read at the same block hash so they are consistent.
	pub async fn status(&self, finality: Finality) -> Result<ChainStatus> {
		let hash: String = match finality {
			Finality::Best => self.call("chain_getBlockHash", json!([])).await?,
			Finality::Finalized => self.call("chain_getFinalizedHead", json!([])).await?,
		};
		let header: Header = self.call("chain_getHeader", json!([hash])).await?;
		let version: RuntimeVersion = self.call("state_getRuntimeVersion", json!([hash])).await?;
		Ok(ChainStatus { height: parse_block_number(&header.number)?, spec_version: version.spec_version })
	}
}

fn parse_block_number(n: &str) -> Result<u64> {
	let hex = n.strip_prefix("0x").ok_or_else(|| anyhow!("block number {n:?} is not hex"))?;
	u64::from_str_radix(hex, 16).with_context(|| format!("bad block number {n:?}"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn block_numbers() {
		assert_eq!(parse_block_number("0x0").unwrap(), 0);
		assert_eq!(parse_block_number("0x1a2b").unwrap(), 0x1a2b);
		assert!(parse_block_number("123").is_err());
	}

	#[test]
	fn ws_urls_become_http() {
		assert_eq!(
			RpcClient::new("ws://127.0.0.1:9944", Duration::from_secs(1)).unwrap().url(),
			"http://127.0.0.1:9944"
		);
		assert!(RpcClient::new("not a url", Duration::from_secs(1)).is_err());
	}
}
