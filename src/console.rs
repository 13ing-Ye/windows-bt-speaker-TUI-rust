//! Windows console adjustments around crossterm's raw mode.
//!
//! The problem this module exists for is a Chinese IME, not a broken input
//! mode. In a conhost window with an active IME, every letter is claimed as
//! pinyin and opens the candidate window, so crossterm never receives the key
//! and the UI looks frozen. Raw mode does not change that: the IME is attached
//! to the console window, not to the console input mode.
//!
//! The fix is `ImmAssociateContext(hwnd, NULL)`, the documented way to disable
//! the IME for a single window. It is installed for the lifetime of the TUI and
//! the previous input context is restored on drop, so other console windows
//! keep their IME.
//!
//! **Known limitation (Windows 11):** IMEs there are implemented on TSF3, and
//! `ImmAssociateContext` only controls the legacy IMM32 input context. Measured
//! on this machine, it does not stop a TSF3 IME (Microsoft Pinyin) from eating
//! letter keys. The guard is kept because it still applies to legacy IMM32
//! IMEs and is harmless when it does not; the reliable workaround is mouse
//! input, which never passes through the IME. The TUI therefore exposes
//! clickable buttons and rows (see `ui::HitArea`).
//!
//! `CtrlHandlerGuard` is kept as a fallback: if Ctrl+C ever reaches the process
//! as `CTRL_C_EVENT` (for example because something re-enables
//! `ENABLE_PROCESSED_INPUT`), the handler only sets a flag. The UI loop then
//! exits through the normal path and restores the IME and the terminal instead
//! of being terminated mid-raw-mode. Normally raw mode leaves
//! `ENABLE_PROCESSED_INPUT` off, so Ctrl+C arrives as a key event and
//! `event::map_key` handles it.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::BOOL;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{GetConsoleWindow, SetConsoleCtrlHandler, CTRL_C_EVENT};
use windows::Win32::UI::Input::Ime::{ImmAssociateContext, HIMC};

/// Set by the console ctrl handler, polled by the UI loop.
static QUIT_FLAG: AtomicBool = AtomicBool::new(false);

/// True when Ctrl+C arrived as a console control event.
pub fn quit_requested() -> bool {
    QUIT_FLAG.load(Ordering::SeqCst)
}

/// Clears the flag so a fresh UI loop does not inherit an earlier request.
pub fn reset_quit_flag() {
    QUIT_FLAG.store(false, Ordering::SeqCst);
}

/// Whether the UI loop should leave. Pure so the rule can be tested without a
/// real console or a worker thread.
pub fn should_exit(quit_requested: bool, app_should_quit: bool) -> bool {
    quit_requested || app_should_quit
}

/// Console handlers have a five second budget, so this only stores a flag and
/// suppresses the default termination with `TRUE`.
unsafe extern "system" fn on_ctrl(ctrl_type: u32) -> BOOL {
    if ctrl_type == CTRL_C_EVENT {
        QUIT_FLAG.store(true, Ordering::SeqCst);
        return BOOL(1);
    }
    BOOL(0)
}

/// Disables the legacy IMM32 IME context for the console window until dropped.
///
/// Created before the UI loop starts; `Drop` puts the previous input context
/// back so the user's other console windows are unaffected.
///
/// Does not affect TSF3 IMEs (Windows 11 default); see the module docs. Mouse
/// interaction is the workaround that does not depend on the IME architecture.
pub struct ImeGuard {
    hwnd: HWND,
    previous: HIMC,
}

impl ImeGuard {
    /// Installs the guard, or returns `None` when the process has no console
    /// window (detached console, piped session) or the context cannot be
    /// changed. Callers log a warning and keep running with the IME enabled.
    pub fn install() -> Option<Self> {
        unsafe {
            let hwnd = GetConsoleWindow();
            if hwnd.0.is_null() {
                return None;
            }
            // A null input context is how Win32 expresses "no IME here". The
            // return value is the context attached before (also null when the
            // window had none), so a null result is not a failure.
            let previous = ImmAssociateContext(hwnd, HIMC(ptr::null_mut()));
            Some(Self { hwnd, previous })
        }
    }
}

impl Drop for ImeGuard {
    fn drop(&mut self) {
        unsafe {
            ImmAssociateContext(self.hwnd, self.previous);
        }
    }
}

/// RAII registration of the Ctrl+C fallback handler.
///
/// Unregisters on drop so a later run in the same process does not stack
/// handlers. With the default raw-mode flags Ctrl+C is a normal key event and
/// `event::map_key` maps it to `Action::Quit`; this handler only matters if the
/// processed-input flag is somehow enabled again.
pub struct CtrlHandlerGuard;

impl CtrlHandlerGuard {
    pub fn install() -> windows::core::Result<Self> {
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), true)?;
        }
        Ok(Self)
    }
}

impl Drop for CtrlHandlerGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(on_ctrl), false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_exit_covers_every_combination() {
        assert!(!should_exit(false, false));
        assert!(should_exit(true, false));
        assert!(should_exit(false, true));
        assert!(should_exit(true, true));
    }

    #[test]
    fn quit_flag_drives_the_loop_exit_decision() {
        reset_quit_flag();
        assert!(!quit_requested());
        assert!(!should_exit(quit_requested(), false));

        // Mirrors exactly what `on_ctrl` does.
        QUIT_FLAG.store(true, Ordering::SeqCst);
        assert!(quit_requested());
        assert!(should_exit(quit_requested(), false));

        reset_quit_flag();
        assert!(!should_exit(quit_requested(), false));
    }
}
