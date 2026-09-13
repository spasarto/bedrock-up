# Plan: `bedrock-supervisor` and the `bedrock-up` library seam

## Goal

Graceful, automated updates: warn players, cleanly stop the server, apply the
update while nothing holds the files open, and start it again.

Bedrock Dedicated Server has no RCON — that is Java Edition. Its only control
channel is **stdin**: `stop` for a save-and-exit, `say <msg>` to warn players.
Only a parent process owns a child's stdin, so the supervisor must be the
process that spawns `bedrock_server`. That single fact drives the whole design.

## Shape

A two-crate Cargo workspace in this repo.

```text
Cargo.toml                      # virtual workspace manifest
Cargo.lock
crates/
  bedrock-up/                   # lib + bin, CLI unchanged
    src/{lib,main,args,config,error,updater,process}.rs
  bedrock-supervisor/           # bin
    src/{main,args,server,supervisor}.rs
    src/bin/fake-bedrock-server.rs
```

`bedrock-supervisor` depends on `bedrock-up` as a **library**, not as a
subprocess. The seam between "download" and "apply" is a Rust type, checked at
compile time, rather than a JSON-and-exit-code contract to hand-maintain.

Same repo rather than a second one: atomic commits across the seam, one CI
pipeline, no version skew between two separately-installed binaries. The crates
stay independently scoped and produce independent release artifacts.

### Hard constraint: the CLI does not change

`bedrock-up`'s flags, arguments, and exit codes (`0` current / `1` error /
`2` update applied) stay exactly as they are today. Anyone running it from a
scheduled task or a wrapper script sees no difference. The library extraction is
an internal refactor; the supervisor is strictly additive.

Consequence: **no subcommands in Phase 1.** Since the supervisor links the
library, `download`/`apply` subcommands buy nothing right now. They are deferred
to Phase 5 as optional standalone conveniences, and would be additive when added.

## The API seam

Today `update()` (`src/updater.rs:17`) welds check, download, apply, and cache
into one call. The supervisor needs a seam between download and apply, because
the download must happen **while the server is still running** — that is the
difference between roughly 90 seconds of downtime and roughly 5.

The split is a typestate chain, so the phases cannot be called out of order:

```rust
pub fn check(config: &UpdateConfig) -> Result<CheckOutcome, UpdateError>;

pub enum CheckOutcome {
    UpToDate { version: String },
    UpdateAvailable(AvailableUpdate),
}

impl AvailableUpdate {
    pub fn version(&self) -> &str;
    /// Network. Server stays up.
    pub fn download(self) -> Result<StagedUpdate, UpdateError>;
}

impl StagedUpdate {
    /// Filesystem only. Fast. Server should be stopped.
    pub fn apply(self, config: &UpdateConfig) -> Result<UpdateOutcome, UpdateError>;
}
```

Supervisor usage:

```rust
if let CheckOutcome::UpdateAvailable(update) = bedrock_up::check(&config)? {
    let staged = update.download()?;      // server still serving players
    server.graceful_stop()?;              // warn, stop, wait
    staged.apply(&config)?;               // seconds
    server.start()?;
}
```

Notes on the types:

- `StagedUpdate` implements `Drop` to remove its temp archive. Today that
  cleanup is an explicit `remove_file` on one code path; under a supervisor that
  can bail between download and apply, it has to be automatic.
- `UpdateOutcome` loses `AlreadyCurrent` — that moves to `CheckOutcome::UpToDate`.
  It keeps `Updated` and `UpdatedPendingRestart`.
- `apply_update`'s in-use staging (`create_replacing_in_use`, `sweep_stale_files`)
  **stays.** Under the supervisor nothing should be in use, but it remains the
  safety net for the standalone CLI and for a graceful stop that timed out.

### `UpdateConfig` — a clap-free config type

The library must not make callers construct clap structs. `UpdateArgs` stays the
CLI's parsed form and gets `impl From<UpdateArgs> for UpdateConfig`. Tilde
expansion (`shellexpand`) moves out of `updater.rs` internals and into that
conversion, so paths are resolved once at the boundary.

