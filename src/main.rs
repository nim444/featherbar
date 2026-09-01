//! featherbar — a tiny, modular macOS menu-bar system monitor.
//!
//! Design goals: flat memory, no background work, easy to extend.
//! - Everything runs on the main thread (required by tray-icon on macOS).
//! - One owner (`Sampler`) holds all sampling state, so no per-tick allocation
//!   and no borrow-checker pain. RSS stays in the single-digit MB.
//! - Add a metric by adding a `Metric` variant + a `Sampler::fragment` arm.

mod login_item;
mod socpower;
mod two_line;

use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use socpower::SocPower;
use sysinfo::{Components, System};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use two_line::{Level, Seg};

/// How often to refresh the reading.
const REFRESH: Duration = Duration::from_secs(2);

/// Which stats to show. Add a variant + a `fragment` arm to extend.
/// CPU temperature comes from sysinfo's Components (PMU die sensors) — a
/// maintained crate, not hand-rolled SMC keys. Power comes from IOReport's
/// "Energy Model" channels (CPU + GPU + ANE) via [`socpower`] — the same
/// channels `powermetrics` aggregates as "Combined Power".
#[derive(Clone, Copy)]
enum Metric {
    Ram,
    Cpu,
    Power, // SoC package watts (CPU+GPU+ANE) from IOReport
    Temp,  // hottest CPU die sensor (PMU tdie*), via sysinfo Components
}

/// The two stacked menu-bar lines, top and bottom.
const LINE_TOP: &[Metric] = &[Metric::Cpu, Metric::Power];
const LINE_BOTTOM: &[Metric] = &[Metric::Ram, Metric::Temp];

/// Single owner of all sampling state.
struct Sampler {
    sys: System,
    components: Components,
    /// IOReport subscription for "Energy Model" channels. `None` only if the
    /// private framework refused to subscribe (shouldn't happen on a stock
    /// Apple Silicon Mac, but we degrade gracefully to "—W" if it does).
    soc_power: Option<SocPower>,
}

impl Sampler {
    fn new() -> Self {
        // System::new() starts empty — refresh_memory/refresh_cpu_usage fill
        // in just what we display. new_all() would also load the full process
        // list and more, which we'd carry in RSS for nothing.
        let sys = System::new();
        let components = Components::new_with_refreshed_list();
        let soc_power = SocPower::new();
        Self {
            sys,
            components,
            soc_power,
        }
    }

    /// One labeled, color-coded fragment per metric, e.g. `CPU  15%`.
    fn fragment(&mut self, metric: Metric, out: &mut Vec<Seg>) {
        match metric {
            Metric::Ram => {
                self.sys.refresh_memory();
                let used = self.sys.used_memory() as f64; // bytes
                let total = self.sys.total_memory() as f64; // bytes
                let pct = if total > 0.0 {
                    used / total * 100.0
                } else {
                    0.0
                };
                out.push(Seg::new("R", Level::Neutral));
                out.push(Seg::new(format!("{}%", pad(pct, 3, 0)), ram_level(pct)));
            }
            Metric::Cpu => {
                // CPU needs two refreshes spaced apart; we refresh every tick, so
                // the value is the load since the previous tick.
                self.sys.refresh_cpu_usage();
                let pct = self.sys.global_cpu_usage() as f64;
                out.push(Seg::new("C", Level::Neutral));
                out.push(Seg::new(format!("{}%", pad(pct, 3, 0)), pct_level(pct)));
            }
            Metric::Power => {
                // None on the first tick (the delta needs two samples) and
                // if IOReport refused to subscribe at startup.
                let watts = self.soc_power.as_mut().and_then(|p| p.watts());
                match watts {
                    Some(w) => {
                        out.push(Seg::new(format!("{}W", pad(w, 4, 0)), watt_level(w)));
                    }
                    None => out.push(Seg::new("   —W", Level::Neutral)),
                }
            }
            Metric::Temp => {
                // Hottest CPU die sensor. The PMU exposes several tdie probes;
                // max is what "CPU temp" colloquially means.
                self.components.refresh(false);
                let max = self
                    .components
                    .iter()
                    .filter(|c| c.label().contains("tdie"))
                    .filter_map(|c| c.temperature())
                    .fold(f32::NAN, f32::max) as f64;
                if max.is_nan() {
                    out.push(Seg::new("  —°C", Level::Neutral));
                } else {
                    out.push(Seg::new(format!("{}°C", pad(max, 3, 0)), temp_level(max)));
                }
            }
        }
    }

    /// Compose one menu-bar line from a list of metrics.
    fn line(&mut self, metrics: &[Metric]) -> Vec<Seg> {
        let mut out = Vec::with_capacity(metrics.len() * 2 + 1);
        for (i, m) in metrics.iter().enumerate() {
            if i > 0 {
                out.push(Seg::new(" ", Level::Neutral));
            }
            self.fragment(*m, &mut out);
        }
        out
    }
}

