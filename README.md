# BEDROCK-UP

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

## Example Usage

### Windows

```shell
bedrock-up -d windows -s C:\minecraft
```

### Linux

```shell
bedrock-up -d linux -s ~/minecraft
```

## Notes

The first time running the update, the update will always be applied since there is no cache built yet.
