//! Battery charge percentage via IOKit's power-sources API — the same
//! source the macOS menu-bar battery icon reads. Returns `None` on Macs
//! without a battery.

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFDictionaryRef;
}

/// Charge of the first power source that reports a capacity, 0–100.
/// Allocates a few CF objects per call; all are released before it returns.
pub fn percent() -> Option<f64> {
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return None;
        }
        let blob = CFType::wrap_under_create_rule(blob);
        let list = IOPSCopyPowerSourcesList(blob.as_CFTypeRef());
        if list.is_null() {
            return None;
        }
        let list: CFArray<CFType> = CFArray::wrap_under_create_rule(list);

        let current_key = CFString::from_static_string("Current Capacity");
        let max_key = CFString::from_static_string("Max Capacity");
        for ps in list.iter() {
            let desc = IOPSGetPowerSourceDescription(blob.as_CFTypeRef(), ps.as_CFTypeRef());
            if desc.is_null() {
                continue;
            }
            // "Get" rule: the description is owned by `blob`.
            let desc: CFDictionary<CFString, CFType> = CFDictionary::wrap_under_get_rule(desc);
            let num = |key: &CFString| {
                desc.find(key)
                    .and_then(|v| v.downcast::<CFNumber>())
                    .and_then(|n| n.to_f64())
            };
            if let (Some(cur), Some(max)) = (num(&current_key), num(&max_key)) {
                if max > 0.0 {
                    return Some((cur / max * 100.0).clamp(0.0, 100.0));
                }
            }
        }
        None
    }
}