```rust
pub struct UpdateConfig {
    pub download_type: DownloadType,
    pub server_path: PathBuf,
    pub cache_path: PathBuf,
    pub exclude: Vec<String>,
    pub force: bool,
}
```

### `UpdateError` — replacing `Result<_, String>`

A supervisor running unattended must distinguish "transient, retry in six hours"
from "broken, stop trying and shout." A `String` cannot express that.

```rust
pub enum UpdateError {
    Network(reqwest::Error),        // retryable
    UpstreamFormat(String),         // API shape changed — needs a human
    Download { url: String, source: ... },
    Archive(zip::result::ZipError),
    Io { path: PathBuf, source: std::io::Error },
    InUse { path: PathBuf },
    CacheWrite { path: PathBuf, source: std::io::Error },  // update DID apply
}

impl UpdateError { pub fn is_retryable(&self) -> bool; }
```

`CacheWrite` is deliberately distinct: the update landed on disk and only the
bookkeeping failed, so the caller must not treat it as a failed update and retry
the whole thing.

Derived with `thiserror` — `#[derive(Error)]` plus `#[error("...")]` per variant
and `#[from]` on the wrapped source errors. It is compile-time only, adds no
runtime weight, and keeps `Display` and `source()` correct as variants get added
later, which the hand-rolled version would not do on its own.

### Logging

`updater.rs` currently has roughly 20 `println!`/`eprintln!` calls. A library
should not own the process's stdout. Swap them for `log::{info, warn, error}`;
the `bedrock-up` binary initializes `env_logger`, and the supervisor initializes
its own logger and gets the updater's output routed into it for free.

**Watch out:** `env_logger` writes to stderr with a `[timestamp LEVEL module]`
prefix, while today's progress output is bare text on stdout. Configure the
builder with a minimal format and `Target::Stdout` so the CLI's visible behavior
stays as close to identical as practical. The exit codes — the actual documented
contract — are untouched either way.

## `bedrock-supervisor`

### `server.rs` — one supervised instance

Spawn with `Stdio::piped()` on all three handles. Launch details vary by
platform and are easy to get wrong:

| Download type | Command | cwd | Env |
| --- | --- | --- | --- |
| `windows`, `preview-windows` | `bedrock_server.exe` | server dir | — |
| `linux`, `preview-linux` | `./bedrock_server` | server dir | `LD_LIBRARY_PATH=.` |
| `server-jar` | `java -jar server.jar` | server dir | — |

Overridable with `--server-exe` and trailing `-- <args>`.

The graceful stop sequence:

1. `say Server restarting for update in 60s` → sleep → repeat at 30s, 10s.
   Configurable via `--warn-at 60,30,10`; empty means stop immediately.
2. Write `stop\n` to child stdin and flush.
3. Wait for exit, up to `--stop-timeout` (default 60s — world saves are not
   instant, and a large world on spinning disk can take a while).
4. On timeout: `kill()`, log loudly, and let `apply_update`'s staging path cover
   the files the dying process still held open.

### `supervisor.rs` — the run loop

Threads, not async. `reqwest` is already `blocking`, and adding tokio to
coordinate three threads is a large tax for no gain. Main loop owns an
`mpsc::Receiver<Event>` fed by:

- child stdout reader → `Event::ConsoleLine` (relay to our stdout / log file)
- child waiter → `Event::ChildExited(status)`
- our stdin reader → `Event::Command(line)`, relayed to child stdin so the
  operator console still works interactively. Detect and skip cleanly when stdin
  is not a TTY, as under a service manager.
- ticker → `Event::CheckUpdate` every `--check-interval` (default 6h; `0`
  disables it, leaving a supervisor that only babysits and restarts)
- signal handler → `Event::Shutdown`

**Console output is a pure relay — nothing parses it.** With player-aware
deferral off the table (see Phase 5), no decision the supervisor makes depends
on the contents of a console line. Shutdown completion is detected by process
exit, not by matching `Quit correctly` in stdout; player counts are never read.
That keeps the supervisor immune to Mojang changing its log format, which is an
unversioned surface that has shifted before. Hold this line: if a future feature
wants to parse console text, it is buying a real maintenance liability and
should justify it.

