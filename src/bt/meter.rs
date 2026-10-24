//! WASAPI peak meter for the default audio render endpoint.
//!
//! Purpose: make "is any audio actually flowing?" observable. The Bluetooth
//! stack can report `Opened` while the A2DP source never starts its media
//! stream, so the connection state alone cannot tell the user whether sound is
//! on the wire.
//!
//! Constraints:
//! - Only ever used from the worker thread, inside its MTA apartment (C2).
//! - Failures are non-fatal: a missing default endpoint (headphones being
//!   plugged/unplugged) simply yields `None` and the meter retries later.

use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// How long to wait before re-acquiring the endpoint after a failure.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Caches `IAudioMeterInformation` for the current default render endpoint.
pub struct AudioMeter {
    meter: Option<IAudioMeterInformation>,
    next_attempt: Instant,
}

impl AudioMeter {
    pub fn new() -> Self {
        Self {
            meter: None,
            next_attempt: Instant::now(),
        }
    }

    /// Returns the current peak value in `0.0..=1.0`, or `None` when the
    /// endpoint is temporarily unavailable. Never panics.
    pub fn sample(&mut self) -> Option<f32> {
        let now = Instant::now();

        if self.meter.is_none() {
            if now < self.next_attempt {
                return None;
            }
            self.meter = acquire();
            if self.meter.is_none() {
                self.next_attempt = now + RETRY_INTERVAL;
                return None;
            }
        }

        let meter = self.meter.as_ref()?;
        // `GetPeakValue` is cheap (a cached value maintained by the audio
        // engine) and safe to poll at 200 ms.
        match unsafe { meter.GetPeakValue() } {
            Ok(value) if value.is_finite() => Some(value.clamp(0.0, 1.0)),
            _ => {
                // A `Close`/`Release` during endpoint switching invalidates the
                // interface; drop it and re-acquire on the next retry window.
                self.meter = None;
                self.next_attempt = now + RETRY_INTERVAL;
                None
            }
        }
    }
}

impl Default for AudioMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a meter for the current default playback device.
///
/// Called on the worker thread only; COM is already initialized there.
fn acquire() -> Option<IAudioMeterInformation> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        device
            .Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)
            .ok()
    }
}
