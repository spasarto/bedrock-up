use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
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

/// Clap-free configuration for the check/download/apply pipeline. Paths are
/// expected to already be resolved (tilde-expanded) by the caller.
pub struct UpdateConfig {
    pub download_type: DownloadType,
    pub server_path: PathBuf,
    pub cache_path: PathBuf,
    pub exclude: Vec<String>,
    pub force: bool,
}
