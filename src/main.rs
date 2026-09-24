use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use midnight_node_updater::{
	commands::{self, AddUpgrade},
	config::Config,
	supervisor,
};
use tracing_subscriber::EnvFilter;

/// Process manager for midnight-node that switches node binaries at chain
/// upgrades.
#[derive(Parser)]
#[command(name = "midnight-node-updater", version, about)]
struct Cli {
	/// TOML config file whose keys are the environment variable names
	/// (e.g. `DAEMON_HOME = "/srv/midnight"`). Environment variables win.
	#[arg(long, env = "MIDNIGHT_NODE_UPDATER_CONFIG")]
	config: Option<PathBuf>,

	#[command(subcommand)]
	command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// Run the node with the given arguments, upgrading it when a plan triggers.
	/// Everything after `run` is passed to the node verbatim.
	#[command(disable_help_flag = true)]
	Run {
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		args: Vec<String>,
	},
	/// Create $DAEMON_HOME/midnight-node-updater and install <path> as the genesis binary.
	Init { path: PathBuf },
	/// Register an upgrade plan and its binary.
	AddUpgrade {
		/// Upgrade name (becomes upgrades/<name>).
		name: String,
		/// Path to the new node binary. Optional when --url is given.
		path: Option<PathBuf>,
		/// Switch once the chain reaches this block height.
		#[arg(long = "upgrade-height")]
		height: Option<u64>,
		/// Switch once the on-chain runtime reaches this spec_version.
		#[arg(long)]
		spec_version: Option<u32>,
		/// Switch as soon as a running supervisor notices the plan.
		#[arg(long)]
		immediate: bool,
		/// Download URL, `[<os>/<arch>=]<url>?checksum=sha256:<hex>`. Repeatable.
		#[arg(long = "url")]
		urls: Vec<String>,
		/// Overwrite an existing plan with the same name.
		#[arg(long)]
		force: bool,
	},
	/// Show the current version, registered upgrades and chain status.
	#[command(alias = "show-upgrade-info")]
	Status {
		#[arg(long)]
		json: bool,
		/// Don't query the node RPC.
		#[arg(long)]
		offline: bool,
	},
	/// Download and verify binaries for all pending upgrades now.
	PrepareUpgrade,
	/// Print the effective configuration.
	Config,
	/// Print midnight-node-updater's version and the current node binary's version.
	Version,
}

fn main() {
	let cli = Cli::parse();
	let code = match real_main(cli) {
		Ok(code) => code,
		Err(e) => {
			eprintln!("midnight-node-updater: error: {e:#}");
			1
		}
	};
	std::process::exit(code);
}

fn real_main(cli: Cli) -> Result<i32> {
	let cfg = Config::load(cli.config.as_deref())?;
	if !cfg.disable_logs {
		tracing_subscriber::fmt()
			.with_env_filter(
				EnvFilter::try_from_env("MIDNIGHT_NODE_UPDATER_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
			)
			.with_target(false)
			.with_writer(std::io::stderr)
			.init();
	}
	// Current-thread runtime: the node is spawned from, and PR_SET_PDEATHSIG is
	// tied to, a thread that lives as long as the process.
	let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
	let _span = tracing::info_span!("midnight-node-updater").entered();

	match cli.command {
		Cmd::Run { args } => return rt.block_on(supervisor::run(cfg, args)),
		Cmd::Init { path } => commands::init(&cfg, &path)?,
		Cmd::AddUpgrade { name, path, height, spec_version, immediate, urls, force } => {
			commands::add_upgrade(
				&cfg,
				AddUpgrade { name: &name, exe: path.as_deref(), height, spec_version, immediate, urls: &urls, force },
			)?;
		}
		Cmd::Status { json, offline } => rt.block_on(commands::status(&cfg, json, !offline))?,
		Cmd::PrepareUpgrade => rt.block_on(commands::prepare_upgrade(&cfg))?,
		Cmd::Config => println!("{}", serde_json::to_string_pretty(&cfg)?),
		Cmd::Version => rt.block_on(commands::version(&cfg))?,
	}
	Ok(0)
}
