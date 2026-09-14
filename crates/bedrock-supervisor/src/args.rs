use std::path::PathBuf;
use std::time::Duration;

use bedrock_up::UpdateConfig;
use clap::Parser;

use crate::server::ServerConfig;
use crate::supervisor::SupervisorConfig;

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

    /// Where to cache the current version info, to detect updates.
    #[arg(short, long, default_value = "~/.bedrock-up/links.json")]
    pub cache_path: String,

    /// Files to leave alone if they already exist when applying an update.
    #[arg(
        short,
        long,
        value_parser,
        value_delimiter = ' ',
        default_values = ["server.properties",
        "permissions.json",
        "allowlist.json"]
    )]
    pub exclude: Vec<String>,

    /// Apply an update even if the cached version already matches.
    #[arg(short, long, default_value_t = false)]
    pub force: bool,

    /// How often to check for updates, in seconds. 0 disables automatic
    /// update checks, leaving a supervisor that only babysits and restarts.
    #[arg(long, default_value_t = 6 * 60 * 60)]
    pub check_interval: u64,

    /// Check for and apply an update before starting the server, instead of
    /// waiting for the first tick. Off by default: a service manager
    /// restarting the supervisor (reboot, crash, config fix) should not
    /// cascade into an update every time.
    #[arg(long, default_value_t = false)]
    pub update_on_start: bool,

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

    pub fn check_interval(&self) -> Duration {
        Duration::from_secs(self.check_interval)
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

impl From<&ServerKind> for bedrock_up::DownloadType {
    fn from(kind: &ServerKind) -> Self {
        match kind {
            ServerKind::Windows => bedrock_up::DownloadType::Windows,
            ServerKind::Linux => bedrock_up::DownloadType::Linux,
            ServerKind::PreviewWindows => bedrock_up::DownloadType::PreviewWindows,
            ServerKind::PreviewLinux => bedrock_up::DownloadType::PreviewLinux,
            ServerKind::ServerJar => bedrock_up::DownloadType::ServerJar,
        }
    }
}

/// Sends an on-demand update-check request to an already-running
/// `bedrock-supervisor` over its control socket, instead of waiting for the
/// next `--check-interval` tick. Does not start a supervisor itself.
#[derive(Parser, Debug)]
#[command(name = "bedrock-supervisor trigger-update", version, about, long_about = None)]
pub struct TriggerUpdateArgs {
    /// The running supervisor's server path — must match exactly what it was
    /// started with, since the control socket's name is derived from it.
    #[arg(short, long)]
    pub server_path: String,
}

impl From<&SupervisorArgs> for SupervisorConfig {
    fn from(args: &SupervisorArgs) -> Self {
        SupervisorConfig {
            server: ServerConfig::from(args),
            update: UpdateConfig {
                download_type: (&args.download_type).into(),
                server_path: PathBuf::from(shellexpand::tilde(&args.server_path).to_string()),
                cache_path: PathBuf::from(shellexpand::tilde(&args.cache_path).to_string()),
                exclude: args.exclude.clone(),
                force: args.force,
            },
            warn_at: args.warn_at_seconds(),
            stop_timeout: args.stop_timeout(),
            check_interval: args.check_interval(),
            update_on_start: args.update_on_start,
        }
    }
}
