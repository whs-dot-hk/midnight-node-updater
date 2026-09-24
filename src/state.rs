//! Persistent supervisor state: which upgrades were applied, and the last chain
//! status observed. The latter lets `midnight-node-updater` apply an upgrade *before*
//! relaunching when the old node died after the trigger was reached (for
//! example because it could not execute a new runtime).

use std::{collections::HashSet, io::Write, os::unix::fs::PermissionsExt as _, path::Path, time::SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::plan::ChainStatus;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
	#[serde(default)]
	pub applied: Vec<AppliedUpgrade>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub last_observed: Option<Observed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedUpgrade {
	pub name: String,
	pub applied_at: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub observed: Option<ChainStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observed {
	#[serde(flatten)]
	pub status: ChainStatus,
	pub observed_at: String,
}

impl State {
	pub fn load(path: &Path) -> Result<Self> {
		match std::fs::read(path) {
			Ok(bytes) => {
				serde_json::from_slice(&bytes).with_context(|| format!("parsing state file {}", path.display()))
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(e).with_context(|| format!("reading state file {}", path.display())),
		}
	}

	pub fn save(&self, path: &Path) -> Result<()> {
		write_atomic(path, &serde_json::to_vec_pretty(self)?)
			.with_context(|| format!("writing state file {}", path.display()))
	}

	pub fn applied_names(&self) -> HashSet<String> {
		self.applied.iter().map(|a| a.name.clone()).collect()
	}

	pub fn record_applied(&mut self, name: &str, observed: Option<ChainStatus>) {
		self.applied.push(AppliedUpgrade { name: name.to_string(), applied_at: now_rfc3339(), observed });
	}

	pub fn observe(&mut self, status: ChainStatus) {
		self.last_observed = Some(Observed { status, observed_at: now_rfc3339() });
	}
}

pub fn now_rfc3339() -> String {
	humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let dir = path.parent().unwrap_or(Path::new("."));
	let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
	tmp.write_all(bytes)?;
	// NamedTempFile creates 0600; state and plans must be readable by other
	// users (e.g. `status` run by an operator while `run` is the service user).
	tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o644))?;
	tmp.as_file().sync_all()?;
	tmp.persist(path).map_err(|e| e.error)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn roundtrip() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("state.json");
		let mut s = State::load(&path).unwrap();
		assert!(s.applied.is_empty());
		s.observe(ChainStatus { height: 5, spec_version: 9 });
		s.record_applied("v2", Some(ChainStatus { height: 5, spec_version: 9 }));
		s.save(&path).unwrap();
		let s2 = State::load(&path).unwrap();
		assert!(s2.applied_names().contains("v2"));
		assert_eq!(s2.last_observed.unwrap().status.height, 5);
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
	}
}
