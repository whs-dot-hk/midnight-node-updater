//! Non-`run` subcommands.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
	config::Config,
	download::{has_download_info, platform},
	layout::{Layout, install_executable},
	plan::{UpgradePlan, normalize_name},
	rpc::RpcClient,
	state::State,
	supervisor::prepare_binary,
};

/// `init <path>`: create the layout and install the genesis binary.
pub fn init(cfg: &Config, exe: &Path) -> Result<()> {
	let layout = Layout::new(&cfg.home, &cfg.name);
	let dest = layout.bin_path(&layout.genesis_dir());
	std::fs::create_dir_all(layout.upgrades_dir())?;
	if dest.exists() {
		warn!("{} already exists; overwriting", dest.display());
	}
	install_executable(exe, &dest)?;
	if std::fs::symlink_metadata(layout.current_link()).is_err() {
		layout.set_current(None)?;
	}
	let state_file = layout.state_file();
	if !state_file.exists() {
		State::default().save(&state_file)?;
	}
	info!("initialized {} (genesis binary {})", layout.root().display(), dest.display());
	Ok(())
}

pub struct AddUpgrade<'a> {
	pub name: &'a str,
	pub exe: Option<&'a Path>,
	pub height: Option<u64>,
	pub spec_version: Option<u32>,
	pub immediate: bool,
	pub urls: &'a [String],
	pub force: bool,
}

/// `add-upgrade`: register a plan and (optionally) install its binary.
pub fn add_upgrade(cfg: &Config, req: AddUpgrade<'_>) -> Result<UpgradePlan> {
	let layout = Layout::new(&cfg.home, &cfg.name);
	let name = normalize_name(req.name)?;
	let state = State::load(&layout.state_file())?;
	if state.applied_names().contains(&name) {
		bail!("upgrade {name:?} has already been applied");
	}
	if layout.plan_file(&name).exists() && !req.force {
		bail!("upgrade {name:?} already exists; pass --force to overwrite");
	}
	if req.immediate && req.height.is_some() {
		bail!("--immediate and --upgrade-height are mutually exclusive");
	}
	let height = if req.immediate { Some(0) } else { req.height };
	if height.is_none() && req.spec_version.is_none() {
		bail!("specify a trigger: --upgrade-height, --spec-version and/or --immediate");
	}
	if req.exe.is_none() && req.urls.is_empty() {
		bail!("provide the binary path or at least one --url");
	}

	let info = if req.urls.is_empty() {
		None
	} else {
		let mut binaries = serde_json::Map::new();
		for u in req.urls {
			let (key, url) = match u.split_once('=') {
				// `linux/amd64=<url>`; a bare URL's first `=` is in its query string.
				Some((k, v)) if !k.contains(':') && (k.contains('/') || k == "any") => (k.to_string(), v.to_string()),
				_ => ("any".to_string(), u.clone()),
			};
			crate::download::split_checksum(&url)?;
			binaries.insert(key, Value::String(url));
		}
		Some(json!({ "binaries": binaries }))
	};

	let plan = UpgradePlan { name: name.clone(), height, spec_version: req.spec_version, info };
	plan.validate()?;
	if let Some(exe) = req.exe {
		install_executable(exe, &layout.upgrade_bin(&name))?;
	} else if !has_download_info(plan.info.as_ref()) {
		warn!("none of the --url entries matches this machine ({})", platform());
	}
	layout.save_plan(&plan)?;
	info!("added upgrade {name:?}: {} ({})", plan.describe_trigger(), layout.plan_file(&name).display());
	Ok(plan)
}