On `CheckUpdate`: `check()` → download → warn → stop → `apply()` → respawn.
A failed check logs and waits for the next tick; **never take the server down
for a check that failed.** On unexpected `ChildExited`: restart with exponential
backoff and a give-up threshold.

**Signals:** the `ctrlc` crate covers Ctrl+C on both platforms, and SIGTERM on
Unix with its `termination` feature. Ctrl+C must mean *graceful child stop, then
exit* — never instant death, which would drop the world without saving. A real
Windows service needs `windows-service` for the SCM stop handler; deferred to
Phase 4, relying on NSSM or `sc.exe` first.

**Startup safety:** `process.rs` loses its current job under the supervisor but
gains a better one. At startup, use `find_server_processes` to refuse to launch
if a `bedrock_server` is already running out of that directory. Two servers on
one world is data loss.

### Startup: `--update-on-start`

Opt-in, **off by default**. Starting the supervisor starts the server, promptly
and predictably; the ticker picks up any pending update within
`--check-interval` anyway.

Defaulting it on would be wrong in the two cases that matter most. A service
manager restarting the supervisor — after a host reboot, a crash, or an operator
fixing a config typo — would cascade into an update every time, and an urgent
"restart it now" would instead sit through a download. Neither is what someone
typing `systemctl restart` expects.

When the flag *is* set, the startup path is genuinely simpler than the ticker's,
because there is no running server yet:

```text
check() → download() → apply() → start()
```

No `say` countdown, no graceful stop, no stop-timeout, no staging — the whole
warn-and-drain sequence is skipped because there are no players to warn and no
files held open. Worth implementing as its own short path rather than
contorting the ticker's state machine to handle a nonexistent child.

**A failed check never blocks startup.** Network down, upstream API changed,
disk full — log it and start the server on whatever build is on disk. Same
principle as the ticker: an update check failing must not cost availability.

That makes three flags covering the reasonable startup intents:

| Want | Flags |
| --- | --- |
| Start now, update later on the tick | *(default)* |
| Be current before opening to players | `--update-on-start` |
| Start now, never auto-update | `--check-interval 0` |

### `--server-path` and instances

One supervisor process per server directory. For multiple servers, run multiple
supervisors. Multi-instance in one process is a config-file feature and is not
worth it yet.

## Testing

The blocker is that CI cannot run a real Bedrock server. `src/bin/fake-bedrock-server.rs`
is a stub that prints a startup banner, echoes stdin, and exits 0 on `stop`.
Cargo exposes it to integration tests as `env!("CARGO_BIN_EXE_fake-bedrock-server")`.

That covers, on both Windows and Linux runners with no network and no Mojang
binary:

- spawn → warn → `stop` → clean exit
- stop-timeout → kill (a stub variant that deliberately ignores `stop`)
- crash → backoff → restart
- console relay in both directions

The existing `updater.rs` tests keep passing through Phase 1 unchanged; that is
the signal that the refactor was faithful.

## New dependencies

`bedrock-up`:

| Crate | Why | Phase |
| --- | --- | --- |
| `thiserror` | derive `UpdateError` | 1 |
| `log` | facade, replacing `println!`/`eprintln!` | 1 |
| `env_logger` | CLI binary only — see feature gating below | 1 |
| `tempfile` | promote dev-dependency → dependency, for unique temp dirs | 1 |

`bedrock-supervisor`: `bedrock-up` (path dependency), `log`, `env_logger`, and
`ctrlc` with its `termination` feature for SIGTERM. `windows-service` is
deferred to Phase 4 and may never be needed if NSSM proves sufficient.

### Feature-gating the CLI dependencies

A lib+bin package shares one `[dependencies]` table, so without care
`bedrock-supervisor` would pull `clap` and `env_logger` transitively just to use
the library. Gate them:

```toml
[features]
default = ["cli"]
cli = ["dep:clap", "dep:env_logger"]

[[bin]]
name = "bedrock-up"
required-features = ["cli"]
```

and have the supervisor depend on it with `default-features = false`.

`DownloadType` moves from `args.rs` to `config.rs`, since `UpdateConfig` needs
it and the library must build without clap. Its `ValueEnum` derive becomes
`#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]`.

