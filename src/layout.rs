//! On-disk layout under `$DAEMON_HOME/midnight-node-updater`:
//!
//! ```text
//! midnight-node-updater/
//! ├── current -> genesis | upgrades/<name>
//! ├── genesis/bin/midnight-node
//! ├── upgrades/<name>/
//! │   ├── bin/midnight-node
//! │   └── upgrade-info.json
//! └── state.json
//! ```

use std::{
	fs,
	io::ErrorKind,
	os::unix::fs::PermissionsExt,
	path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::{plan::UpgradePlan, state::write_atomic};

pub const ROOT_DIR: &str = "midnight-node-updater";
pub const GENESIS: &str = "genesis";
pub const UPGRADES: &str = "upgrades";
pub const CURRENT: &str = "current";
pub const PLAN_FILE: &str = "upgrade-info.json";
pub const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone)]
pub struct Layout {
	root: PathBuf,
	daemon_name: String,
}

impl Layout {
	pub fn new(home: &Path, daemon_name: &str) -> Self {
		Self { root: home.join(ROOT_DIR), daemon_name: daemon_name.to_string() }
	}

	pub fn root(&self) -> &Path {
		&self.root
	}

	pub fn genesis_dir(&self) -> PathBuf {
		self.root.join(GENESIS)
	}

	pub fn upgrades_dir(&self) -> PathBuf {
		self.root.join(UPGRADES)
	}

	pub fn upgrade_dir(&self, name: &str) -> PathBuf {
		self.upgrades_dir().join(name)
	}

	pub fn plan_file(&self, name: &str) -> PathBuf {
		self.upgrade_dir(name).join(PLAN_FILE)
	}

	pub fn current_link(&self) -> PathBuf {
		self.root.join(CURRENT)
	}

	pub fn state_file(&self) -> PathBuf {
		self.root.join(STATE_FILE)
	}

	pub fn bin_path(&self, dir: &Path) -> PathBuf {
		dir.join("bin").join(&self.daemon_name)
	}

	pub fn upgrade_bin(&self, name: &str) -> PathBuf {
		self.bin_path(&self.upgrade_dir(name))
	}

	/// The directory `current` points to; `genesis` if the link does not exist yet.
	pub fn current_dir(&self) -> Result<PathBuf> {
		match fs::read_link(self.current_link()) {
			Ok(target) if target.is_absolute() => Ok(target),
			Ok(target) => Ok(self.root.join(target)),
			Err(e) if e.kind() == ErrorKind::NotFound => Ok(self.genesis_dir()),
			Err(e) => Err(e).with_context(|| format!("reading {}", self.current_link().display())),
		}
	}

	/// Human-readable name of the active version: `genesis` or the upgrade name.
	pub fn current_name(&self) -> Result<String> {
		let dir = self.current_dir()?;
		Ok(dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
	}

	pub fn current_bin(&self) -> Result<PathBuf> {
		let bin = self.bin_path(&self.current_dir()?);
		if !bin.is_file() {
			bail!(
				"no node binary at {}; run `midnight-node-updater init <path-to-{}>` first",
				bin.display(),
				self.daemon_name
			);
		}
		Ok(bin)
	}

	/// Atomically repoint `current` to `genesis` or `upgrades/<name>`.
	pub fn set_current(&self, upgrade: Option<&str>) -> Result<()> {
		let target = match upgrade {
			None => PathBuf::from(GENESIS),
			Some(name) => Path::new(UPGRADES).join(name),
		};
		if !self.root.join(&target).is_dir() {
			bail!("cannot switch to missing directory {}", self.root.join(&target).display());
		}
		let tmp = self.root.join(".current.tmp");
		match fs::remove_file(&tmp) {
			Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
			_ => {}
		}
		std::os::unix::fs::symlink(&target, &tmp).with_context(|| format!("creating symlink {}", tmp.display()))?;
		fs::rename(&tmp, self.current_link()).with_context(|| format!("updating {}", self.current_link().display()))?;
		Ok(())
	}

	/// All parseable plans. Broken plan files are logged and skipped so that a
	/// typo never takes a running node down.
	pub fn load_plans(&self) -> Vec<UpgradePlan> {
		let entries = match fs::read_dir(self.upgrades_dir()) {
			Ok(e) => e,
			Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
			Err(e) => {
				warn!("cannot read {}: {e}", self.upgrades_dir().display());
				return Vec::new();
			}
		};
		let mut plans = Vec::new();
		for entry in entries.flatten() {
			let dir_name = entry.file_name().to_string_lossy().into_owned();
			let path = entry.path().join(PLAN_FILE);
			if !path.is_file() {
				continue;
			}
			match read_plan(&path) {
				Ok(plan) if plan.name != dir_name => warn!(
					"ignoring {}: plan name {:?} does not match directory {:?}",
					path.display(),
					plan.name,
					dir_name
				),
				Ok(plan) => plans.push(plan),
				Err(e) => warn!("ignoring {}: {e:#}", path.display()),
			}
		}
		plans.sort_by(|a, b| a.name.cmp(&b.name));
		plans
	}

	pub fn save_plan(&self, plan: &UpgradePlan) -> Result<()> {
		plan.validate()?;
		let dir = self.upgrade_dir(&plan.name);
		fs::create_dir_all(&dir)?;
		write_atomic(&self.plan_file(&plan.name), &serde_json::to_vec_pretty(plan)?)?;
		Ok(())
	}
}

fn read_plan(path: &Path) -> Result<UpgradePlan> {
	let plan: UpgradePlan = serde_json::from_slice(&fs::read(path)?)?;
	plan.validate()?;
	Ok(plan)
}

/// Copy `src` to `dst` as an executable (0755), creating parent dirs.
pub fn install_executable(src: &Path, dst: &Path) -> Result<()> {
	if !src.is_file() {
		bail!("{} is not a file", src.display());
	}
	let parent = dst.parent().context("destination has no parent")?;
	fs::create_dir_all(parent)?;
	let tmp = parent.join(format!(".{}.tmp", dst.file_name().unwrap().to_string_lossy()));
	fs::copy(src, &tmp).with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
	fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
	fs::rename(&tmp, dst)?;
	Ok(())
}

pub fn is_executable(path: &Path) -> bool {
	fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn switch_current() {
		let home = tempfile::tempdir().unwrap();
		let l = Layout::new(home.path(), "midnight-node");
		fs::create_dir_all(l.genesis_dir().join("bin")).unwrap();
		assert_eq!(l.current_dir().unwrap(), l.genesis_dir());
		assert!(l.set_current(Some("v2")).is_err());

		l.save_plan(&UpgradePlan { name: "v2".into(), height: Some(10), spec_version: None, info: None }).unwrap();
		l.set_current(Some("v2")).unwrap();
		assert_eq!(l.current_name().unwrap(), "v2");
		l.set_current(None).unwrap();
		assert_eq!(l.current_name().unwrap(), "genesis");

		assert_eq!(l.load_plans().len(), 1);
		fs::create_dir_all(l.upgrade_dir("broken")).unwrap();
		fs::write(l.plan_file("broken"), "{not json").unwrap();
		assert_eq!(l.load_plans().len(), 1);
	}
}
