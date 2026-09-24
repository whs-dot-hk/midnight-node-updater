# midnight-node-updater

A process manager for [midnight-node](https://github.com/midnightntwrk/midnight-node) that automates
node binary switches at chain upgrades.

## How upgrades are detected

Midnight is a Substrate chain, and nothing on chain tells a node when to swap its binary:

- **Runtime upgrades** (WASM, enacted on-chain via governance) need no operator action.
- **Node upgrades** (new host functions, client and networking fixes, hard forks) need a new binary on
  every node, sometimes at a specific block, or once a new runtime is live that the old node cannot run.

So `midnight-node-updater` watches the node itself over JSON-RPC (`chain_getHeader`,
`state_getRuntimeVersion`) and switches binaries when an operator-registered plan triggers on:

| trigger          | fires when                                              | typical use                                    |
|------------------|---------------------------------------------------------|------------------------------------------------|
| `height`         | the best (or finalized) block number reaches it         | coordinated node upgrade at an agreed block    |
| `spec_version`   | the on-chain runtime `spec_version` reaches it          | node release required by a governance runtime upgrade |
| `--immediate`    | the running supervisor notices the plan                 | emergency swap without restarting by hand      |

A plan can have both `height` and `spec_version`; it fires on whichever comes first.

## Install

```sh
cargo install --path .        # installs `midnight-node-updater`
```

## Quick start

```sh
export DAEMON_HOME=/srv/midnight          # holds midnight-node-updater/ (binaries + state)
export DAEMON_DATA_DIR=/srv/midnight/chain  # the node's --base-path, backed up before each upgrade

midnight-node-updater init ./midnight-node        # installs the genesis binary, current -> genesis

# Days before the upgrade, stage the new binary:
midnight-node-updater add-upgrade node-1.1.0 ./midnight-node-1.1.0 --upgrade-height 2500000
# or tie it to the runtime version it supports:
midnight-node-updater add-upgrade node-2.0.0 ./midnight-node-2.0.0 --spec-version 2000000

midnight-node-updater status                      # shows pending upgrades, missing binaries, live height/spec

# Run the node through the supervisor. Everything after `run` goes to midnight-node:
midnight-node-updater run --chain mainnet --base-path /srv/midnight/chain --validator
```

When a plan triggers, `midnight-node-updater`:

1. makes sure `upgrades/<name>/bin/midnight-node` exists (downloading it if allowed) and that
   `midnight-node --version` runs. **If the binary isn't ready, the old node keeps running.** The error
   repeats every minute, and the supervisor picks the binary up on the next poll once you put it in place;
2. stops the node with SIGINT, waits up to `DAEMON_SHUTDOWN_GRACE`, then SIGKILL;
3. waits `DAEMON_RESTART_DELAY`, backs up the data directory, runs the pre-upgrade script;
4. atomically repoints `current` to `upgrades/<name>` and records the upgrade in `state.json`;
5. restarts the node with the same arguments (unless `DAEMON_RESTART_AFTER_UPGRADE=false`).

The last observed height and spec_version are saved to disk. If the old node crashes after a trigger is
reached (for example, because it cannot execute a new runtime), the next start applies the upgrade
*before* launching anything. When several plans have triggered, they are applied one at a time in
(height, spec_version) order.

## Folder layout

```text
$DAEMON_HOME/midnight-node-updater/
├── current -> genesis | upgrades/<name>
├── genesis/bin/midnight-node
├── upgrades/<name>/
│   ├── bin/midnight-node
│   └── upgrade-info.json
└── state.json               # applied upgrades + last observed chain status
```

`upgrades/<name>/upgrade-info.json`:

```json
{
  "name": "node-1.1.0",
  "height": 2500000,
  "spec_version": 1001000,
  "info": {
    "binaries": {
      "linux/amd64": "https://example.com/midnight-node-linux-amd64.tar.gz?checksum=sha256:<hex>",
      "linux/arm64": "https://example.com/midnight-node-linux-arm64.tar.gz?checksum=sha256:<hex>"
    }
  }
}
```

`info` holds a `binaries` map keyed by `<os>/<arch>` (or `any`). It can also be a URL string pointing to a JSON document of that shape.
Download URLs may point to a raw executable, a `.tar.gz` or a `.zip`. Archives are searched for
`bin/midnight-node`, then `midnight-node`, then any file named `midnight-node`. You can drop plan files in
by hand (or with configuration management) instead of using `add-upgrade`. The running supervisor
rescans the directory on every poll. A malformed plan is logged and ignored and never stops the node.

## Commands

| command | description |
|---|---|
| `run <node args…>` | Run and supervise the node. `midnight-node-updater run --help` shows midnight-node's help. |
| `init <path>` | Create the layout and install the genesis binary. |
| `add-upgrade <name> [path] [--upgrade-height N] [--spec-version N] [--immediate] [--url [os/arch=]URL]… [--force]` | Register a plan. Omit `path` if `--url` is given. |
| `status` (alias `show-upgrade-info`) `[--json] [--offline]` | Current version, plans, binary readiness and chain status. |
| `prepare-upgrade` | Download and verify binaries for every pending plan now. Always allowed, since the operator runs it explicitly. |
| `config` | Print the effective configuration. |
| `version` | Print midnight-node-updater's version and `current/bin/midnight-node --version`. |

## Configuration

Settings come from environment variables. You can also pass a TOML file (`--config` or
`MIDNIGHT_NODE_UPDATER_CONFIG`) whose keys are the same names (`DAEMON_HOME = "/srv/midnight"`). Environment
variables override the file.

| variable | default | notes |
|---|---|---|
| `DAEMON_HOME` | required | absolute path; holds `midnight-node-updater/` |
| `DAEMON_NAME` | `midnight-node` | binary name under `bin/` |
| `DAEMON_RPC_URL` | `http://127.0.0.1:9944` | node JSON-RPC; `ws://` is accepted and converted |
| `DAEMON_POLL_INTERVAL` | `5s` | block time is 6s |
| `DAEMON_UPGRADE_FINALITY` | `best` | `finalized` evaluates triggers against the GRANDPA-finalized block |
| `DAEMON_ALLOW_DOWNLOAD_BINARIES` | `false` | auto-download at trigger time (not recommended for validators) |
| `DAEMON_DOWNLOAD_MUST_HAVE_CHECKSUM` | `true` | unverified binaries are refused |
| `DAEMON_RESTART_AFTER_UPGRADE` | `true` | `false`: exit 0 after switching |
| `DAEMON_RESTART_DELAY` | `0s` | pause between stopping the node and upgrading |
| `DAEMON_SHUTDOWN_GRACE` | `60s` | SIGINT → SIGKILL timeout; Substrate flushes its DB on shutdown |
| `DAEMON_DATA_DIR` | from `--base-path`/`-d`, then `$BASE_PATH` | directory backed up before upgrades |
| `DAEMON_DATA_BACKUP_DIR` | `$DAEMON_HOME/backups` | backups go to `data-backup-<time>-<name>/` |
| `UNSAFE_SKIP_BACKUP` | `false` | full chain DB copies can be large; set `true` if you have snapshots |
| `DAEMON_PREUPGRADE_SCRIPT` | none | run with args `<name> <height>` after the node stops; relative paths resolve under `midnight-node-updater/`; non-zero exit aborts |
| `MIDNIGHT_NODE_UPDATER_DISABLE_LOGS` | `false` | silence supervisor logs (the node's output is unaffected) |
| `MIDNIGHT_NODE_UPDATER_LOG` | `info` | `tracing` filter for supervisor logs |

The pre-upgrade script also receives `MIDNIGHT_NODE_UPDATER_UPGRADE_NAME`, `MIDNIGHT_NODE_UPDATER_UPGRADE_HEIGHT`,
`MIDNIGHT_NODE_UPDATER_UPGRADE_SPEC_VERSION` and `MIDNIGHT_NODE_UPDATER_UPGRADE_BIN`.

With backups enabled, `run` refuses to start if it cannot work out the data directory. That's
deliberate: a backup that silently doesn't happen is worse than none.

## Signals and exit codes

- The node runs in its own process group, so a terminal Ctrl-C reaches only `midnight-node-updater`, which
  forwards it once. On SIGINT/SIGTERM, `midnight-node-updater` forwards the signal and exits with the node's
  exit code.
- If the node exits on its own and no upgrade has triggered, `midnight-node-updater` exits with the same code
  (let systemd restart it). A node killed by a signal gives `128 + signo`.
- On Linux the node gets `PR_SET_PDEATHSIG`, so it is not orphaned if `midnight-node-updater` is SIGKILLed.

## systemd

```ini
[Unit]
Description=Midnight node (midnight-node-updater)
After=network-online.target
Wants=network-online.target

[Service]
User=midnight
Environment=DAEMON_HOME=/srv/midnight
Environment=DAEMON_DATA_DIR=/srv/midnight/chain
Environment=DAEMON_RPC_URL=http://127.0.0.1:9944
EnvironmentFile=-/etc/midnight/node.env
ExecStart=/usr/local/bin/midnight-node-updater run --chain mainnet --base-path /srv/midnight/chain --validator
Restart=always
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=90
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

Keep `TimeoutStopSec` above `DAEMON_SHUTDOWN_GRACE`. The node's RPC must be reachable at
`DAEMON_RPC_URL`. The defaults (`--rpc-port 9944`, localhost) work without extra flags.

## Development

```sh
cargo test     # unit tests + end-to-end tests with fake nodes and a mock Substrate RPC
cargo clippy --all-targets
```
