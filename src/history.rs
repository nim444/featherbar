//! Metric history that survives restarts, in one fixed-size file.
//!
//! The file is memory-mapped, so recording a sample is a plain memory write
//! and the kernel writes dirty pages back on its own (they survive even a
//! crash or `kill -9`; only a kernel panic or power loss can drop the last
//! few seconds). The file never grows: it holds two fixed rings and new data
//! overwrites the oldest.
//!
//! - `raw`: every sample (one per tick, ~2s) with its timestamp, ~1 hour.
//! - `min`: one slot per wall-clock minute (avg + peak), 24 hours. A slot
//!   remembers which minute it belongs to, so a stale slot reads as a gap.
//!
//! Time the app wasn't running (or the Mac was asleep) simply has no
//! samples, and [`History::series`] turns that into `None` gap markers.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

/// Number of metrics stored per sample (see `Metric::slot` in main.rs).
pub const METRICS: usize = 5;
/// Raw ring length. 1900 × ~2s is a bit over an hour, so the Hour view
/// always has full coverage even with timer drift.
const RAW_LEN: usize = 1900;
/// Minute ring length: 24 hours.
const MIN_LEN: usize = 24 * 60;
/// Expected spacing of raw samples, seconds. A jump of more than
/// `RAW_GAP` between neighbours is drawn as a gap.
pub const RAW_STEP: u64 = 2;
const RAW_GAP: u64 = RAW_STEP * 3;

