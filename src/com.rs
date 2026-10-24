//! Minimal COM/WinRT apartment helpers.

use anyhow::Result;
use windows::Win32::System::Com::CoIncrementMTAUsage;

/// Enters the multi-threaded apartment (MTA) for the current process.
///
/// `windows` 0.62 no longer ships the `windows::core::initialize_mta()` helper,
/// so this wraps `CoIncrementMTAUsage`, which is exactly what that helper did.
/// The returned cookie is deliberately not decremented: the process stays in
/// MTA for its whole lifetime, and there is no matching uninitialize needed.
pub fn initialize_mta() -> Result<()> {
    unsafe {
        let _cookie = CoIncrementMTAUsage()?;
    }
    Ok(())
}
