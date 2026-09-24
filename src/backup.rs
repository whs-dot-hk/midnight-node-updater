//! Pre-upgrade backup of the node's data directory (its `--base-path`).

use std::{
	fs,
	path::{Path, PathBuf},
	time::Instant,
};

use anyhow::{Context, Result, bail};
use tracing::info;

use crate::state::now_rfc3339;

/// Copy `data_dir` to `<backup_root>/data-backup-<timestamp>-<upgrade>`.
/// Must only be called while the node is stopped.
pub fn backup_data_dir(data_dir: &Path, backup_root: &Path, upgrade: &str) -> Result<PathBuf> {
	if !data_dir.is_dir() {
		bail!("data directory {} does not exist", data_dir.display());
	}
	let stamp = now_rfc3339().replace(':', "");
	let dest = backup_root.join(format!("data-backup-{stamp}-{upgrade}"));
	if dest.starts_with(data_dir) {
		bail!("backup directory {} is inside the data directory", dest.display());
	}
	info!("backing up {} to {} (set UNSAFE_SKIP_BACKUP=true to skip)", data_dir.display(), dest.display());
	let started = Instant::now();
	fs::create_dir_all(backup_root)?;
	copy_dir(data_dir, &dest).with_context(|| format!("backing up {}", data_dir.display()))?;
	info!("backup finished in {:.1?}", started.elapsed());
	Ok(dest)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
	fs::create_dir_all(dst)?;
	for entry in fs::read_dir(src)? {
		let entry = entry?;
		let ty = entry.file_type()?;
		let to = dst.join(entry.file_name());
		if ty.is_dir() {
			copy_dir(&entry.path(), &to)?;
		} else if ty.is_symlink() {
			std::os::unix::fs::symlink(fs::read_link(entry.path())?, &to)?;
		} else {
			fs::copy(entry.path(), &to)?;
		}
	}
	// Last: a read-only source directory must not block the copies above.
	fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::os::unix::fs::PermissionsExt as _;

	use super::*;

	#[test]
	fn copies_tree() {
		let tmp = tempfile::tempdir().unwrap();
		let data = tmp.path().join("chain");
		fs::create_dir_all(data.join("chains/mainnet/db")).unwrap();
		fs::write(data.join("chains/mainnet/db/CURRENT"), "x").unwrap();
		let out = backup_data_dir(&data, &tmp.path().join("backups"), "v2").unwrap();
		assert_eq!(fs::read_to_string(out.join("chains/mainnet/db/CURRENT")).unwrap(), "x");
		assert!(backup_data_dir(&data, &data.join("bk"), "v2").is_err());

		// Read-only directories are copied, contents included.
		fs::set_permissions(data.join("chains"), fs::Permissions::from_mode(0o555)).unwrap();
		let out = backup_data_dir(&data, &tmp.path().join("backups2"), "v3");
		fs::set_permissions(data.join("chains"), fs::Permissions::from_mode(0o755)).unwrap();
		let out = out.unwrap();
		assert!(out.join("chains/mainnet/db/CURRENT").is_file());
		assert_eq!(fs::metadata(out.join("chains")).unwrap().permissions().mode() & 0o777, 0o555);
		fs::set_permissions(out.join("chains"), fs::Permissions::from_mode(0o755)).unwrap();
	}
}