Be honest about the payoff: in-workspace this buys little build time, because
Cargo builds the `bedrock-up` binary anyway and compiles `clap` regardless. The
real value is that it makes the "the library is not clap-shaped" rule
**mechanically enforced** rather than a convention someone erodes later — if the
library starts reaching for `UpdateArgs`, the no-default-features build stops
compiling. That is worth the handful of lines on its own.

## Phases

**Phase 1 — workspace and library seam.** No behavior change. Move sources under
`crates/bedrock-up/` with `git mv`, add the virtual workspace manifest, add
`lib.rs`, introduce `UpdateConfig` / `UpdateError`, split `update()` into
`check` → `download` → `apply`, swap to the `log` facade. `main.rs` becomes a
thin CLI shell. Existing tests must pass untouched. Fix CI and README (below).

**Phase 2 — `server.rs`.** Supervisor crate skeleton: spawn, console relay,
graceful stop, the fake-server test suite. No update logic yet; this phase ends
with a binary that can babysit a server and stop it politely.

**Phase 3 — `supervisor.rs`.** The event loop, update orchestration, signal
handling, crash restart with backoff. This is where `bedrock-supervisor` becomes
real.

**Phase 4 — packaging and docs.** systemd unit, NSSM / Windows service notes,
README section, CI artifact updates, optionally `windows-service` for a proper
SCM handler.

**Phase 5 — optional.** `download` / `apply` subcommands on the `bedrock-up`
CLI (useful standalone: pre-download during the day, apply at night). A control
socket — named pipe on Windows, Unix socket on Linux — so
`bedrock-supervisor trigger-update` can force an on-demand check.

**Not planned.** Player-aware deferral — waiting for an empty server rather than
counting down — is dropped. The `say` countdown is enough protection, and
deferral drags in a maximum-deferral escape hatch to stop a popular server from
never updating, which is most of the complexity for the smaller half of the
benefit. Revisit only if the countdown proves disruptive in practice.

## Known breakage from the workspace move

**`cargo install --git` will need a package argument.** Cargo searches the whole
repo for `Cargo.toml` files and errors when more than one package has a binary.
The README instruction becomes:

```shell
cargo install --git https://github.com/spasarto/bedrock-up.git bedrock-up
```

This is unavoidable once the repo ships two binaries. Verify the exact behavior
during Phase 1 and update the README in the same commit.

**CI version extraction breaks.** Both workflows grep the root `Cargo.toml` for
a `version =` line, which a virtual workspace manifest does not have. Fix by putting
the version in `[workspace.package]` at the root and using
`version.workspace = true` in each crate — the existing grep in both workflows
then keeps working unchanged, and the two crates version in lockstep, which is
what we want for a single repo.

**CI artifact paths.** Both workflows upload a single hardcoded binary path.
They need to upload `bedrock-up` and `bedrock-supervisor` separately, and must
not ship `fake-bedrock-server`. Name the binaries explicitly rather than globbing
the release directory.

## Incidental fix worth taking in Phase 1

`fetch_update_zip` writes to `std::env::temp_dir().join(file_name)` using a name
derived from the URL. Two concurrent runs — plausible once a supervisor and a
cron job both exist — collide on that path and corrupt each other. Use a unique
temp directory. `tempfile` is already a dev-dependency and would be promoted to
a real one.

## Decisions

No open questions remain; Phase 1 is ready to start.

- **Same repo, two crates.** Separation of concerns without the cross-repo tax:
  atomic commits across the seam, one CI pipeline, no version skew.
- **Library seam, not a subprocess.** The download/apply boundary is a Rust
  typestate chain checked by the compiler, not a JSON-and-exit-code contract to
  hand-maintain.
- **`bedrock-up`'s CLI does not change.** Same flags, same exit codes. No
  subcommands in Phase 1.
- **Update-on-startup is opt-in, off by default** — see
  [Startup: `--update-on-start`](#startup---update-on-start).
- **Player-aware deferral is dropped;** the `say` countdown is the whole
  player-protection mechanism. Consequently nothing parses console output.
- **`thiserror` is in.** Compile-time only, and keeps `Display`/`source()`
  correct as `UpdateError` grows.
