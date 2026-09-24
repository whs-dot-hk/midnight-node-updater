//! `midnight-node-updater run`: launch the node, watch the chain, switch binaries.

use std::{
	collections::HashMap,
	os::unix::process::ExitStatusExt,
	path::{Path, PathBuf},
	process::{ExitStatus, Stdio},
	time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nix::{sys::signal::Signal, unistd::Pid};
use tokio::{
	process::{Child, Command},
	signal::unix::{SignalKind, signal},
	time::{Interval, MissedTickBehavior, interval, timeout},
};
use tracing::{debug, error, info, warn};

use crate::{
	backup::backup_data_dir,
	config::Config,
	download::{Downloader, has_download_info},
	layout::{Layout, is_executable},
	plan::{ChainStatus, UpgradePlan, next_triggered},
	rpc::RpcClient,
	state::State,
};

const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often to persist `last_observed` when nothing interesting changes.
const PERSIST_EVERY: Duration = Duration::from_secs(30);
/// How often to repeat "upgrade triggered but binary not ready" errors.
const NAG_EVERY: Duration = Duration::from_secs(60);

enum Outcome {
	/// The node exited (on its own or because we forwarded a signal).
	Exited(i32),
	/// The node was stopped because `plan` triggered.
	Upgrade(UpgradePlan, ChainStatus),
}

struct Signals {
	int: tokio::signal::unix::Signal,
	term: tokio::signal::unix::Signal,
}

impl Signals {
	fn new() -> Result<Self> {
		Ok(Self { int: signal(SignalKind::interrupt())?, term: signal(SignalKind::terminate())? })
	}

	async fn recv(&mut self) -> Signal {
		tokio::select! {
			_ = self.int.recv() => Signal::SIGINT,
			_ = self.term.recv() => Signal::SIGTERM,
		}
	}
}

/// Run the node with `args` until it exits, performing upgrades along the way.
/// Returns the exit code `midnight-node-updater` should exit with.
pub async fn run(cfg: Config, args: Vec<String>) -> Result<i32> {
	let layout = Layout::new(&cfg.home, &cfg.name);
	layout.current_bin()?;

	let data_dir = if cfg.unsafe_skip_backup {
		None
	} else {
		Some(cfg.resolve_data_dir(&args).context(
			"cannot determine the node data directory for pre-upgrade backups; \
			 set DAEMON_DATA_DIR, pass --base-path, or set UNSAFE_SKIP_BACKUP=true",
		)?)
	};
	let rpc = RpcClient::new(&cfg.rpc_url, RPC_TIMEOUT)?;
	let mut signals = Signals::new()?;
	// No point backing up twice in a row when several upgrades are applied
	// back-to-back without the node running in between.
	let mut node_ran_since_backup = true;

	loop {
		let mut state = State::load(&layout.state_file())?;

		// The old node may have died after the trigger was reached (e.g. it
		// could not execute a new runtime). Upgrade before relaunching it.
		if let Some((plan, status)) = already_triggered(&layout, &state) {
			info!(
				"upgrade {:?} ({}) was already reached at height {} / spec_version {}; applying before launch",
				plan.name,
				plan.describe_trigger(),
				status.height,
				status.spec_version
			);
			match prepare_binary(&cfg, &layout, &plan, cfg.allow_download).await {
				Ok(_) => {
					let backup = data_dir.as_deref().filter(|_| node_ran_since_backup);
					apply_upgrade(&cfg, &layout, &mut state, &plan, status, backup).await?;
					node_ran_since_backup = false;
					if !cfg.restart_after_upgrade {
						info!("DAEMON_RESTART_AFTER_UPGRADE=false; exiting after upgrade");
						return Ok(0);
					}
					continue;
				}
				// Same policy as while running: never sit with no node at all.
				Err(e) => error!(
					"upgrade {:?} has triggered but its binary is not ready: {e:#}. \
					 Launching the current binary; install the binary at {} to proceed",
					plan.name,
					layout.upgrade_bin(&plan.name).display()
				),
			}
		}

		let bin = layout.current_bin()?;
		warn_unprepared_plans(&cfg, &layout, &state);
		info!("starting {} ({}) {}", bin.display(), layout.current_name()?, args.join(" "));
		let mut child = spawn_node(&bin, &args)?;
		node_ran_since_backup = true;

		let outcome = monitor(&cfg, &layout, &rpc, &mut state, &mut child, &mut signals).await?;
		state.save(&layout.state_file())?;
		match outcome {
			Outcome::Exited(code) => {
				// The upgrade was reached but the node exited before we stopped
				// it ourselves; go round again so it is applied before relaunch.
				if cfg.restart_after_upgrade
					&& let Some((plan, _)) = already_triggered(&layout, &state)
				{
					info!("upgrade {:?} had triggered before the node exited", plan.name);
					match prepare_binary(&cfg, &layout, &plan, cfg.allow_download).await {
						Ok(_) => continue,
						Err(e) => {
							error!("upgrade {:?}: binary not ready ({e:#}); exiting with the node's status", plan.name)
						}
					}
				}
				return Ok(code);
			}
			Outcome::Upgrade(plan, status) => {
				apply_upgrade(&cfg, &layout, &mut state, &plan, status, data_dir.as_deref()).await?;
				node_ran_since_backup = false;
				if !cfg.restart_after_upgrade {
					info!("DAEMON_RESTART_AFTER_UPGRADE=false; exiting after upgrade");
					return Ok(0);
				}
			}
		}
	}
}

/// A pending plan whose trigger was reached at the last observed chain status.
fn already_triggered(layout: &Layout, state: &State) -> Option<(UpgradePlan, ChainStatus)> {
	let status = state.last_observed.as_ref()?.status;
	let plans = layout.load_plans();
	next_triggered(&plans, &state.applied_names(), &status).map(|p| (p.clone(), status))
}

fn spawn_node(bin: &Path, args: &[String]) -> Result<Child> {
	let mut cmd = Command::new(bin);
	cmd.args(args)
		.stdin(Stdio::null())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		// Own process group: a terminal Ctrl-C reaches only us, and we forward
		// it once, rather than the node receiving it twice.
		.process_group(0);
	#[cfg(target_os = "linux")]
	unsafe {
		// Don't leave an orphaned node behind if midnight-node-updater is SIGKILLed.
		// Safe with the current-thread runtime: the spawning thread lives as
		// long as the process.
		cmd.pre_exec(|| nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::from));
	}
	cmd.spawn().with_context(|| format!("spawning {}", bin.display()))
}