const MAGIC: [u8; 8] = *b"FBHIST\0\0";
/// Bump when the layout changes; an old file is then started fresh.
const VERSION: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: [u8; 8],
    version: u32,
    raw_len: u32,
    min_len: u32,
    /// Next raw slot to write.
    raw_head: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawSlot {
    /// Unix seconds; 0 = never written.
    t: u32,
    v: [f32; METRICS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MinSlot {
    /// Unix minute (secs / 60) this slot holds; 0 = never written.
    minute: u32,
    /// Per-metric sample counts, so a metric that was missing (NaN) for
    /// part of the minute still averages correctly.
    count: [u16; METRICS],
    sum: [f32; METRICS],
    min: [f32; METRICS],
    max: [f32; METRICS],
}

/// The whole file. Every field is valid when zeroed.
#[repr(C)]
struct Layout {
    header: Header,
    raw: [RawSlot; RAW_LEN],
    min: [MinSlot; MIN_LEN],
}

const SIZE: usize = std::mem::size_of::<Layout>();

/// One point on a graph: time (unix secs), average, and peak. For raw
/// samples `min == avg == max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub t: u64,
    pub avg: f32,
    pub min: f32,
    pub max: f32,
}

pub struct History {
    map: NonNull<Layout>,
}

impl History {
    /// Default location: `~/Library/Application Support/Featherbar/history.bin`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/Featherbar")
                .join("history.bin"),
        )
    }

    /// Map the history file, creating or resetting it as needed. Never
    /// fails: if the file can't be used, history is kept in memory for
    /// this session only.
    pub fn open(path: Option<&Path>) -> Self {
        if let Some(path) = path {
            match map_file(path) {
                Ok(map) => return Self::init(map),
                Err(e) => eprintln!("history: {}: {e}; keeping it in memory", path.display()),
            }
        }
        Self::init(map_anon().expect("anonymous mmap failed"))
    }

    fn init(map: NonNull<Layout>) -> Self {
        let mut h = Self { map };
        let hd = h.layout().header;
        let valid = hd.magic == MAGIC
            && hd.version == VERSION
            && hd.raw_len as usize == RAW_LEN
            && hd.min_len as usize == MIN_LEN
            && (hd.raw_head as usize) < RAW_LEN;
        if !valid {
            // Unknown or corrupt: start fresh. All-zero is a valid empty state.
            unsafe { std::ptr::write_bytes(h.map.as_ptr() as *mut u8, 0, SIZE) };
            h.layout_mut().header = Header {
                magic: MAGIC,
                version: VERSION,
                raw_len: RAW_LEN as u32,
                min_len: MIN_LEN as u32,
                raw_head: 0,
            };
        }
        h
    }

    fn layout(&self) -> &Layout {
        unsafe { self.map.as_ref() }
    }

    fn layout_mut(&mut self) -> &mut Layout {
        unsafe { self.map.as_mut() }
    }

    /// Store one sample taken at unix time `t`. NaN means "no reading".
    pub fn record(&mut self, t: u64, v: [f32; METRICS]) {
        let l = self.layout_mut();
        let head = l.header.raw_head as usize;
        l.raw[head] = RawSlot { t: t as u32, v };
        l.header.raw_head = ((head + 1) % RAW_LEN) as u32;

        let minute = (t / 60) as u32;
        let slot = &mut l.min[minute as usize % MIN_LEN];
        if slot.minute != minute {
            *slot = MinSlot {
                minute,
                count: [0; METRICS],
                sum: [0.0; METRICS],
                min: [f32::NAN; METRICS],
                max: [f32::NAN; METRICS],
            };
        }
        for (i, &x) in v.iter().enumerate() {
            if !x.is_nan() {
                slot.count[i] = slot.count[i].saturating_add(1);
                slot.sum[i] += x;
                slot.min[i] = slot.min[i].min(x);
                slot.max[i] = slot.max[i].max(x);
            }
        }
    }

    /// The most recent raw reading of `metric`, if it was taken within a
    /// few seconds of `now`.
    pub fn latest(&self, metric: usize, now: u64) -> Option<f32> {
        let l = self.layout();
        let prev = (l.header.raw_head as usize + RAW_LEN - 1) % RAW_LEN;
        let s = l.raw[prev];
        let fresh = s.t != 0 && now.saturating_sub(s.t as u64) <= RAW_GAP;
        (fresh && !s.v[metric].is_nan()).then_some(s.v[metric])
    }

    /// Points for `metric` over `(now - span, now]`, oldest first, with
    /// `None` wherever there's a gap (app not running, Mac asleep, or the
    /// metric had no reading). Spans up to an hour use raw samples; longer
    /// spans use per-minute averages and peaks.
    pub fn series(&self, metric: usize, now: u64, span: u64) -> Vec<Option<Point>> {
        if span <= 3600 {
            self.raw_series(metric, now, span)
        } else {
            self.minute_series(metric, now, span)
        }
    }

    fn raw_series(&self, metric: usize, now: u64, span: u64) -> Vec<Option<Point>> {
        let l = self.layout();
        let head = l.header.raw_head as usize;
        let start = now.saturating_sub(span);
        let mut out = Vec::with_capacity((span / RAW_STEP) as usize + 8);
        let mut prev_t: Option<u64> = None;
        // Oldest → newest is head, head+1, … head-1.
        for i in 0..RAW_LEN {
            let s = l.raw[(head + i) % RAW_LEN];
            let t = s.t as u64;
            if s.t == 0 || t <= start || t > now {
                continue;
            }
            if let Some(p) = prev_t {
                if t.saturating_sub(p) > RAW_GAP && !matches!(out.last(), Some(None)) {
                    out.push(None);
                }
            }
            prev_t = Some(t);
            let x = s.v[metric];
            if x.is_nan() {
                if !matches!(out.last(), Some(None)) {
                    out.push(None);
                }
            } else {
                out.push(Some(Point { t, avg: x, min: x, max: x }));
            }
        }
        out
    }

    fn minute_series(&self, metric: usize, now: u64, span: u64) -> Vec<Option<Point>> {
        let l = self.layout();
        let last = now / 60;
        let first = now.saturating_sub(span) / 60 + 1;
        let mut out = Vec::with_capacity((last + 1 - first) as usize);
        for m in first..=last {
            let s = l.min[m as usize % MIN_LEN];
            let n = s.count[metric];
            if s.minute as u64 == m && n > 0 {
                out.push(Some(Point {
                    t: m * 60 + 30,
                    avg: s.sum[metric] / n as f32,
                    min: s.min[metric],
                    max: s.max[metric],
                }));
            } else if !matches!(out.last(), Some(None)) {
                out.push(None);
            }
        }
        out
    }
}

/// Merge points into time buckets `bucket` seconds wide (counted from
/// `start`), so a long span draws as a smooth line instead of a dense
/// scribble. Each bucket keeps the mean time and average, and the lowest
/// low and highest high, so spikes and dips survive. Gaps are preserved:
/// points on either side of a `None` are never merged.
pub fn downsample(series: &[Option<Point>], start: u64, bucket: u64) -> Vec<Option<Point>> {
    struct Acc {
        key: u64,
        t_sum: u64,
        avg_sum: f64,
        min: f32,
        max: f32,
        n: u64,
    }
    impl Acc {
        fn point(&self) -> Option<Point> {
            Some(Point {
                t: self.t_sum / self.n,
                avg: (self.avg_sum / self.n as f64) as f32,
                min: self.min,
                max: self.max,
            })
        }
    }

    let bucket = bucket.max(1);
    let mut out = Vec::new();
    let mut runs = series.split(Option::is_none).peekable();
    while let Some(run) = runs.next() {
        let mut acc: Option<Acc> = None;
        for p in run.iter().flatten() {
            let key = p.t.saturating_sub(start) / bucket;
            match &mut acc {
                Some(a) if a.key == key => {
                    a.t_sum += p.t;
                    a.avg_sum += p.avg as f64;
                    a.min = a.min.min(p.min);
                    a.max = a.max.max(p.max);
                    a.n += 1;
                }
                _ => {
                    if let Some(a) = &acc {
                        out.push(a.point());
                    }
                    acc = Some(Acc {
                        key,
                        t_sum: p.t,
                        avg_sum: p.avg as f64,
                        min: p.min,
                        max: p.max,
                        n: 1,
                    });
                }
            }
        }
        if let Some(a) = &acc {
            out.push(a.point());
        }
        if runs.peek().is_some() && !matches!(out.last(), Some(None)) {
            out.push(None);
        }
    }
    out
}

