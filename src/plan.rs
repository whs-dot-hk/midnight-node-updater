//! Upgrade plans: `upgrades/<name>/upgrade-info.json`.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A registered node-binary upgrade.
///
/// A plan triggers once the chain reaches `height` **or** the on-chain runtime
/// reaches `spec_version`, whichever happens first. At least one must be set.
/// `height: 0` means "switch as soon as the supervisor sees this plan".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradePlan {
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub height: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub spec_version: Option<u32>,
	/// Download instructions, in this format: either
	/// `{"binaries": {"linux/amd64": "<url>?checksum=sha256:<hex>", ...}}`
	/// or a URL string pointing at a JSON document of that shape.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub info: Option<serde_json::Value>,
}

/// What the supervisor last saw on chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainStatus {
	pub height: u64,
	pub spec_version: u32,
}

impl UpgradePlan {
	pub fn validate(&self) -> Result<()> {
		validate_name(&self.name)?;
		if self.height.is_none() && self.spec_version.is_none() {
			bail!("upgrade {:?} has neither `height` nor `spec_version`", self.name);
		}
		Ok(())
	}

	pub fn is_triggered(&self, status: &ChainStatus) -> bool {
		self.height.is_some_and(|h| status.height >= h) || self.spec_version.is_some_and(|v| status.spec_version >= v)
	}

	pub fn describe_trigger(&self) -> String {
		match (self.height, self.spec_version) {
			(Some(h), Some(v)) => format!("height >= {h} or spec_version >= {v}"),
			(Some(0), None) => "immediate".to_string(),
			(Some(h), None) => format!("height >= {h}"),
			(None, Some(v)) => format!("spec_version >= {v}"),
			(None, None) => "never (invalid)".to_string(),
		}
	}

	fn order_key(&self) -> (u64, u32, &str) {
		(self.height.unwrap_or(u64::MAX), self.spec_version.unwrap_or(u32::MAX), &self.name)
	}
}

/// Upgrade names become directory names, so keep them boring. Names are
/// lowercased.
pub fn normalize_name(name: &str) -> Result<String> {
	let n = name.trim().to_ascii_lowercase();
	validate_name(&n)?;
	Ok(n)
}

fn validate_name(name: &str) -> Result<()> {
	let ok = !name.is_empty()
		&& name != "."
		&& name != ".."
		&& name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
	if !ok {
		bail!("invalid upgrade name {name:?}: use lowercase letters, digits, `.`, `-` or `_`");
	}
	Ok(())
}

/// The first not-yet-applied plan whose trigger has fired. When several fire at
/// once (e.g. a node catching up after downtime) they are applied one at a time
/// in (height, spec_version) order.
pub fn next_triggered<'a>(
	plans: &'a [UpgradePlan],
	applied: &HashSet<String>,
	status: &ChainStatus,
) -> Option<&'a UpgradePlan> {
	plans
		.iter()
		.filter(|p| !applied.contains(&p.name) && p.is_triggered(status))
		.min_by(|a, b| a.order_key().cmp(&b.order_key()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn plan(name: &str, height: Option<u64>, spec: Option<u32>) -> UpgradePlan {
		UpgradePlan { name: name.into(), height, spec_version: spec, info: None }
	}

	#[test]
	fn triggers() {
		let s = ChainStatus { height: 100, spec_version: 1_000_000 };
		assert!(plan("a", Some(100), None).is_triggered(&s));
		assert!(!plan("a", Some(101), None).is_triggered(&s));
		assert!(plan("a", None, Some(1_000_000)).is_triggered(&s));
		assert!(!plan("a", None, Some(1_000_001)).is_triggered(&s));
		assert!(plan("a", Some(500), Some(999)).is_triggered(&s));
		assert!(plan("a", Some(0), None).is_triggered(&s));
	}

	#[test]
	fn picks_earliest_unapplied() {
		let plans = vec![
			plan("v3", Some(300), None),
			plan("v2", Some(200), None),
			plan("v4", Some(400), None),
			plan("rt", None, Some(7)),
		];
		let s = ChainStatus { height: 350, spec_version: 7 };
		let mut applied = HashSet::new();
		assert_eq!(next_triggered(&plans, &applied, &s).unwrap().name, "v2");
		applied.insert("v2".to_string());
		assert_eq!(next_triggered(&plans, &applied, &s).unwrap().name, "v3");
		applied.insert("v3".to_string());
		assert_eq!(next_triggered(&plans, &applied, &s).unwrap().name, "rt");
		applied.insert("rt".to_string());
		assert!(next_triggered(&plans, &applied, &s).is_none());
	}

	#[test]
	fn names() {
		assert_eq!(normalize_name("Node-1.1.0").unwrap(), "node-1.1.0");
		assert!(normalize_name("../etc").is_err());
		assert!(normalize_name("a/b").is_err());
		assert!(normalize_name("..").is_err());
		assert!(normalize_name("").is_err());
		assert!(plan("x", None, None).validate().is_err());
	}

	#[test]
	fn json_roundtrip() {
		let p: UpgradePlan = serde_json::from_str(
			r#"{"name":"node-1.1.0","spec_version":1001000,"info":{"binaries":{"linux/amd64":"https://x/y?checksum=sha256:00"}}}"#,
		)
		.unwrap();
		assert_eq!(p.spec_version, Some(1_001_000));
		assert_eq!(p.height, None);
		let back: UpgradePlan = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
		assert_eq!(p, back);
	}
}
