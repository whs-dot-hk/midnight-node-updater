//! End-to-end: real `midnight-node-updater` binary, fake `midnight-node` shell scripts
//! and a mock Substrate JSON-RPC endpoint whose height / spec_version the test
//! controls.

use std::{
	fs,
	io::{BufRead, BufReader, Read, Write},
	net::TcpListener,
	os::unix::fs::PermissionsExt,
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
	sync::{
		Arc,
		atomic::{AtomicU32, AtomicU64, Ordering},
	},
	thread,
	time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_midnight-node-updater");

struct MockChain {
	height: Arc<AtomicU64>,
	spec: Arc<AtomicU32>,
	url: String,
}

impl MockChain {
	fn start(spec_version: u32) -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let height = Arc::new(AtomicU64::new(1));
		let spec = Arc::new(AtomicU32::new(spec_version));
		let (h, s) = (height.clone(), spec.clone());
		thread::spawn(move || {
			for stream in listener.incoming() {
				let Ok(mut stream) = stream else { continue };
				let mut reader = BufReader::new(stream.try_clone().unwrap());
				let mut len = 0usize;
				loop {
					let mut line = String::new();
					if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
						break;
					}
					if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
						len = v.trim().parse().unwrap();
					}
				}
				let mut body = vec![0; len];
				if reader.read_exact(&mut body).is_err() {
					continue;
				}
				let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
				let result = match req["method"].as_str().unwrap() {
					"chain_getBlockHash" | "chain_getFinalizedHead" => serde_json::json!("0xabc"),
					"chain_getHeader" => {
						serde_json::json!({ "number": format!("0x{:x}", h.load(Ordering::SeqCst)) })
					}
					"state_getRuntimeVersion" => {
						serde_json::json!({ "specName": "midnight", "specVersion": s.load(Ordering::SeqCst) })
					}
					m => panic!("unexpected method {m}"),
				};
				let resp = serde_json::json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }).to_string();
				let _ = write!(
					stream,
					"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
					resp.len(),
					resp
				);
			}
		});
		Self { height, spec, url }
	}
}

fn fake_node(dir: &Path, label: &str) -> PathBuf {
	let path = dir.join(format!("node-{label}"));
	fs::write(
		&path,
		format!(
			r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "midnight-node {label}"; exit 0; fi
echo "{label} started $*" >> "$NODE_LOG"
trap 'echo "{label} stopped" >> "$NODE_LOG"; exit 0' INT TERM
while true; do sleep 0.05; done
"#
		),
	)
	.unwrap();
	fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
	path
}

fn visor(home: &Path, rpc: &str) -> Command {
	let mut c = Command::new(BIN);
	c.env_clear()
		.env("PATH", std::env::var("PATH").unwrap())
		.env("DAEMON_HOME", home)
		.env("DAEMON_RPC_URL", rpc)
		.env("DAEMON_POLL_INTERVAL", "100ms")
		.env("DAEMON_SHUTDOWN_GRACE", "5s")
		.env("NODE_LOG", home.join("node.log"));
	c
}

fn ok(cmd: &mut Command) {
	let out = cmd.output().unwrap();
	assert!(out.status.success(), "{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
}

fn wait_for_log(home: &Path, needle: &str, child: &mut Child) {
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		let log = fs::read_to_string(home.join("node.log")).unwrap_or_default();
		if log.contains(needle) {
			return;
		}
		if let Some(status) = child.try_wait().unwrap() {
			panic!("midnight-node-updater exited early ({status}); node log:\n{log}");
		}
		if Instant::now() > deadline {
			let _ = child.kill();
			panic!("timed out waiting for {needle:?}; node log:\n{log}");
		}
		thread::sleep(Duration::from_millis(50));
	}
}

fn current_target(home: &Path) -> PathBuf {
	fs::read_link(home.join("midnight-node-updater/current")).unwrap()
}

