use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::server::ServerConfig;

/// Which server build this is. Picks the default launch command; not the
/// same type as `bedrock-up`'s `DownloadType`, since the library builds
/// without clap and this needs to be a `clap::ValueEnum`.
#[derive(clap::ValueEnum, Clone, Debug)]
pub enum ServerKind {
    Windows,
    Linux,
    PreviewWindows,
    PreviewLinux,
    ServerJar,
}

/// Supervises a Bedrock Dedicated Server process: spawns it, relays its
/// console, and stops it politely (warn players, `stop`, wait, kill on
/// timeout) so an update never has to fight files the server still holds
/// open.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct SupervisorArgs {
    /// Which server build this is, used to pick the launch command.
    #[arg(short = 't', long)]
    pub download_type: ServerKind,

    /// Minecraft server path. The directory containing the server files.
    #[arg(short, long)]
    pub server_path: String,

    /// Override the executable to launch instead of the default for
    /// `--download-type`.
    #[arg(long)]
    pub server_exe: Option<String>,

    /// Countdown warnings sent to players via `say`, as a comma-separated
    /// list of seconds-before-stop, most distant first. Empty stops the
    /// server immediately with no countdown.
    #[arg(long, default_value = "60,30,10")]
    pub warn_at: String,

    /// How long to wait for the server to exit after `stop` before killing it.
    #[arg(long, default_value_t = 60)]
    pub stop_timeout: u64,

    /// Extra arguments passed through to the server process.
    #[arg(last = true)]
    pub server_args: Vec<String>,
}

impl SupervisorArgs {
    pub fn warn_at_seconds(&self) -> Vec<u64> {
        self.warn_at
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    pub fn stop_timeout(&self) -> Duration {
        Duration::from_secs(self.stop_timeout)
    }
}

impl From<&SupervisorArgs> for ServerConfig {
    fn from(args: &SupervisorArgs) -> Self {
        ServerConfig {
            server_path: PathBuf::from(shellexpand::tilde(&args.server_path).to_string()),
            kind: args.download_type.clone(),
            server_exe: args.server_exe.clone(),
            extra_args: args.server_args.clone(),
        }
    }
}
