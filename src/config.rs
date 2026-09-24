//! Configuration, read from environment variables and an
//! optional TOML file whose keys are the same variable names. Environment
//! variables take precedence over the file.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

pub const DEFAULT_DAEMON_NAME: &str = "midnight-node";
pub const DEFAULT_RPC_URL: &str = "http://127.0.0.1:9944";
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(60);

pub const ENV_HOME: &str = "DAEMON_HOME";
pub const ENV_NAME: &str = "DAEMON_NAME";
pub const ENV_RPC_URL: &str = "DAEMON_RPC_URL";
pub const ENV_POLL_INTERVAL: &str = "DAEMON_POLL_INTERVAL";
pub const ENV_UPGRADE_FINALITY: &str = "DAEMON_UPGRADE_FINALITY";
pub const ENV_ALLOW_DOWNLOAD: &str = "DAEMON_ALLOW_DOWNLOAD_BINARIES";
pub const ENV_MUST_HAVE_CHECKSUM: &str = "DAEMON_DOWNLOAD_MUST_HAVE_CHECKSUM";
pub const ENV_RESTART_AFTER_UPGRADE: &str = "DAEMON_RESTART_AFTER_UPGRADE";
pub const ENV_RESTART_DELAY: &str = "DAEMON_RESTART_DELAY";
pub const ENV_SHUTDOWN_GRACE: &str = "DAEMON_SHUTDOWN_GRACE";
pub const ENV_DATA_DIR: &str = "DAEMON_DATA_DIR";
pub const ENV_BACKUP_DIR: &str = "DAEMON_DATA_BACKUP_DIR";
pub const ENV_SKIP_BACKUP: &str = "UNSAFE_SKIP_BACKUP";
pub const ENV_PREUPGRADE_SCRIPT: &str = "DAEMON_PREUPGRADE_SCRIPT";
pub const ENV_DISABLE_LOGS: &str = "MIDNIGHT_NODE_UPDATER_DISABLE_LOGS";

/// Which block the upgrade triggers are evaluated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Finality {
	/// The best (head) block. Reacts fastest; the default.
	Best,
	/// The last GRANDPA-finalized block. Never reacts to a block that may be reorged away.
	Finalized,
}

#[derive(Debug, Clone, Serialize)]
pub struct Config {
	pub home: PathBuf,
	pub name: String,
	pub rpc_url: String,
	#[serde(with = "humantime_serde_compat")]
	pub poll_interval: Duration,
	pub finality: Finality,
	pub allow_download: bool,
	pub download_must_have_checksum: bool,
	pub restart_after_upgrade: bool,
	#[serde(with = "humantime_serde_compat")]
	pub restart_delay: Duration,
	#[serde(with = "humantime_serde_compat")]
	pub shutdown_grace: Duration,
	pub data_dir: Option<PathBuf>,
	pub backup_dir: Option<PathBuf>,
	pub unsafe_skip_backup: bool,
	pub preupgrade_script: Option<PathBuf>,
	pub disable_logs: bool,
}

impl Config {
	/// Load from the process environment, layered over `file` if given.
	pub fn load(file: Option<&Path>) -> Result<Self> {
		let file_values = match file {
			Some(path) => read_config_file(path)?,
			None => HashMap::new(),
		};
		Self::from_lookup(|key| {
			std::env::var(key).ok().filter(|v| !v.is_empty()).or_else(|| file_values.get(key).cloned())
		})
	}

	pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
		let home = get(ENV_HOME).map(PathBuf::from).ok_or_else(|| anyhow!("{ENV_HOME} is not set"))?;
		if !home.is_absolute() {
			bail!("{ENV_HOME} must be an absolute path, got {}", home.display());
		}
		let name = get(ENV_NAME).unwrap_or_else(|| DEFAULT_DAEMON_NAME.to_string());
		if name.is_empty() || name.contains('/') {
			bail!("{ENV_NAME} must be a plain file name, got {name:?}");
		}

		let finality = match get(ENV_UPGRADE_FINALITY).as_deref().map(str::to_ascii_lowercase) {
			None => Finality::Best,
			Some(v) if v == "best" => Finality::Best,
			Some(v) if v == "finalized" => Finality::Finalized,
			Some(v) => bail!("{ENV_UPGRADE_FINALITY} must be `best` or `finalized`, got {v:?}"),
		};

