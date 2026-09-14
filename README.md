# BEDROCK-UP

[![Linux](https://github.com/spasarto/bedrock-up/actions/workflows/linux.yml/badge.svg)](https://github.com/spasarto/bedrock-up/actions/workflows/linux.yml)
[![Windows](https://github.com/spasarto/bedrock-up/actions/workflows/windows.yml/badge.svg)](https://github.com/spasarto/bedrock-up/actions/workflows/windows.yml)

Fast and efficient Minecraft Bedrock Edition server updater. Supports Windows and Linux as well as the preview versions.

This repository ships two binaries:

- **`bedrock-up`** — a one-shot CLI: check for an update, download it, apply
  it, exit. You run it (or a service manager runs it) and it decides whether
  to restart the server. Documented below.
- **`bedrock-supervisor`** — a long-running process that owns the server: it
  spawns `bedrock_server`, relays its console, and handles the whole
  warn-players → stop → update → restart cycle itself, since Bedrock
  Dedicated Server has no RCON and only the process that spawned it can talk
  to its stdin. See [`bedrock-supervisor`](#bedrock-supervisor) below.

## Usage

Run `bedrock-up` to show the usage help text:

```text
Usage: bedrock-up [OPTIONS] --download-type <DOWNLOAD_TYPE> --server-path <SERVER_PATH>

Options:
  -d, --download-type <DOWNLOAD_TYPE>  Which version of minecraft to download [possible values: windows, linux, preview-windows, preview-linux, server-jar]
  -f, --force                          Whether to force the update even if the version is the same
  -s, --server-path <SERVER_PATH>      Minecraft server path. Should be the directory where the server files are located
  -c, --cache-path <CACHE_PATH>        [default: ~/.bedrock-up/links.json]
  -e, --exclude <EXCLUDE>              Excluded files to not update if they already exist [default: server.properties permissions.json allowlist.json]
  -h, --help                           Print help
  -V, --version                        Print version
```

## Installation

If you have `cargo` — the repo contains two packages, so name the one you want:

```shell
cargo install --git https://github.com/spasarto/bedrock-up.git bedrock-up
# or
cargo install --git https://github.com/spasarto/bedrock-up.git bedrock-supervisor
```

If you don't have cargo, check out the releases 👉

## Example Usage

### Windows

```shell
bedrock-up -d windows -s C:\minecraft
```

### Linux

```shell
bedrock-up -d linux -s ~/minecraft
```

## Usage Notes

The first time running the update, the update will always be applied since there is no cache built yet.

### Exit codes

The exit code tells a wrapper script whether a restart is needed:

| Exit code | Meaning |
|---|---|
| `0` | Already on the latest version — no files were changed |
| `1` | An error occurred — no assumptions should be made about the server directory |
| `2` | New files are on disk — restart the server to run them |

For example, restarting only when an update actually landed:

```powershell
bedrock-up -d windows -s C:\minecraft
if ($LASTEXITCODE -eq 2) { Restart-Service minecraft }
```

### `download` / `apply` subcommands

The default invocation above runs check → download → apply in one shot.
`download` and `apply` split that into two standalone steps, so you can fetch
the update ahead of a maintenance window — during the day, while the server
is still busy — and do the fast filesystem swap later once it's stopped:

```shell
# Ahead of time: check for an update and stage it if one exists.
bedrock-up download -d windows --stage-path ~/.bedrock-up/staged

# Later, once the server is stopped: apply whatever was staged.
bedrock-up apply -s C:\minecraft --stage-path ~/.bedrock-up/staged
```

`--stage-path` defaults to `~/.bedrock-up/staged` and must match between the
two calls. Exit codes follow the same convention as the full pipeline: `0`
nothing new (or nothing staged, for `apply`), `1` error, `2` staged / applied.

## `bedrock-supervisor`

`bedrock-supervisor` runs the server for you and keeps it updated, instead of
you running `bedrock-up` from a wrapper script or scheduled task around a
separately-managed server process. It spawns `bedrock_server` itself, relays
its console in both directions, and on a timer (or on request) checks for an
update, downloads it while the server keeps serving players, warns players on
a countdown, stops the server cleanly, applies the update, and restarts it.

```text
Usage: bedrock-supervisor [OPTIONS] --download-type <DOWNLOAD_TYPE> --server-path <SERVER_PATH> [-- <SERVER_ARGS>...]

Arguments:
  [SERVER_ARGS]...  Extra arguments passed through to the server process

Options:
  -t, --download-type <DOWNLOAD_TYPE>
          Which server build this is, used to pick the launch command [possible values: windows, linux, preview-windows, preview-linux, server-jar]
  -s, --server-path <SERVER_PATH>
          Minecraft server path. The directory containing the server files
      --server-exe <SERVER_EXE>
          Override the executable to launch instead of the default for `--download-type`
      --warn-at <WARN_AT>
          Countdown warnings sent to players via `say`, as a comma-separated list of seconds-before-stop, most distant first. Empty stops the server immediately with no countdown [default: 60,30,10]
      --stop-timeout <STOP_TIMEOUT>
          How long to wait for the server to exit after `stop` before killing it [default: 60]
  -c, --cache-path <CACHE_PATH>
          Where to cache the current version info, to detect updates [default: ~/.bedrock-up/links.json]
  -e, --exclude <EXCLUDE>
          Files to leave alone if they already exist when applying an update [default: server.properties permissions.json allowlist.json]
  -f, --force
          Apply an update even if the cached version already matches
      --check-interval <CHECK_INTERVAL>
          How often to check for updates, in seconds. 0 disables automatic update checks, leaving a supervisor that only babysits and restarts [default: 21600]
      --update-on-start
          Check for and apply an update before starting the server, instead of waiting for the first tick. Off by default: a service manager restarting the supervisor (reboot, crash, config fix) should not cascade into an update every time
  -h, --help
          Print help
  -V, --version
          Print version
```

### Example usage

```shell
# Linux
bedrock-supervisor -t linux -s ~/minecraft

# Windows
bedrock-supervisor -t windows -s C:\minecraft

# Extra arguments after `--` are passed straight to the server executable
bedrock-supervisor -t server-jar -s ~/minecraft -- nogui
```

Once running, typing at its stdin (interactively — not under a service
manager) is relayed straight to the server, so `say hello`, `list`, and other
console commands work exactly as if you'd started `bedrock_server` yourself.
Ctrl+C (and `SIGTERM` on Linux) triggers the same graceful stop as an update:
warn players, `stop`, wait, exit.

### On-demand updates: `trigger-update`

The `--check-interval` ticker covers routine updates, but sometimes you want
one right now without waiting for it — or without restarting the supervisor,
which `--update-on-start` would otherwise require. `trigger-update` connects
to a running supervisor's control socket (a named pipe on Windows, a Unix
domain socket on Linux) and asks it to run its normal check → download →
warn → stop → apply → restart cycle immediately:

```shell
bedrock-supervisor trigger-update -s C:\minecraft
```

`--server-path` must match exactly what the running supervisor was started
with — the socket's name is derived from it. This only nudges an
already-running supervisor; it does not start one.

### Running it as a service

`bedrock-supervisor` is meant to run under a process supervisor of its own —
systemd on Linux, [NSSM](https://nssm.cc/) on Windows — so it starts on boot
and comes back if the machine restarts. A systemd unit file, an NSSM install
script, and the tradeoffs behind that setup are in
[`docs/deployment.md`](docs/deployment.md).

## How It Works

The Minecraft Bedrock Dedicated Server page makes a call out to an API to get the latest server versions. Rather than manipulating and scaping the page, this app calls the same API. This assumes a level of risk since it is an internal API. However, it is my hope that Microsoft agrees that API calls is preferable to web scraping. Should the backend API change, please submit an issue!
