//! Bluetooth worker module: everything WinRT lives behind this boundary.

pub mod id;
pub mod meter;
pub mod types;
pub mod worker;

pub use id::mac_address;
pub use types::{
    silence_due, Cmd, DeviceInfo, Evt, AUDIO_ACTIVE_THRESHOLD, SILENCE_HINT_DELAY,
};
pub use worker::{ForcedOpenError, Worker, WorkerOptions};
