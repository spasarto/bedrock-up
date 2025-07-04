use clap::Parser;
use std::fmt;

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

use clap::ValueEnum;

#[derive(Debug, Clone, ValueEnum)]
pub enum DownloadType {
    Windows,
    Linux,
    PreviewWindows,
    PreviewLinux,
    ServerJar,
}

impl fmt::Display for DownloadType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadType::Windows => write!(f, "serverBedrockWindows"),
            DownloadType::Linux => write!(f, "serverBedrockLinux"),
            DownloadType::PreviewWindows => {
                write!(f, "serverBedrockPreviewWindows")
            }
            DownloadType::PreviewLinux => write!(f, "serverBedrockPreviewLinux"),
            DownloadType::ServerJar => write!(f, "serverJar"),
        }
    }
}
