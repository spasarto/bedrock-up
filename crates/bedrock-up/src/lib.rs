pub mod config;
pub mod error;
pub mod process;
mod updater;

pub use config::{DownloadType, UpdateConfig};
pub use error::UpdateError;
pub use updater::{AvailableUpdate, CheckOutcome, StagedUpdate, UpdateOutcome, check};
