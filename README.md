# BEDROCK-UP

[![Linux](https://github.com/spasarto/bedrock-up/actions/workflows/linux.yml/badge.svg)](https://github.com/spasarto/bedrock-up/actions/workflows/linux.yml)
[![Windows](https://github.com/spasarto/bedrock-up/actions/workflows/windows.yml/badge.svg)](https://github.com/spasarto/bedrock-up/actions/workflows/windows.yml)

Fast and efficient Minecraft Bedrock Edition server updater. Supports Windows and Linux as well as the preview versions.

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

If you have `cargo`

```shell
cargo install --git https://github.com/spasarto/bedrock-up.git
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

`bedrock-up` does not manage the Minecraft server process itself — it only downloads and writes files. If the server is running, files it needs to overwrite (like the server executable) will be locked, and the update will fail. Stop the server before running `bedrock-up` and start it again afterward, e.g. from a wrapper script driven by your scheduler.

The process exit code tells you whether a restart is actually needed:

| Exit code | Meaning |
|---|---|
| `0` | Already on the latest version — no files were changed |
| `1` | An error occurred — no assumptions should be made about the server directory |
| `2` | Update was applied — the server should be (re)started |

## How It Works

The Minecraft Bedrock Dedicated Server page makes a call out to an API to get the latest server versions. Rather than manipulating and scaping the page, this app calls the same API. This assumes a level of risk since it is an internal API. However, it is my hope that Microsoft agrees that API calls is preferable to web scraping. Should the backend API change, please submit an issue!
