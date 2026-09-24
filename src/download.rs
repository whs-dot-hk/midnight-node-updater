//! Binary auto-download from an upgrade plan's `info` field:
//!
//! ```json
//! {"binaries": {"linux/amd64": "https://host/midnight-node.tar.gz?checksum=sha256:<hex>"}}
//! ```
//!
//! The URL may point at a raw executable, a `.tar.gz` or a `.zip`. Archives
//! are searched for `bin/<DAEMON_NAME>` or `<DAEMON_NAME>`.

use std::{
	fs::File,
	io::{Read, Write as _},
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt as _;
use tracing::info;

use crate::layout::install_executable;

/// `<os>/<arch>` in Go notation, e.g. `linux/amd64`.
pub fn platform() -> String {
	let os = match std::env::consts::OS {
		"macos" => "darwin",
		other => other,
	};
	let arch = match std::env::consts::ARCH {
		"x86_64" => "amd64",
		"aarch64" => "arm64",
		"x86" => "386",
		other => other,
	};
	format!("{os}/{arch}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
	Sha256(String),
	Sha512(String),
}

impl Checksum {
	fn parse(s: &str) -> Result<Self> {
		let (algo, hex) = s.split_once(':').ok_or_else(|| anyhow!("checksum {s:?} must be <algo>:<hex>"))?;
		let hex = hex.to_ascii_lowercase();
		let (sum, len) = match algo.to_ascii_lowercase().as_str() {
			"sha256" => (Self::Sha256(hex.clone()), 64),
			"sha512" => (Self::Sha512(hex.clone()), 128),
			other => bail!("unsupported checksum algorithm {other:?} (use sha256 or sha512)"),
		};
		if hex.len() != len || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
			bail!("malformed {algo} checksum {hex:?}");
		}
		Ok(sum)
	}

	fn expected(&self) -> &str {
		match self {
			Self::Sha256(h) | Self::Sha512(h) => h,
		}
	}
}

enum Hasher {
	Sha256(Sha256),
	Sha512(Sha512),
}

impl Hasher {
	fn for_checksum(c: Option<&Checksum>) -> Option<Self> {
		c.map(|c| match c {
			Checksum::Sha256(_) => Self::Sha256(Sha256::new()),
			Checksum::Sha512(_) => Self::Sha512(Sha512::new()),
		})
	}

	fn update(&mut self, data: &[u8]) {
		match self {
			Self::Sha256(h) => h.update(data),
			Self::Sha512(h) => h.update(data),
		}
	}

	fn finish(self) -> String {
		match self {
			Self::Sha256(h) => hex::encode(h.finalize()),
			Self::Sha512(h) => hex::encode(h.finalize()),
		}
	}
}

/// Split the go-getter style `?checksum=<algo>:<hex>` query parameter off a URL.
pub fn split_checksum(raw: &str) -> Result<(String, Option<Checksum>)> {
	let mut url = url::Url::parse(raw).with_context(|| format!("invalid download URL {raw:?}"))?;
	if !matches!(url.scheme(), "https" | "http" | "file") {
		bail!("unsupported URL scheme in {raw:?}");
	}
	// Read the checksum decoded, but rebuild the rest of the query byte-for-byte:
	// re-encoding would break pre-signed (e.g. S3) URLs.
	let checksum = url.query_pairs().find(|(k, _)| k == "checksum").map(|(_, v)| v.into_owned());
	let rest: Vec<&str> = url
		.query()
		.unwrap_or_default()
		.split('&')
		.filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("checksum"))
		.collect();
	let rest = if rest.is_empty() { None } else { Some(rest.join("&")) };
	url.set_query(rest.as_deref());
	Ok((url.to_string(), checksum.as_deref().map(Checksum::parse).transpose()?))
}

fn select_binary(doc: &Value) -> Result<String> {
	let binaries =
		doc.get("binaries").and_then(Value::as_object).ok_or_else(|| anyhow!("upgrade info has no `binaries` map"))?;
	let key = platform();
	binaries
		.get(&key)
		.or_else(|| binaries.get("any"))
		.and_then(Value::as_str)
		.map(str::to_string)
		.ok_or_else(|| anyhow!("upgrade info has no binary for {key} (or `any`)"))
}

pub fn has_download_info(info: Option<&Value>) -> bool {
	match info {
		Some(Value::String(_)) => true,
		Some(doc) => select_binary(doc).is_ok(),
		None => false,
	}
}

const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Downloader {
	http: reqwest::Client,
	must_have_checksum: bool,
}