/// Debug builds only: fill the last day with plausible fake samples and a
/// few gaps (app off for 2 h, asleep for 8 min, a 10 s hiccup), so the
/// graphs can be checked without waiting a day.
#[cfg(debug_assertions)]
impl History {
    pub fn seed_demo(&mut self, now: u64) {
        let off = [(now - 10 * 3600, now - 8 * 3600), (now - 20 * 60, now - 12 * 60), (now - 40, now - 30)];
        let mut t = now - 86_400;
        while t <= now {
            if !off.iter().any(|&(a, b)| t >= a && t < b) {
                let x = t as f32;
                // Cheap deterministic jitter in -1..1, like real sample noise.
                let noise = (t.wrapping_mul(2_654_435_761) % 1000) as f32 / 500.0 - 1.0;
                let spike = if (t / 2) % 211 < 4 { 40.0 } else { 0.0 };
                let cpu = (18.0 + 8.0 * (x / 1800.0).sin() + 6.0 * noise + spike).clamp(0.0, 100.0);
                let power = 4.0 + cpu * 0.22;
                let ram = 72.0 + 9.0 * (x / 5400.0).sin();
                let temp = 46.0 + cpu * 0.42;
                // Drains over ~6 h, then charges back up: a sawtooth.
                let phase = (t % 28_800) as f32 / 28_800.0;
                let battery = if phase < 0.75 { 100.0 - phase / 0.75 * 95.0 } else { 5.0 + (phase - 0.75) / 0.25 * 95.0 };
                self.record(t, [cpu, power, ram, temp, battery]);
            }
            t += RAW_STEP;
        }
    }
}

impl Drop for History {
    fn drop(&mut self) {
        unsafe {
            let p = self.map.as_ptr() as *mut libc::c_void;
            libc::msync(p, SIZE, libc::MS_SYNC);
            libc::munmap(p, SIZE);
        }
    }
}

fn map_file(path: &Path) -> std::io::Result<NonNull<Layout>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path contains NUL"))?;
    unsafe {
        let fd = libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC, 0o644);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if st.st_size as usize != SIZE {
                // Wrong size = foreign or old file. Truncate to zero first so
                // the extension below is all zeros (a valid empty layout).
                if libc::ftruncate(fd, 0) != 0 || libc::ftruncate(fd, SIZE as libc::off_t) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(NonNull::new_unchecked(p as *mut Layout))
        })();
        // The mapping keeps the file alive; the descriptor isn't needed.
        libc::close(fd);
        result
    }
}

