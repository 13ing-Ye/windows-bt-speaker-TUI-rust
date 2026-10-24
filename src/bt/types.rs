//! Message protocol between the UI thread and the Bluetooth worker thread.
//!
//! Every type here is a plain value: no WinRT object is ever sent across the
//! channel boundary (constraint C2).

use std::time::Duration;

/// A peak at or above this value counts as audible audio.
pub const AUDIO_ACTIVE_THRESHOLD: f32 = 0.005;

/// How long a link may stay silent before the UI hints (and the optional
/// automatic reconnect fires).
pub const SILENCE_HINT_DELAY: Duration = Duration::from_secs(8);

/// True when a link has produced only silent samples for the whole hint delay.
///
/// `samples == 0` means the meter never yielded a reading (no default endpoint,
/// or WASAPI unavailable); in that case we must not claim the device is silent.
pub fn silence_due(elapsed: Duration, saw_audio: bool, samples: u32) -> bool {
    !saw_audio && samples > 0 && elapsed >= SILENCE_HINT_DELAY
}

/// One enumerable device, reduced to value types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_paired: bool,
    pub kind: String,
}

/// UI -> worker commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    RefreshDevices,
    Connect { device_id: String, name: String },
    /// Disconnect, cool down, and connect again ("f").
    ///
    /// `device_id` is only needed when nothing is connected: the worker then
    /// connects the device the UI had selected.
    ForceReconnect {
        device_id: Option<String>,
        name: Option<String>,
    },
    Disconnect,
    OpenBluetoothSettings,
    Shutdown,
}

/// Worker -> UI events.
///
/// No `Eq`: [`Evt::AudioLevel`] carries an `f32`.
#[derive(Debug, Clone, PartialEq)]
pub enum Evt {
    /// The worker finished COM/MTA setup and is ready for commands.
    WorkerReady,
    /// Full snapshot, sent when enumeration finishes (and on refresh).
    DeviceList(Vec<DeviceInfo>),
    DeviceAdded(DeviceInfo),
    DeviceRemoved(String),
    /// An `Open()` attempt is in flight for this device.
    Connecting(String),
    /// `State` reached `Opened`, i.e. audio is actually streaming.
    Opened { name: String },
    OpenFailed { name: String, reason: String },
    Closed { name: String },
    /// Peak level (`0.0..=1.0`) of the default render endpoint, sampled every
    /// 200 ms while the link is `Opened`.
    AudioLevel(f32),
    /// The link has been open for a while but the meter never saw any audio.
    SilenceDetected,
    Log(String),
    Fatal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_needs_samples_and_the_full_delay() {
        // No meter reading at all: never claim silence.
        assert!(!silence_due(Duration::from_secs(30), false, 0));
        // Audible audio was seen at some point: a later pause is not silence.
        assert!(!silence_due(Duration::from_secs(30), true, 100));
        // Silent samples, but not for long enough yet.
        assert!(!silence_due(Duration::from_secs(7), false, 40));
        // Silent samples for the whole delay.
        assert!(silence_due(SILENCE_HINT_DELAY, false, 40));
        assert!(silence_due(Duration::from_secs(9), false, 1));
    }
}