impl Downloader {
	pub fn new(must_have_checksum: bool) -> Result<Self> {
		let http = reqwest::Client::builder()
			.connect_timeout(Duration::from_secs(30))
			// A stalled host must not hang the supervisor; large binaries make a
			// total timeout impractical, so bound the gap between bytes instead.
			.read_timeout(READ_TIMEOUT)
			.user_agent(concat!("midnight-node-updater/", env!("CARGO_PKG_VERSION")))
			.build()?;
		Ok(Self { http, must_have_checksum })
	}

	/// Resolve `info` to a binary URL, download and verify it, and install it
	/// at `dest` (e.g. `upgrades/<name>/bin/midnight-node`).
	pub async fn install(&self, info: &Value, daemon_name: &str, dest: &Path) -> Result<()> {
		let work_parent = dest.parent().and_then(Path::parent).context("bad destination")?;
		std::fs::create_dir_all(work_parent)?;
		let work = tempfile::Builder::new().prefix(".download-").tempdir_in(work_parent)?;

		let binary_url = match info {
			Value::String(doc_url) => {
				let doc_path = self.fetch(doc_url, &work.path().join("upgrade-info.json")).await?;
				let doc: Value = serde_json::from_slice(&std::fs::read(&doc_path)?)
					.with_context(|| format!("{doc_url} is not a JSON upgrade info document"))?;
				select_binary(&doc)?
			}
			doc => select_binary(doc)?,
		};

		let file = self.fetch(&binary_url, &work.path().join("download")).await?;
		let extract_dir = work.path().join("extract");
		let name = daemon_name.to_string();
		let found = tokio::task::spawn_blocking(move || unpack(&file, &extract_dir, &name)).await??;
		install_executable(&found, dest)?;
		info!("installed {}", dest.display());
		Ok(())
	}

	async fn fetch(&self, raw_url: &str, out: &Path) -> Result<PathBuf> {
		let (url, checksum) = split_checksum(raw_url)?;
		if checksum.is_none() && self.must_have_checksum {
			bail!("{url} has no `?checksum=sha256:<hex>` and DAEMON_DOWNLOAD_MUST_HAVE_CHECKSUM is enabled");
		}
		info!("downloading {url}");
		let hasher = Hasher::for_checksum(checksum.as_ref());

		let hasher = if let Some(path) = url.strip_prefix("file://") {
			let (path, out) = (path.to_string(), out.to_path_buf());
			tokio::task::spawn_blocking(move || copy_and_hash(&path, &out, hasher)).await??
		} else {
			let mut hasher = hasher;
			let mut file = tokio::fs::File::create(out).await?;
			let mut resp = self.http.get(&url).send().await?.error_for_status()?;
			while let Some(chunk) = resp.chunk().await? {
				if let Some(h) = hasher.as_mut() {
					h.update(&chunk);
				}
				file.write_all(&chunk).await?;
			}
			file.sync_all().await?;
			hasher
		};

		if let (Some(h), Some(expected)) = (hasher, checksum.as_ref()) {
			let actual = h.finish();
			if actual != expected.expected() {
				bail!("checksum mismatch for {url}: expected {}, got {actual}", expected.expected());
			}
			info!("checksum verified for {url}");
		}
		Ok(out.to_path_buf())
	}
}

fn copy_and_hash(src: &str, out: &Path, mut hasher: Option<Hasher>) -> Result<Option<Hasher>> {
	let mut src = File::open(src).with_context(|| format!("opening {src}"))?;
	let mut file = File::create(out)?;
	let mut buf = vec![0u8; 1 << 16];
	loop {
		let n = src.read(&mut buf)?;
		if n == 0 {
			break;
		}
		if let Some(h) = hasher.as_mut() {
			h.update(&buf[..n]);
		}
		file.write_all(&buf[..n])?;
	}
	file.sync_all()?;
	Ok(hasher)
}

/// Unpack `file` if it is an archive and return the path of the node binary.
fn unpack(file: &Path, extract_dir: &Path, daemon_name: &str) -> Result<PathBuf> {
	let mut magic = [0u8; 4];
	let n = File::open(file)?.read(&mut magic)?;
	let magic = &magic[..n];

	if magic.starts_with(&[0x1f, 0x8b]) {
		std::fs::create_dir_all(extract_dir)?;
		let gz = flate2::read::GzDecoder::new(File::open(file)?);
		// Default permission handling: never honour setuid/setgid bits from an
		// untrusted archive. install_executable sets 0755 on the result anyway.
		tar::Archive::new(gz).unpack(extract_dir).context("extracting tar.gz")?;
	} else if magic.starts_with(b"PK\x03\x04") {
		std::fs::create_dir_all(extract_dir)?;
		zip::ZipArchive::new(File::open(file)?)?.extract(extract_dir).context("extracting zip")?;
	} else {
		return Ok(file.to_path_buf());
	}

	for candidate in [extract_dir.join("bin").join(daemon_name), extract_dir.join(daemon_name)] {
		if candidate.is_file() {
			return Ok(candidate);
		}
	}
	find_file(extract_dir, daemon_name)?.ok_or_else(|| anyhow!("archive does not contain a `{daemon_name}` executable"))
}