/// `status`: current version, plans, and (optionally) live chain status.
pub async fn status(cfg: &Config, as_json: bool, query_rpc: bool) -> Result<()> {
	let layout = Layout::new(&cfg.home, &cfg.name);
	let state = State::load(&layout.state_file())?;
	let applied = state.applied_names();
	let chain = if query_rpc {
		RpcClient::new(&cfg.rpc_url, Duration::from_secs(5))?.status(cfg.finality).await.ok()
	} else {
		None
	};

	let plans: Vec<Value> = layout
		.load_plans()
		.into_iter()
		.map(|p| {
			let bin = layout.upgrade_bin(&p.name);
			json!({
				"name": p.name,
				"trigger": p.describe_trigger(),
				"height": p.height,
				"spec_version": p.spec_version,
				"status": if applied.contains(&p.name) { "applied" } else { "pending" },
				"binary": bin,
				"binary_present": bin.is_file(),
				"downloadable": has_download_info(p.info.as_ref()),
			})
		})
		.collect();

	let report = json!({
		"home": layout.root(),
		"current": layout.current_name().ok(),
		"current_binary": layout.current_bin().ok(),
		"chain": chain,
		"last_observed": state.last_observed,
		"applied": state.applied,
		"upgrades": plans,
	});

	if as_json {
		println!("{}", serde_json::to_string_pretty(&report)?);
		return Ok(());
	}

	println!("home:     {}", layout.root().display());
	println!("current:  {}", layout.current_name().unwrap_or_else(|_| "?".into()));
	match (chain, &state.last_observed) {
		(Some(c), _) => println!(
			"chain:    height {} spec_version {} (live, {} block)",
			c.height,
			c.spec_version,
			finality_name(cfg)
		),
		(None, Some(o)) => println!(
			"chain:    height {} spec_version {} (last observed {}; RPC {} unreachable)",
			o.status.height, o.status.spec_version, o.observed_at, cfg.rpc_url
		),
		(None, None) => println!("chain:    unknown (RPC {} unreachable)", cfg.rpc_url),
	}
	if plans.is_empty() {
		println!("upgrades: none registered");
	} else {
		println!("upgrades:");
		for p in &plans {
			let binary = if p["binary_present"].as_bool() == Some(true) {
				"binary ok"
			} else if p["downloadable"].as_bool() == Some(true) {
				"binary downloadable"
			} else {
				"BINARY MISSING"
			};
			println!(
				"  {:<24} {:<8} {:<40} {}",
				p["name"].as_str().unwrap_or_default(),
				p["status"].as_str().unwrap_or_default(),
				p["trigger"].as_str().unwrap_or_default(),
				binary
			);
		}
	}
	Ok(())
}

fn finality_name(cfg: &Config) -> &'static str {
	match cfg.finality {
		crate::config::Finality::Best => "best",
		crate::config::Finality::Finalized => "finalized",
	}
}

/// `prepare-upgrade`: download and verify binaries for all pending upgrades now.
pub async fn prepare_upgrade(cfg: &Config) -> Result<()> {
	let layout = Layout::new(&cfg.home, &cfg.name);
	let applied = State::load(&layout.state_file())?.applied_names();
	let pending: Vec<_> = layout.load_plans().into_iter().filter(|p| !applied.contains(&p.name)).collect();
	if pending.is_empty() {
		info!("no pending upgrades");
		return Ok(());
	}
	let mut failed = 0;
	for plan in &pending {
		match prepare_binary(cfg, &layout, plan, true).await {
			Ok(bin) => info!("upgrade {:?} ready: {}", plan.name, bin.display()),
			Err(e) => {
				failed += 1;
				tracing::error!("upgrade {:?}: {e:#}", plan.name);
			}
		}
	}
	if failed > 0 {
		bail!("{failed} of {} pending upgrades are not ready", pending.len());
	}
	Ok(())
}

/// `version`: our version, then the current node binary's.
pub async fn version(cfg: &Config) -> Result<()> {
	println!("midnight-node-updater {}", env!("CARGO_PKG_VERSION"));
	let layout = Layout::new(&cfg.home, &cfg.name);
	let bin = layout.current_bin()?;
	let out = tokio::process::Command::new(&bin)
		.arg("--version")
		.output()
		.await
		.with_context(|| format!("running {}", bin.display()))?;
	print!("{}", String::from_utf8_lossy(&out.stdout));
	Ok(())
}