#[test]
fn upgrades_on_height_then_spec_version() {
	let tmp = tempfile::tempdir().unwrap();
	let home = tmp.path().join("home");
	let data = tmp.path().join("chain-data");
	fs::create_dir_all(data.join("chains/testnet/db")).unwrap();
	fs::write(data.join("chains/testnet/db/CURRENT"), "db").unwrap();
	let chain = MockChain::start(1_000_000);

	ok(visor(&home, &chain.url).arg("init").arg(fake_node(tmp.path(), "genesis")));
	ok(visor(&home, &chain.url)
		.args(["add-upgrade", "node-1.1.0"])
		.arg(fake_node(tmp.path(), "v1.1"))
		.args(["--upgrade-height", "10"]));
	ok(visor(&home, &chain.url)
		.args(["add-upgrade", "Node-2.0.0"])
		.arg(fake_node(tmp.path(), "v2.0"))
		.args(["--spec-version", "2000000"]));
	assert_eq!(current_target(&home), PathBuf::from("genesis"));

	let mut run = visor(&home, &chain.url)
		.args(["run", "--chain", "testnet", "--base-path"])
		.arg(&data)
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();

	wait_for_log(&home, "genesis started --chain testnet --base-path", &mut run);

	chain.height.store(10, Ordering::SeqCst);
	wait_for_log(&home, "v1.1 started --chain testnet", &mut run);
	assert_eq!(current_target(&home), PathBuf::from("upgrades/node-1.1.0"));
	let log = fs::read_to_string(home.join("node.log")).unwrap();
	assert!(log.contains("genesis stopped"), "old node was not stopped gracefully:\n{log}");
	let backups: Vec<_> = fs::read_dir(home.join("backups")).unwrap().flatten().collect();
	assert_eq!(backups.len(), 1);
	assert_eq!(fs::read_to_string(backups[0].path().join("chains/testnet/db/CURRENT")).unwrap(), "db");

	chain.height.store(11, Ordering::SeqCst);
	chain.spec.store(2_000_000, Ordering::SeqCst);
	wait_for_log(&home, "v2.0 started", &mut run);
	assert_eq!(current_target(&home), PathBuf::from("upgrades/node-2.0.0"));

	nix::sys::signal::kill(nix::unistd::Pid::from_raw(run.id() as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
	let status = run.wait().unwrap();
	assert!(status.success(), "midnight-node-updater exit: {status}");
	assert!(fs::read_to_string(home.join("node.log")).unwrap().contains("v2.0 stopped"));

	let out = visor(&home, &chain.url).args(["status", "--json", "--offline"]).output().unwrap();
	let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
	assert_eq!(report["current"], "node-2.0.0");
	let applied: Vec<_> = report["applied"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
	assert_eq!(applied, ["node-1.1.0", "node-2.0.0"]);
}

#[test]
fn keeps_old_node_running_until_binary_appears() {
	let tmp = tempfile::tempdir().unwrap();
	let home = tmp.path().join("home");
	let chain = MockChain::start(1);

	ok(visor(&home, &chain.url).arg("init").arg(fake_node(tmp.path(), "genesis")));
	// Plan registered with only a (disallowed) download URL: no local binary.
	ok(visor(&home, &chain.url).args([
		"add-upgrade",
		"v2",
		"--immediate",
		"--url",
		"https://example.invalid/midnight-node?checksum=sha256:0000000000000000000000000000000000000000000000000000000000000000",
	]));

	let mut run = visor(&home, &chain.url)
		.env("UNSAFE_SKIP_BACKUP", "true")
		.args(["run", "--dev"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	wait_for_log(&home, "genesis started --dev", &mut run);
	thread::sleep(Duration::from_millis(500));
	assert!(run.try_wait().unwrap().is_none(), "supervisor must keep the old node running");
	assert_eq!(current_target(&home), PathBuf::from("genesis"));

	// Operator drops the binary in; the next poll picks it up.
	let dest = home.join("midnight-node-updater/upgrades/v2/bin/midnight-node");
	fs::create_dir_all(dest.parent().unwrap()).unwrap();
	fs::copy(fake_node(tmp.path(), "v2"), &dest).unwrap();
	wait_for_log(&home, "v2 started --dev", &mut run);
	assert_eq!(current_target(&home), PathBuf::from("upgrades/v2"));

	nix::sys::signal::kill(nix::unistd::Pid::from_raw(run.id() as i32), nix::sys::signal::Signal::SIGINT).unwrap();
	assert!(run.wait().unwrap().success());
}

#[test]
fn upgrades_before_launch_when_trigger_already_passed() {
	let tmp = tempfile::tempdir().unwrap();
	let home = tmp.path().join("home");
	// RPC deliberately unreachable: the decision comes from persisted state.
	let rpc = "http://127.0.0.1:1";

	ok(visor(&home, rpc).arg("init").arg(fake_node(tmp.path(), "genesis")));
	ok(visor(&home, rpc).args(["add-upgrade", "v2"]).arg(fake_node(tmp.path(), "v2")).args(["--spec-version", "5"]));
	fs::write(
		home.join("midnight-node-updater/state.json"),
		r#"{"applied":[],"last_observed":{"height":42,"spec_version":5,"observed_at":"2026-01-01T00:00:00Z"}}"#,
	)
	.unwrap();

	let mut run = visor(&home, rpc)
		.env("UNSAFE_SKIP_BACKUP", "true")
		.args(["run", "--dev"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	wait_for_log(&home, "v2 started", &mut run);
	let log = fs::read_to_string(home.join("node.log")).unwrap();
	assert!(!log.contains("genesis started"), "old binary must not be launched:\n{log}");
	nix::sys::signal::kill(nix::unistd::Pid::from_raw(run.id() as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
	assert!(run.wait().unwrap().success());
}

#[test]
fn launches_current_binary_when_triggered_upgrade_is_not_installed() {
	let tmp = tempfile::tempdir().unwrap();
	let home = tmp.path().join("home");
	let rpc = "http://127.0.0.1:1";

	ok(visor(&home, rpc).arg("init").arg(fake_node(tmp.path(), "genesis")));
	// Plan is registered by URL only, downloads are disabled, trigger already passed.
	ok(visor(&home, rpc).args([
		"add-upgrade",
		"v2",
		"--spec-version",
		"5",
		"--url",
		"https://example.invalid/midnight-node?checksum=sha256:0000000000000000000000000000000000000000000000000000000000000000",
	]));
	fs::write(
		home.join("midnight-node-updater/state.json"),
		r#"{"applied":[],"last_observed":{"height":42,"spec_version":5,"observed_at":"2026-01-01T00:00:00Z"}}"#,
	)
	.unwrap();

	// Must not crash-loop: the old binary is launched and the supervisor stays up.
	let mut run = visor(&home, rpc)
		.env("UNSAFE_SKIP_BACKUP", "true")
		.args(["run", "--dev"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	wait_for_log(&home, "genesis started --dev", &mut run);
	thread::sleep(Duration::from_millis(300));
	assert!(run.try_wait().unwrap().is_none());
	assert_eq!(current_target(&home), PathBuf::from("genesis"));
	nix::sys::signal::kill(nix::unistd::Pid::from_raw(run.id() as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
	assert!(run.wait().unwrap().success());
}