/// Per-run bookkeeping for the polling loop.
struct Watch {
	ticker: Interval,
	last_persist: Instant,
	rpc_up: bool,
	nagged: HashMap<String, Instant>,
}

async fn monitor(
	cfg: &Config,
	layout: &Layout,
	rpc: &RpcClient,
	state: &mut State,
	child: &mut Child,
	signals: &mut Signals,
) -> Result<Outcome> {
	enum Event {
		Exited(std::io::Result<ExitStatus>),
		Signal(Signal),
		Polled(Result<Option<(UpgradePlan, ChainStatus)>>),
	}

	let mut ticker = interval(cfg.poll_interval);
	ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
	let mut watch = Watch { ticker, last_persist: Instant::now(), rpc_up: false, nagged: HashMap::new() };

	loop {
		// The whole poll (RPC calls, download, `--version` check) runs inside
		// the select so node exit and SIGINT/SIGTERM are never left waiting
		// behind a slow network. Dropping a half-done poll is safe: downloads
		// go to a temp dir and `state` is only mutated between awaits.
		let event = tokio::select! {
			status = child.wait() => Event::Exited(status),
			sig = signals.recv() => Event::Signal(sig),
			polled = poll(cfg, layout, rpc, state, &mut watch) => Event::Polled(polled),
		};

		match event {
			Event::Exited(status) => {
				let code = exit_code(status?);
				if code == 0 {
					info!("node exited");
				} else {
					warn!("node exited with code {code}");
				}
				return Ok(Outcome::Exited(code));
			}
			Event::Signal(sig) => {
				info!("received {sig}; stopping node");
				let status = stop_node(child, sig, cfg.shutdown_grace).await?;
				return Ok(Outcome::Exited(exit_code(status)));
			}
			Event::Polled(Ok(None)) => {}
			Event::Polled(Ok(Some((plan, status)))) => {
				info!(
					"upgrade {:?} triggered ({}) at height {}, spec_version {}; stopping node",
					plan.name,
					plan.describe_trigger(),
					status.height,
					status.spec_version
				);
				state.save(&layout.state_file())?;
				stop_node(child, Signal::SIGINT, cfg.shutdown_grace).await?;
				return Ok(Outcome::Upgrade(plan, status));
			}
			Event::Polled(Err(e)) => return Err(e),
		}
	}
}

