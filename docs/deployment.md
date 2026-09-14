# Deploying `bedrock-supervisor`

`bedrock-supervisor` is a long-running foreground process: it spawns
`bedrock_server`, owns its stdin/stdout, and handles graceful stop/update/
restart itself (see [`supervisor-plan.md`](supervisor-plan.md)). It expects a
process supervisor above it — systemd on Linux, NSSM on Windows — to start it
on boot, restart it if it exits unexpectedly, and forward a stop request as a
signal it already understands.

## Linux (systemd)

Copy [`packaging/systemd/bedrock-supervisor.service`](../packaging/systemd/bedrock-supervisor.service)
to `/etc/systemd/system/`, edit the `User`, `WorkingDirectory`, and
`ExecStart` values for your setup, then:

```shell
sudo systemctl daemon-reload
sudo systemctl enable --now bedrock-supervisor
```

Stop it with `sudo systemctl stop bedrock-supervisor` — systemd sends
`SIGTERM`, which `bedrock-supervisor` handles exactly like Ctrl+C: it warns
players, sends `stop`, waits for the world to save, and exits. Follow the
console with `journalctl -u bedrock-supervisor -f`.

**`TimeoutStopSec` must exceed the graceful-stop budget.** That budget is the
sum of `--warn-at` (100s with the default `60,30,10`) and `--stop-timeout`
(60s default) — 160s by default. The shipped unit sets `TimeoutStopSec=300`.
If you raise `--warn-at` or `--stop-timeout`, raise `TimeoutStopSec` to match,
or systemd will `SIGKILL` the process mid-shutdown and the world will not
save.

**`Restart=on-failure`** brings the supervisor itself back after it gives up
on a crash-looping server (see `MAX_CONSECUTIVE_FAILURES` in
`supervisor.rs`) or fails outright. A clean stop exits `0` and is left alone.
`StartLimitIntervalSec`/`StartLimitBurst` in the `[Unit]` section stop that
from spinning forever if the server can never start.

## Windows (NSSM)

`bedrock-supervisor` is a normal console executable, not a native Windows
service — there is no SCM control handler in this build (see
[Why not `windows-service`?](#why-not-windows-service-yet) below). The
practical path is [NSSM](https://nssm.cc/), which runs it as a child process
and translates service stop requests into a Ctrl+C event that the
supervisor's existing `ctrlc` handler already understands.

[`packaging/windows/install-service.ps1`](../packaging/windows/install-service.ps1)
wraps the setup:

```powershell
./packaging/windows/install-service.ps1 -ServerPath C:\minecraft -DownloadType windows
nssm start BedrockSupervisor
```

Or by hand:

```powershell
nssm install BedrockSupervisor "C:\Program Files\bedrock-up\bedrock-supervisor.exe"
nssm set BedrockSupervisor AppParameters "--download-type windows --server-path C:\minecraft"
nssm set BedrockSupervisor AppDirectory "C:\Program Files\bedrock-up"

# Same TimeoutStopSec concern as systemd, in milliseconds: must exceed
# --warn-at + --stop-timeout or NSSM escalates to a hard kill early.
nssm set BedrockSupervisor AppStopMethodConsole 170000
```

Stop it with `nssm stop BedrockSupervisor` or `Stop-Service
BedrockSupervisor`. As with systemd, raise the NSSM stop timeouts if you
raise `--warn-at` or `--stop-timeout`.

`sc.exe create` is not an alternative here: it can only register executables
that speak the Windows Service Control API directly, which a plain console
app does not.

### Why not `windows-service` yet?

A native SCM handler (the `windows-service` crate) would let `sc.exe` and
`Services.msc` control `bedrock-supervisor` directly, without NSSM in
between. It's deferred: NSSM already gets a working Ctrl+C-based stop to the
existing shutdown path with no code changes, and the crate would add a
Windows-only entry point and control-handler plumbing that's awkward to
exercise in CI (no Windows service test harness). Revisit if NSSM proves
insufficient in practice.

## Both platforms

- **Updates in place:** the supervisor already checks for and applies
  updates on its own ticker (`--check-interval`, default 6h) and optionally
  on startup (`--update-on-start`). Neither the systemd unit nor the NSSM
  service needs to do anything special for updates — that's the point of
  running it under a supervisor rather than a plain cron + restart script.
- **Logs:** `bedrock-supervisor` writes to stdout/stderr like any console
  app. systemd captures that in the journal automatically; NSSM needs
  `AppStdout`/`AppStderr` pointed at files, as in the install script above.
- **Two servers, two supervisors:** one `bedrock-supervisor` process per
  server directory. For multiple servers, install multiple units/services,
  each with its own `--server-path`.
