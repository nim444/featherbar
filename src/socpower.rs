//! Apple Silicon SoC package power via the private IOReport framework.
//!
//! Subscribes once to the `"Energy Model"` channel group, then on each call
//! to [`SocPower::watts`] takes a new sample, deltas it against the previous
//! one, sums the CPU + GPU + ANE energy counters, and divides by the
//! elapsed wall time to give average watts over the interval. This is what
//! `powermetrics --samplers cpu_power` reports as "Combined Power" and what
//! `macmon`/`asitop` show as package power.
//!
//! IOReport is a private framework. No entitlement is required to use it
//! from an unsandboxed process, and the C ABI here has been stable across
//! every Apple Silicon generation (M1 → M4) that ships these counters.
//! App Sandbox would block it, but featherbar's `scripts/bundle.sh` only
//! ad-hoc signs for direct install, so that path doesn't apply.

use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;
use std::ptr::null;
use std::time::Instant;

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{CFRelease, CFTypeRef, kCFAllocatorDefault, kCFAllocatorNull};
use core_foundation::dictionary::{
    CFDictionaryCreateMutableCopy, CFDictionaryGetCount, CFDictionaryGetValue, CFDictionaryRef,
    CFMutableDictionaryRef,
};
use core_foundation::string::{
    CFStringCreateWithBytesNoCopy, CFStringGetCString, CFStringRef, kCFStringEncodingUTF8,
};

#[repr(C)]
struct IOReportSubscription {
    _data: [u8; 0],
}
type IOReportSubscriptionRef = *const IOReportSubscription;