/// Right-align a number in `width` chars. The title renders in a fully
/// monospaced font, so space padding keeps `3%` and `15%` the same width and
/// nothing shifts as values change.
fn pad(value: f64, width: usize, decimals: usize) -> String {
    format!("{value:>width$.decimals$}")
}

/// CPU percent: green below 40, orange 40-70, red above.
fn pct_level(pct: f64) -> Level {
    match pct {
        p if p < 40.0 => Level::Good,
        p if p < 70.0 => Level::Warn,
        _ => Level::Crit,
    }
}

/// RAM percent runs hotter than CPU by nature: green below 75, orange 75-85,
/// red above.
fn ram_level(pct: f64) -> Level {
    match pct {
        p if p < 75.0 => Level::Good,
        p if p < 85.0 => Level::Warn,
        _ => Level::Crit,
    }
}

/// SoC package draw (CPU+GPU+ANE): green under 10W (idle/light), orange
/// 10-20W (sustained background work), red above (compile/render territory).
fn watt_level(watts: f64) -> Level {
    match watts {
        w if w < 10.0 => Level::Good,
        w if w < 20.0 => Level::Warn,
        _ => Level::Crit,
    }
}

/// CPU die temp: green under 60, orange 60-80, red above (throttle territory).
fn temp_level(temp: f64) -> Level {
    match temp {
        t if t < 60.0 => Level::Good,
        t if t < 80.0 => Level::Warn,
        _ => Level::Crit,
    }
}

/// Flatten segments to plain text for the single-line fallback path.
fn plain(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

fn main() {
    // Accessory => menu-bar only, no Dock icon, no app window.
    let mut event_loop = EventLoopBuilder::new().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let mut sampler = Sampler::new();
    // Prime CPU so the first reading is a real delta, not 0%/garbage.
    sampler.sys.refresh_cpu_usage();

    // Right-click menu: disabled "Featherbar" header (so the app name is
    // visible — the menu-bar icon is a numeric readout, not a glyph) +
    // launch-at-login toggle + Quit.
    // SMAppService needs a real .app bundle (scripts/bundle.sh); from a bare
    // `cargo run` binary the item is shown disabled so it can't silently fail.
    let title_item = MenuItem::new("Featherbar", false, None);
    let bundled = login_item::is_bundled();
    let login_item = CheckMenuItem::new(
        if bundled {
            "Launch at login"
        } else {
            "Launch at login (needs .app build)"
        },
        bundled,
        bundled && login_item::is_enabled(),
        None,
    );
    let login_id: MenuId = login_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id: MenuId = quit_item.id().clone();
    let menu = Menu::new();
    menu.append(&title_item).expect("failed to build menu");
    menu.append(&PredefinedMenuItem::separator())
        .expect("failed to build menu");
    menu.append(&login_item).expect("failed to build menu");
    menu.append(&quit_item).expect("failed to build menu");

    // Created lazily on StartCause::Init (must exist while loop is running).
    let mut tray: Option<TrayIcon> = None;
    // The NSStatusBarButton behind the tray icon, found once after creation;
    // needed because the two-line display is drawn into its image.
    let mut button: Option<objc2::rc::Retained<objc2_app_kit::NSStatusBarButton>> = None;
    // Font + attribute dictionaries + line geometry, built once.
    let renderer = two_line::Renderer::new();

    event_loop.run(move |event, _target, control_flow| match event {
        Event::NewEvents(StartCause::Init) => {
            tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .build()
                    .expect("failed to create tray icon"),
            );
            let mtm = MainThreadMarker::new().expect("event loop runs on the main thread");
            button = two_line::status_button(mtm);
            // The pool guarantees ObjC temporaries (image, strings) die now,
            // not whenever the loop feels like draining — keeps RSS flat.
            objc2::rc::autoreleasepool(|_| {
                let (top, bottom) = (sampler.line(LINE_TOP), sampler.line(LINE_BOTTOM));
                match &button {
                    Some(b) => renderer.set_title(b, &top, &bottom),
                    // Fallback: single line via the plain title, colors dropped.
                    None => {
                        if let Some(t) = &tray {
                            t.set_title(Some(format!("{} · {}", plain(&top), plain(&bottom))));
                        }
                    }
                }
            });
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == quit_id {
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                if ev.id == login_id {
                    // The native menu already flipped the checkmark; make the
                    // registration match it, and re-sync on failure.
                    let want = login_item.is_checked();
                    match login_item::set_enabled(want) {
                        Ok(now) => login_item.set_checked(now),
                        Err(e) => {
                            eprintln!("launch-at-login: {e}");
                            login_item.set_checked(login_item::is_enabled());
                        }
                    }
                }
            }
            objc2::rc::autoreleasepool(|_| {
                let (top, bottom) = (sampler.line(LINE_TOP), sampler.line(LINE_BOTTOM));
                match &button {
                    Some(b) => renderer.set_title(b, &top, &bottom),
                    None => {
                        if let Some(t) = &tray {
                            t.set_title(Some(format!("{} · {}", plain(&top), plain(&bottom))));
                        }
                    }
                }
            });
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        _ => {}
    });
}
