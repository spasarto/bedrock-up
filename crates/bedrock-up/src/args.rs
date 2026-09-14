use std::path::PathBuf;

use bedrock_up::{DownloadType, UpdateConfig};
use clap::Parser;

/// Manages Minecraft Bedrock Edition server updates.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct UpdateArgs {
    /// Which version of minecraft to download
    #[arg(short, long)]
    pub(crate) download_type: DownloadType,

    /// Whether to force the update even if the version is the same.
    #[arg(short, long, default_value_t = false)]
    pub(crate) force: bool,

    /// Minecraft server path. Should be the directory where the server files are located.
    #[arg(short, long)]
    pub(crate) server_path: String,

    #[arg(short, long, default_value = "~/.bedrock-up/links.json")]
    pub(crate) cache_path: String,

    /// Excluded files to not update if they already exist.
    #[arg(
        short,
        long,
        value_parser,
        value_delimiter = ' ',
        default_values = ["server.properties",
        "permissions.json",
        "allowlist.json"]
    )]
    pub(crate) exclude: Vec<String>,
}

impl From<UpdateArgs> for UpdateConfig {
    fn from(args: UpdateArgs) -> Self {
        UpdateConfig {
            download_type: args.download_type,
            server_path: std::path::PathBuf::from(shellexpand::tilde(&args.server_path).to_string()),
            cache_path: std::path::PathBuf::from(shellexpand::tilde(&args.cache_path).to_string()),
            exclude: args.exclude,
            force: args.force,
        }
    }
}

/// Downloads an update if one is available, without touching the server.
/// Meant to run ahead of a maintenance window — e.g. during the day, while
/// the server is still busy — so the later `apply` is just the fast
/// filesystem swap.
#[derive(Parser, Debug)]
#[command(name = "bedrock-up download", version, about, long_about = None)]
pub struct DownloadArgs {
    /// Which version of minecraft to download
    #[arg(short, long)]
    pub(crate) download_type: DownloadType,

    /// Whether to download even if the cached version already matches.
    #[arg(short, long, default_value_t = false)]
    pub(crate) force: bool,

    #[arg(short, long, default_value = "~/.bedrock-up/links.json")]
    pub(crate) cache_path: String,

    /// Where to store the staged download for a later `apply`.
    #[arg(long, default_value = "~/.bedrock-up/staged")]
    pub(crate) stage_path: String,
}

impl DownloadArgs {
    pub(crate) fn stage_path(&self) -> PathBuf {
        PathBuf::from(shellexpand::tilde(&self.stage_path).to_string())
    }
}

impl From<&DownloadArgs> for UpdateConfig {
    fn from(args: &DownloadArgs) -> Self {
        UpdateConfig {
            download_type: args.download_type.clone(),
            // `check`/`download` never read server_path or exclude — those
            // only matter to `apply`.
            server_path: PathBuf::new(),
            cache_path: PathBuf::from(shellexpand::tilde(&args.cache_path).to_string()),
            exclude: Vec::new(),
            force: args.force,
        }
    }
}

/// Applies a previously staged update (see `bedrock-up download`).
/// Filesystem only and fast; stop the server first.
#[derive(Parser, Debug)]
#[command(name = "bedrock-up apply", version, about, long_about = None)]
pub struct ApplyArgs {
    /// Minecraft server path. Should be the directory where the server files are located.
    #[arg(short, long)]
    pub(crate) server_path: String,

    #[arg(short, long, default_value = "~/.bedrock-up/links.json")]
    pub(crate) cache_path: String,

    /// Excluded files to not update if they already exist.
    #[arg(
        short,
        long,
        value_parser,
        value_delimiter = ' ',
        default_values = ["server.properties",
        "permissions.json",
        "allowlist.json"]
    )]
    pub(crate) exclude: Vec<String>,

    /// Where `bedrock-up download` staged the update to apply.
    #[arg(long, default_value = "~/.bedrock-up/staged")]
    pub(crate) stage_path: String,
}

impl ApplyArgs {
    pub(crate) fn stage_path(&self) -> PathBuf {
        PathBuf::from(shellexpand::tilde(&self.stage_path).to_string())
    }
}

impl From<&ApplyArgs> for UpdateConfig {
    fn from(args: &ApplyArgs) -> Self {
        UpdateConfig {
            // `apply` never reads download_type — only `check`/`download` do.
            download_type: DownloadType::Windows,
            server_path: PathBuf::from(shellexpand::tilde(&args.server_path).to_string()),
            cache_path: PathBuf::from(shellexpand::tilde(&args.cache_path).to_string()),
            exclude: args.exclude.clone(),
            force: false,
        }
    }
}