/// One poll: wait a tick, read the chain status, and return the plan to apply
/// if one has triggered and its binary is ready.
async fn poll(
	cfg: &Config,
	layout: &Layout,
	rpc: &RpcClient,
	state: &mut State,
	watch: &mut Watch,
) -> Result<Option<(UpgradePlan, ChainStatus)>> {
	watch.ticker.tick().await;

	let status = match rpc.status(cfg.finality).await {
		Ok(s) => {
			if !watch.rpc_up {
				info!("node RPC reachable at {}: height {}, spec_version {}", rpc.url(), s.height, s.spec_version);
				watch.rpc_up = true;
			}
			s
		}
		Err(e) => {
			if watch.rpc_up {
				warn!("node RPC unreachable: {e:#}");
				watch.rpc_up = false;
			} else {
				debug!("node RPC not reachable yet: {e:#}");
			}
			return Ok(None);
		}
	};

	let spec_changed = state.last_observed.as_ref().is_none_or(|o| o.status.spec_version != status.spec_version);
	if spec_changed {
		info!("runtime spec_version is {} at height {}", status.spec_version, status.height);
	}
	state.observe(status);
	if spec_changed || watch.last_persist.elapsed() >= PERSIST_EVERY {
		state.save(&layout.state_file())?;
		watch.last_persist = Instant::now();
	}

	let plans = layout.load_plans();
	let Some(plan) = next_triggered(&plans, &state.applied_names(), &status) else {
		return Ok(None);
	};

	// Make sure the new binary is ready *before* stopping the old node. If
	// it is not, keep the old node running and retry every tick, so the
	// operator can drop the binary in place without a restart.
	match prepare_binary(cfg, layout, plan, cfg.allow_download).await {
		Ok(_) => Ok(Some((plan.clone(), status))),
		Err(e) => {
			if watch.nagged.get(&plan.name).is_none_or(|t| t.elapsed() >= NAG_EVERY) {
				error!(
					"upgrade {:?} has triggered but its binary is not ready: {e:#}. \
					 The current node keeps running; install the binary at {} to proceed",
					plan.name,
					layout.upgrade_bin(&plan.name).display()
				);
				watch.nagged.insert(plan.name.clone(), Instant::now());
			}
			Ok(None)
		}
	}
}

/// Send `sig`, wait up to `grace`, then SIGKILL.
async fn stop_node(child: &mut Child, sig: Signal, grace: Duration) -> Result<ExitStatus> {
	if let Some(pid) = child.id() {
		if let Err(e) = nix::sys::signal::kill(Pid::from_raw(pid as i32), sig) {
			warn!("failed to send {sig} to node (pid {pid}): {e}");
		}
	}
	match timeout(grace, child.wait()).await {
		Ok(status) => Ok(status?),
		Err(_) => {
			warn!("node did not exit within {}; killing it", humantime::format_duration(grace));
			child.kill().await?;
			Ok(child.wait().await?)
		}
	}
}