fn find_file(dir: &Path, name: &str) -> Result<Option<PathBuf>> {
	for entry in std::fs::read_dir(dir)? {
		let entry = entry?;
		let ty = entry.file_type()?;
		if ty.is_dir() {
			if let Some(found) = find_file(&entry.path(), name)? {
				return Ok(Some(found));
			}
		} else if ty.is_file() && entry.file_name() == name {
			return Ok(Some(entry.path()));
		}
	}
	Ok(None)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn checksum_split() {
		let h = "a".repeat(64);
		let (url, sum) = split_checksum(&format!("https://x.io/node.tar.gz?checksum=sha256:{h}&foo=bar")).unwrap();
		assert_eq!(url, "https://x.io/node.tar.gz?foo=bar");
		assert_eq!(sum, Some(Checksum::Sha256(h.clone())));
		let (url, sum) = split_checksum("https://x.io/node").unwrap();
		assert_eq!(url, "https://x.io/node");
		assert!(sum.is_none());
		assert!(split_checksum("https://x.io/n?checksum=md5:abc").is_err());
		assert!(split_checksum("https://x.io/n?checksum=sha256:zz").is_err());
		assert!(split_checksum("ftp://x.io/n").is_err());
		// Pre-signed URLs must survive byte-for-byte.
		let signed = "https://b.s3.amazonaws.com/n.tar.gz?X-Amz-Credential=AK%2F2026%2Fus&X-Amz-Signature=ab%3Dcd";
		let (url, sum) = split_checksum(&format!("{signed}&checksum=sha256:{h}")).unwrap();
		assert_eq!(url, signed);
		assert!(sum.is_some());
	}

	#[test]
	fn selects_platform_or_any() {
		let doc = json!({"binaries": {platform(): "https://a", "any": "https://b"}});
		assert_eq!(select_binary(&doc).unwrap(), "https://a");
		let doc = json!({"binaries": {"any": "https://b"}});
		assert_eq!(select_binary(&doc).unwrap(), "https://b");
		assert!(select_binary(&json!({"binaries": {"plan9/mips": "x"}})).is_err());
	}

	fn sha256_file(p: &Path) -> String {
		hex::encode(Sha256::digest(std::fs::read(p).unwrap()))
	}

	#[tokio::test]
	async fn installs_from_tar_gz_with_checksum() {
		let dir = tempfile::tempdir().unwrap();
		let tgz = dir.path().join("node.tar.gz");
		{
			let enc = flate2::write::GzEncoder::new(File::create(&tgz).unwrap(), flate2::Compression::fast());
			let mut tar = tar::Builder::new(enc);
			let body = b"#!/bin/sh\necho v2\n";
			let mut header = tar::Header::new_gnu();
			header.set_size(body.len() as u64);
			header.set_mode(0o755);
			header.set_cksum();
			tar.append_data(&mut header, "release/bin/midnight-node", &body[..]).unwrap();
			tar.into_inner().unwrap().finish().unwrap();
		}
		let dest = dir.path().join("upgrades/v2/bin/midnight-node");
		let good =
			json!({"binaries": {"any": format!("file://{}?checksum=sha256:{}", tgz.display(), sha256_file(&tgz))}});
		Downloader::new(true).unwrap().install(&good, "midnight-node", &dest).await.unwrap();
		assert!(crate::layout::is_executable(&dest));

		let bad = json!({"binaries": {"any": format!("file://{}?checksum=sha256:{}", tgz.display(), "0".repeat(64))}});
		let err = Downloader::new(true).unwrap().install(&bad, "midnight-node", &dest).await.unwrap_err();
		assert!(err.to_string().contains("checksum mismatch"), "{err}");

		let unsummed = json!({"binaries": {"any": format!("file://{}", tgz.display())}});
		assert!(Downloader::new(true).unwrap().install(&unsummed, "midnight-node", &dest).await.is_err());
		Downloader::new(false).unwrap().install(&unsummed, "midnight-node", &dest).await.unwrap();
	}
}