		let cfg = Self {
			home,
			name,
			rpc_url: get(ENV_RPC_URL).unwrap_or_else(|| DEFAULT_RPC_URL.to_string()),
			poll_interval: duration(&get, ENV_POLL_INTERVAL, DEFAULT_POLL_INTERVAL)?,
			finality,
			allow_download: boolean(&get, ENV_ALLOW_DOWNLOAD, false)?,
			download_must_have_checksum: boolean(&get, ENV_MUST_HAVE_CHECKSUM, true)?,
			restart_after_upgrade: boolean(&get, ENV_RESTART_AFTER_UPGRADE, true)?,
			restart_delay: duration(&get, ENV_RESTART_DELAY, Duration::ZERO)?,
			shutdown_grace: duration(&get, ENV_SHUTDOWN_GRACE, DEFAULT_SHUTDOWN_GRACE)?,
			data_dir: get(ENV_DATA_DIR).map(PathBuf::from),
			backup_dir: get(ENV_BACKUP_DIR).map(PathBuf::from),
			unsafe_skip_backup: boolean(&get, ENV_SKIP_BACKUP, false)?,
			preupgrade_script: get(ENV_PREUPGRADE_SCRIPT).map(PathBuf::from),
			disable_logs: boolean(&get, ENV_DISABLE_LOGS, false)?,
		};
		if cfg.poll_interval.is_zero() {
			bail!("{ENV_POLL_INTERVAL} must be greater than zero");
		}
		Ok(cfg)
	}

	/// The node's data directory, used for pre-upgrade backups.
	///
	/// Resolution order: `DAEMON_DATA_DIR`, then `--base-path`/`-d` in the node
	/// arguments, then the `BASE_PATH` environment variable that midnight-node's
	/// own config layer reads.
	pub fn resolve_data_dir(&self, node_args: &[String]) -> Option<PathBuf> {
		self.data_dir
			.clone()
			.or_else(|| base_path_from_args(node_args))
			.or_else(|| std::env::var("BASE_PATH").ok().filter(|v| !v.is_empty()).map(PathBuf::from))
	}
}

pub fn base_path_from_args(args: &[String]) -> Option<PathBuf> {
	let mut iter = args.iter();
	while let Some(arg) = iter.next() {
		if arg == "--" {
			break;
		}
		if let Some(v) = arg.strip_prefix("--base-path=") {
			return Some(PathBuf::from(v));
		}
		if arg == "--base-path" || arg == "-d" {
			return iter.next().map(PathBuf::from);
		}
	}
	None
}

fn read_config_file(path: &Path) -> Result<HashMap<String, String>> {
	let text = std::fs::read_to_string(path).with_context(|| format!("reading config file {}", path.display()))?;
	let table: toml::Table =
		toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
	table
		.into_iter()
		.map(|(k, v)| {
			let s = match v {
				toml::Value::String(s) => s,
				toml::Value::Integer(i) => i.to_string(),
				toml::Value::Boolean(b) => b.to_string(),
				other => bail!("config key {k}: unsupported value {other}"),
			};
			Ok((k.to_ascii_uppercase(), s))
		})
		.collect()
}

fn boolean(get: &impl Fn(&str) -> Option<String>, key: &str, default: bool) -> Result<bool> {
	match get(key) {
		None => Ok(default),
		Some(v) => match v.to_ascii_lowercase().as_str() {
			"1" | "true" | "yes" | "on" => Ok(true),
			"0" | "false" | "no" | "off" => Ok(false),
			_ => bail!("{key} must be a boolean, got {v:?}"),
		},
	}
}

fn duration(get: &impl Fn(&str) -> Option<String>, key: &str, default: Duration) -> Result<Duration> {
	match get(key) {
		None => Ok(default),
		Some(v) => humantime::parse_duration(&v)
			.with_context(|| format!("{key} must be a duration such as `5s` or `300ms`, got {v:?}")),
	}
}

mod humantime_serde_compat {
	use std::time::Duration;

	pub fn serialize<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
		s.serialize_str(&humantime::format_duration(*d).to_string())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cfg(pairs: &[(&str, &str)]) -> Result<Config> {
		let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
		Config::from_lookup(|k| map.get(k).cloned())
	}

	#[test]
	fn defaults() {
		let c = cfg(&[(ENV_HOME, "/srv/midnight")]).unwrap();
		assert_eq!(c.name, "midnight-node");
		assert_eq!(c.rpc_url, DEFAULT_RPC_URL);
		assert_eq!(c.finality, Finality::Best);
		assert!(!c.allow_download);
		assert!(c.download_must_have_checksum);
		assert!(c.restart_after_upgrade);
		assert!(!c.unsafe_skip_backup);
		assert_eq!(c.shutdown_grace, DEFAULT_SHUTDOWN_GRACE);
	}

	#[test]
	fn requires_absolute_home() {
		assert!(cfg(&[]).is_err());
		assert!(cfg(&[(ENV_HOME, "relative")]).is_err());
	}

	#[test]
	fn parses_values() {
		let c = cfg(&[
			(ENV_HOME, "/h"),
			(ENV_POLL_INTERVAL, "300ms"),
			(ENV_UPGRADE_FINALITY, "Finalized"),
			(ENV_SKIP_BACKUP, "true"),
			(ENV_PREUPGRADE_SCRIPT, "pre.sh"),
		])
		.unwrap();
		assert_eq!(c.poll_interval, Duration::from_millis(300));
		assert_eq!(c.finality, Finality::Finalized);
		assert!(c.unsafe_skip_backup);
		assert_eq!(c.preupgrade_script, Some(PathBuf::from("pre.sh")));
		assert!(cfg(&[(ENV_HOME, "/h"), (ENV_SKIP_BACKUP, "maybe")]).is_err());
	}

	#[test]
	fn base_path_parsing() {
		let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
		assert_eq!(base_path_from_args(&a(&["--base-path", "/d"])), Some("/d".into()));
		assert_eq!(base_path_from_args(&a(&["--validator", "--base-path=/x"])), Some("/x".into()));
		assert_eq!(base_path_from_args(&a(&["-d", "node/chain"])), Some("node/chain".into()));
		assert_eq!(base_path_from_args(&a(&["--chain", "mainnet"])), None);
	}
}