fn exit_code(status: ExitStatus) -> i32 {
	status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Ensure the plan's binary exists (downloading it if allowed) and runs.
pub async fn prepare_binary(
	cfg: &Config,
	layout: &Layout,
	plan: &UpgradePlan,
	allow_download: bool,
) -> Result<PathBuf> {
	let bin = layout.upgrade_bin(&plan.name);
	if !bin.exists() {
		match (&plan.info, allow_download) {
			(Some(info), true) => {
				Downloader::new(cfg.download_must_have_checksum)?
					.install(info, &cfg.name, &bin)
					.await
					.with_context(|| format!("downloading binary for upgrade {:?}", plan.name))?;
			}
			(Some(_), false) => {
				bail!("{} does not exist and DAEMON_ALLOW_DOWNLOAD_BINARIES is disabled", bin.display())
			}
			(None, _) => bail!("{} does not exist and the plan has no download info", bin.display()),
		}
	}
	if !is_executable(&bin) {
		bail!("{} is not an executable file", bin.display());
	}
	check_runs(&bin).await?;
	Ok(bin)
}

/// `<bin> --version` must succeed; catches wrong-architecture or truncated files.
async fn check_runs(bin: &Path) -> Result<()> {
	let out = timeout(
		VERSION_CHECK_TIMEOUT,
		Command::new(bin).arg("--version").stdin(Stdio::null()).kill_on_drop(true).output(),
	)
	.await
	.with_context(|| format!("`{} --version` timed out", bin.display()))?
	.with_context(|| format!("running `{} --version`", bin.display()))?;
	if !out.status.success() {
		bail!("`{} --version` failed ({}): {}", bin.display(), out.status, String::from_utf8_lossy(&out.stderr).trim());
	}
	debug!("{} --version: {}", bin.display(), String::from_utf8_lossy(&out.stdout).trim());
	Ok(())
}

async fn apply_upgrade(
	cfg: &Config,
	layout: &Layout,
	state: &mut State,
	plan: &UpgradePlan,
	status: ChainStatus,
	backup_from: Option<&Path>,
) -> Result<()> {
	if !cfg.restart_delay.is_zero() {
		info!("waiting {} before upgrading", humantime::format_duration(cfg.restart_delay));
		tokio::time::sleep(cfg.restart_delay).await;
	}

	if let Some(data_dir) = backup_from {
		let data_dir = data_dir.to_path_buf();
		let backup_root = cfg.backup_dir.clone().unwrap_or_else(|| cfg.home.join("backups"));
		let name = plan.name.clone();
		tokio::task::spawn_blocking(move || backup_data_dir(&data_dir, &backup_root, &name))
			.await?
			.context("pre-upgrade backup failed; fix it or set UNSAFE_SKIP_BACKUP=true")?;
	}

	if let Some(script) = &cfg.preupgrade_script {
		run_preupgrade(cfg, layout, script, plan, status).await?;
	}

	let from = layout.current_name()?;
	layout.set_current(Some(&plan.name))?;
	state.record_applied(&plan.name, Some(status));
	state.save(&layout.state_file())?;
	info!("upgraded {from} -> {} ({})", plan.name, layout.current_link().display());
	Ok(())
}

async fn run_preupgrade(
	cfg: &Config,
	layout: &Layout,
	script: &Path,
	plan: &UpgradePlan,
	status: ChainStatus,
) -> Result<()> {
	let script = if script.is_absolute() { script.to_path_buf() } else { layout.root().join(script) };
	info!("running pre-upgrade script {}", script.display());
	let result = Command::new(&script)
		.arg(&plan.name)
		.arg(status.height.to_string())
		.env("DAEMON_HOME", &cfg.home)
		.env("DAEMON_NAME", &cfg.name)
		.env("MIDNIGHT_NODE_UPDATER_UPGRADE_NAME", &plan.name)
		.env("MIDNIGHT_NODE_UPDATER_UPGRADE_HEIGHT", status.height.to_string())
		.env("MIDNIGHT_NODE_UPDATER_UPGRADE_SPEC_VERSION", status.spec_version.to_string())
		.env("MIDNIGHT_NODE_UPDATER_UPGRADE_BIN", layout.upgrade_bin(&plan.name))
		.stdin(Stdio::null())
		.status()
		.await
		.with_context(|| format!("running pre-upgrade script {}", script.display()))?;
	if !result.success() {
		bail!("pre-upgrade script {} failed with {result}; upgrade aborted", script.display());
	}
	Ok(())
}

/// Warn at startup about pending upgrades that would fail when triggered.
fn warn_unprepared_plans(cfg: &Config, layout: &Layout, state: &State) {
	let applied = state.applied_names();
	for plan in layout.load_plans().iter().filter(|p| !applied.contains(&p.name)) {
		let bin = layout.upgrade_bin(&plan.name);
		if bin.exists() {
			info!("pending upgrade {:?}: {} -> {}", plan.name, plan.describe_trigger(), bin.display());
		} else if cfg.allow_download && has_download_info(plan.info.as_ref()) {
			info!("pending upgrade {:?}: {} (binary will be downloaded)", plan.name, plan.describe_trigger());
		} else {
			warn!(
				"pending upgrade {:?} ({}) has no binary at {} and cannot be downloaded; \
				 run `midnight-node-updater prepare-upgrade` or install it before the trigger",
				plan.name,
				plan.describe_trigger(),
				bin.display()
			);
		}
	}
}
