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
