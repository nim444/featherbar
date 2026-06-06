//! Launch-at-login via SMAppService (macOS 13+).
//!
//! SMAppService only works for a real .app bundle — a bare binary can't
//! register itself. `is_bundled` gates the menu item so the toggle is only
//! offered when it can actually work (build the bundle with scripts/bundle.sh).

use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// True when the running executable lives inside an .app bundle.
pub fn is_bundled() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

/// Whether the app is currently registered to launch at login.
pub fn is_enabled() -> bool {
    // Safety: mainAppService/status are plain ObjC calls with no preconditions
    // beyond running in an app context.
    unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled }
}

/// Register or unregister the app as a login item. Returns the new state.
pub fn set_enabled(enable: bool) -> Result<bool, String> {
    let service = unsafe { SMAppService::mainAppService() };
    let result = if enable {
        unsafe { service.registerAndReturnError() }
    } else {
        unsafe { service.unregisterAndReturnError() }
    };
    match result {
        Ok(()) => Ok(enable),
        Err(e) => Err(e.localizedDescription().to_string()),
    }
}