fn map_anon() -> std::io::Result<NonNull<Layout>> {
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(NonNull::new_unchecked(p as *mut Layout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("featherbar-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("history.bin")
    }

    const T0: u64 = 1_800_000_000; // a minute boundary: 1_800_000_000 % 60 == 0

    fn vals(x: f32) -> [f32; METRICS] {
        [x; METRICS]
    }

    #[test]
    fn survives_reopen() {
        let p = tmp("reopen");
        {
            let mut h = History::open(Some(&p));
            for i in 0..10 {
                h.record(T0 + i * 2, vals(i as f32));
            }
        }
        assert_eq!(std::fs::metadata(&p).unwrap().len() as usize, SIZE);
        let h = History::open(Some(&p));
        let s = h.series(0, T0 + 18, 60);
        assert_eq!(s.len(), 10);
        assert_eq!(s[9], Some(Point { t: T0 + 18, avg: 9.0, min: 9.0, max: 9.0 }));
    }

    #[test]
    fn corrupt_file_starts_fresh() {
        let p = tmp("corrupt");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![0xAB; SIZE]).unwrap();
        let h = History::open(Some(&p));
        assert!(h.series(0, T0, 3600).is_empty());
        std::fs::write(&p, b"short").unwrap();
        drop(h);
        let h = History::open(Some(&p));
        assert!(h.series(0, T0, 3600).is_empty());
        assert_eq!(std::fs::metadata(&p).unwrap().len() as usize, SIZE);
    }

    #[test]
    fn raw_gap_is_marked() {
        let mut h = History::open(None);
        h.record(T0, vals(1.0));
        h.record(T0 + 2, vals(2.0));
        h.record(T0 + 100, vals(3.0)); // app was off / Mac asleep
        h.record(T0 + 102, vals(4.0));
        let s = h.series(0, T0 + 102, 300);
        assert_eq!(s.len(), 5);
        assert!(s[2].is_none());
        assert_eq!(s[3].unwrap().avg, 3.0);
    }

    #[test]
    fn nan_is_a_gap_for_that_metric_only() {
        let mut h = History::open(None);
        h.record(T0, [1.0, f32::NAN, 1.0, 1.0, 1.0]);
        h.record(T0 + 2, [2.0, 5.0, 2.0, 2.0, 2.0]);
        assert_eq!(h.series(0, T0 + 2, 60).len(), 2);
        let power = h.series(1, T0 + 2, 60);
        assert_eq!(power, vec![None, Some(Point { t: T0 + 2, avg: 5.0, min: 5.0, max: 5.0 })]);
    }

    #[test]
    fn window_excludes_old_samples() {
        let mut h = History::open(None);
        for i in 0..100 {
            h.record(T0 + i * 2, vals(i as f32));
        }
        let s = h.series(0, T0 + 198, 60);
        assert_eq!(s.len(), 30);
        assert!(s.iter().all(|p| p.unwrap().t > T0 + 138));
    }

    #[test]
    fn ring_wraps() {
        let mut h = History::open(None);
        let n = RAW_LEN as u64 + 50;
        for i in 0..n {
            h.record(T0 + i * 2, vals(i as f32));
        }
        let now = T0 + (n - 1) * 2;
        let s = h.series(0, now, 3600);
        assert_eq!(s.len(), 1800);
        assert!(s.iter().all(Option::is_some));
        assert_eq!(s.last().unwrap().unwrap().avg, (n - 1) as f32);
    }

    #[test]
    fn minutes_keep_average_and_peak() {
        let mut h = History::open(None);
        // Minute 0: 1, 2, 9 → low 1, avg 4, peak 9. Minute 1 missing. Minute 2: 5.
        h.record(T0, vals(1.0));
        h.record(T0 + 2, vals(2.0));
        h.record(T0 + 4, vals(9.0));
        h.record(T0 + 120, vals(5.0));
        let s = h.series(0, T0 + 120, 86_400);
        let n = s.len();
        assert_eq!(s[n - 3], Some(Point { t: T0 + 30, avg: 4.0, min: 1.0, max: 9.0 }));
        assert!(s[n - 2].is_none());
        assert_eq!(s[n - 1].unwrap().avg, 5.0);
        // Everything before the first recorded minute collapses to one gap.
        assert_eq!(n, 4);
    }

    #[test]
    fn stale_minute_slot_reads_as_gap() {
        let mut h = History::open(None);
        h.record(T0, vals(1.0));
        // Exactly one day later lands in the same slot index, but an hour
        // view of "now" mustn't show yesterday's minute.
        let s = h.series(0, T0 + 86_400 * 2, 86_400);
        assert!(s.iter().all(Option::is_none));
    }

    #[test]
    fn latest_is_only_fresh() {
        let mut h = History::open(None);
        assert_eq!(h.latest(0, T0), None);
        h.record(T0, vals(7.0));
        assert_eq!(h.latest(0, T0 + 2), Some(7.0));
        assert_eq!(h.latest(0, T0 + 60), None);
    }

    fn pt(t: u64, v: f32) -> Option<Point> {
        Some(Point { t, avg: v, min: v, max: v })
    }

    #[test]
    fn downsample_keeps_lows_and_highs() {
        // Two buckets of 10s: [1, 9, 2] and [4, 6].
        let s = vec![pt(0, 1.0), pt(2, 9.0), pt(4, 2.0), pt(10, 4.0), pt(12, 6.0)];
        let d = downsample(&s, 0, 10);
        assert_eq!(
            d,
            vec![
                Some(Point { t: 2, avg: 4.0, min: 1.0, max: 9.0 }),
                Some(Point { t: 11, avg: 5.0, min: 4.0, max: 6.0 }),
            ]
        );
    }

    #[test]
    fn downsample_never_merges_across_a_gap() {
        // Same bucket, but a gap between: must stay two points with a gap.
        let s = vec![pt(0, 1.0), None, pt(4, 3.0)];
        let d = downsample(&s, 0, 10);
        assert_eq!(d, vec![pt(0, 1.0), None, pt(4, 3.0)]);
    }

    #[test]
    fn downsample_leading_gap_is_kept() {
        let s = vec![None, pt(4, 3.0), pt(6, 5.0)];
        let d = downsample(&s, 0, 10);
        assert_eq!(d, vec![None, Some(Point { t: 5, avg: 4.0, min: 3.0, max: 5.0 })]);
    }
}