#[link(name = "IOReport", kind = "dylib")]
unsafe extern "C" {
    fn IOReportCopyChannelsInGroup(
        group: CFStringRef,
        subgroup: CFStringRef,
        a: u64,
        b: u64,
        c: u64,
    ) -> CFDictionaryRef;
    fn IOReportCreateSubscription(
        allocator: *const c_void,
        desired: CFMutableDictionaryRef,
        subscribed_out: *mut CFMutableDictionaryRef,
        chan_id: u64,
        ext: CFTypeRef,
    ) -> IOReportSubscriptionRef;
    fn IOReportCreateSamples(
        sub: IOReportSubscriptionRef,
        chans: CFMutableDictionaryRef,
        ext: CFTypeRef,
    ) -> CFDictionaryRef;
    fn IOReportCreateSamplesDelta(
        a: CFDictionaryRef,
        b: CFDictionaryRef,
        ext: CFTypeRef,
    ) -> CFDictionaryRef;
    fn IOReportSimpleGetIntegerValue(item: CFDictionaryRef, a: i32) -> i64;
    fn IOReportChannelGetGroup(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetChannelName(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetUnitLabel(item: CFDictionaryRef) -> CFStringRef;
}

/// `CFString::new` (the high-level wrapper) silently produces broken refs
/// for inputs longer than 9 bytes — a quirk macmon's source documents.
/// Bypass it with the low-level no-copy constructor over the Rust string's
/// own bytes. Result must be released with `CFRelease`.
fn cfstr(val: &str) -> CFStringRef {
    unsafe {
        CFStringCreateWithBytesNoCopy(
            kCFAllocatorDefault,
            val.as_ptr(),
            val.len() as isize,
            kCFStringEncodingUTF8,
            0,
            kCFAllocatorNull,
        )
    }
}

fn from_cfstr(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    // Channel/group/unit labels are short ASCII — 128 bytes is plenty.
    let mut buf = [0i8; 128];
    unsafe {
        if CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8) == 0 {
            return String::new();
        }
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Borrowed (Get-rule) lookup; do not release the returned ref.
fn cfdict_get(dict: CFDictionaryRef, key: &str) -> Option<CFTypeRef> {
    unsafe {
        let k = cfstr(key);
        let v = CFDictionaryGetValue(dict, k as *const c_void);
        CFRelease(k as CFTypeRef);
        (!v.is_null()).then_some(v as CFTypeRef)
    }
}

pub struct SocPower {
    sub: IOReportSubscriptionRef,
    /// Mutable copy of the desired channels dict. IOReport's docs hand back
    /// a "subscribed channels" out-param too, but reusing this dict for
    /// sampling matches what `macmon` does and works in practice.
    chan: CFMutableDictionaryRef,
    /// Last sample + when it was taken. `None` until the first call.
    prev: Option<(CFDictionaryRef, Instant)>,
}

impl SocPower {
    pub fn new() -> Option<Self> {
        unsafe {
            let group = cfstr("Energy Model");
            let desired = IOReportCopyChannelsInGroup(group, null(), 0, 0, 0);
            CFRelease(group as CFTypeRef);
            if desired.is_null() {
                return None;
            }
            let size = CFDictionaryGetCount(desired);
            let chan = CFDictionaryCreateMutableCopy(kCFAllocatorDefault, size, desired);
            CFRelease(desired as CFTypeRef);
            if chan.is_null() {
                return None;
            }
            let mut subscribed: MaybeUninit<CFMutableDictionaryRef> = MaybeUninit::uninit();
            let sub = IOReportCreateSubscription(null(), chan, subscribed.as_mut_ptr(), 0, null());
            // Defensive: the out-param is documented but undefined whether
            // it gets populated. Release if non-null to avoid leaking it.
            let subscribed = subscribed.assume_init();
            if !subscribed.is_null() {
                CFRelease(subscribed as CFTypeRef);
            }
            if sub.is_null() {
                CFRelease(chan as CFTypeRef);
                return None;
            }
            Some(Self { sub, chan, prev: None })
        }
    }

    /// Average SoC package power (watts) over the interval since the last
    /// call. Returns `None` on the first call — a delta needs two samples.
    pub fn watts(&mut self) -> Option<f64> {
        unsafe {
            let curr = IOReportCreateSamples(self.sub, self.chan, null());
            let now = Instant::now();
            if curr.is_null() {
                return None;
            }
            let result = self.prev.take().and_then(|(prev, prev_at)| {
                let dt = now.duration_since(prev_at).as_secs_f64();
                let w = (dt > 0.0).then(|| sum_watts(prev, curr, dt)).flatten();
                CFRelease(prev as CFTypeRef);
                w
            });
            self.prev = Some((curr, now));
            result
        }
    }
}

impl Drop for SocPower {
    fn drop(&mut self) {
        unsafe {
            if let Some((s, _)) = self.prev.take() {
                CFRelease(s as CFTypeRef);
            }
            if !self.chan.is_null() {
                CFRelease(self.chan as CFTypeRef);
            }
            // IOReportSubscriptionRef has no documented release function;
            // macmon leaves it too. It dies with the process.
        }
    }
}

/// Sum CPU + GPU + ANE energy channels from a delta dict, converting each
/// channel's cumulative energy (in its unit) to average watts over `dt`.
/// CPU+GPU+ANE matches what `powermetrics` reports as "Combined Power" and
/// what `asitop` displays — DRAM/SRAM are tracked separately and excluded.
fn sum_watts(prev: CFDictionaryRef, curr: CFDictionaryRef, dt: f64) -> Option<f64> {
    unsafe {
        let delta = IOReportCreateSamplesDelta(prev, curr, null());
        if delta.is_null() {
            return None;
        }
        let items = match cfdict_get(delta, "IOReportChannels") {
            Some(v) => v as CFArrayRef,
            None => {
                CFRelease(delta as CFTypeRef);
                return None;
            }
        };
        let mut total = 0.0;
        let n = CFArrayGetCount(items);
        for i in 0..n {
            let item = CFArrayGetValueAtIndex(items, i) as CFDictionaryRef;
            if from_cfstr(IOReportChannelGetGroup(item)) != "Energy Model" {
                continue;
            }
            let name = from_cfstr(IOReportChannelGetChannelName(item));
            // CPU: M-series exposes per-cluster counters — "ECPU Energy",
            // "PCPU Energy", and on multi-die parts "ECPU0/PCPU1/…". All
            // end in "CPU Energy". GPU is a single "GPU Energy". ANE may
            // be "ANE", "ANE0", "ANE1" on Ultra.
            let matched = name.ends_with("CPU Energy")
                || name == "GPU Energy"
                || name.starts_with("ANE");
            if !matched {
                continue;
            }
            let energy = IOReportSimpleGetIntegerValue(item, 0) as f64;
            let unit = from_cfstr(IOReportChannelGetUnitLabel(item));
            let joules = match unit.trim() {
                "mJ" => energy / 1e3,
                "uJ" => energy / 1e6,
                "nJ" => energy / 1e9,
                _ => continue,
            };
            total += joules / dt;
        }
        CFRelease(delta as CFTypeRef);
        Some(total)
    }
}
