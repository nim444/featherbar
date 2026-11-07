//! featherbar — a tiny, modular macOS menu-bar system monitor.
//!
//! Design goals: flat memory, no background work, easy to extend.
//! - Everything runs on the main thread (required by tray-icon on macOS).
//! - One owner (`Sampler`) holds all sampling state, so no per-tick allocation
//!   and no borrow-checker pain. RSS stays in the single-digit MB.
//! - Add a metric by adding a `Metric` variant + arms in its methods,
//!   `Sampler::read` and `fragment`.
//! - History lives in one fixed-size memory-mapped file ([`history`]); the
//!   optional graphs window ([`graphs`]) only exists while it's open.

mod graphs;
mod history;
mod battery;
mod login_item;
mod socpower;
mod two_line;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use history::History;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::NSStatusBarButton;
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

/// Which stats to show. Add a variant + arms in `Metric`'s methods and
/// `Sampler::read` to extend.
/// CPU temperature comes from sysinfo's Components (PMU die sensors) — a
/// maintained crate, not hand-rolled SMC keys. Power comes from IOReport's
/// "Energy Model" channels (CPU + GPU + ANE) via [`socpower`] — the same
/// channels `powermetrics` aggregates as "Combined Power". Battery comes
/// from IOKit's power sources via [`battery`] (graphs only, not the menu bar).
///
/// The discriminants are each metric's slot in a reading and in the history
/// file on disk, so don't reorder them.
#[derive(Clone, Copy)]
pub enum Metric {
    Cpu = 0,
    Power = 1, // SoC package watts (CPU+GPU+ANE) from IOReport
    Ram = 2,
    Temp = 3, // hottest CPU die sensor (PMU tdie*), via sysinfo Components
    Battery = 4, // charge %, IOKit power sources; NaN on Macs without one
}

impl Metric {
    /// Every metric, in slot order (also the graph window's top-to-bottom order).
    pub const ALL: [Metric; history::METRICS] =
        [Metric::Cpu, Metric::Power, Metric::Ram, Metric::Temp, Metric::Battery];

    pub fn slot(self) -> usize {
        self as usize
    }

    /// `(warn, crit)`: green until `warn`, orange until `crit`, then red.
    /// Usually high is bad; see [`Metric::low_is_bad`] for the exception.
    /// - CPU %: 40 / 70.
    /// - Power W (SoC package): 10 (idle/light) / 20 (compile/render territory).
    /// - RAM % runs hotter than CPU by nature: 75 / 85.
    /// - CPU die °C: 60 / 80 (throttle territory).
    /// - Battery %: 20 / 10 (where macOS shows its low-battery warnings).
    pub fn thresholds(self) -> (f64, f64) {
        match self {
            Metric::Cpu => (40.0, 70.0),
            Metric::Power => (10.0, 20.0),
            Metric::Ram => (75.0, 85.0),
            Metric::Temp => (60.0, 80.0),
            Metric::Battery => (20.0, 10.0),
        }
    }

    /// True when lower values are worse (battery), flipping the thresholds.
    pub fn low_is_bad(self) -> bool {
        matches!(self, Metric::Battery)
    }

    pub fn level(self, value: f64) -> Level {
        let (warn, crit) = self.thresholds();
        let past = |t: f64| if self.low_is_bad() { value <= t } else { value >= t };
        if past(crit) {
            Level::Crit
        } else if past(warn) {
            Level::Warn
        } else {
            Level::Good
        }
    }
}

/// One value per metric, indexed by `Metric::slot`. NaN = no reading.
type Reading = [f64; history::METRICS];

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

    /// Take one sample of every metric.
    fn read(&mut self) -> Reading {
        let mut r = [f64::NAN; history::METRICS];

        self.sys.refresh_memory();
        let used = self.sys.used_memory() as f64; // bytes
        let total = self.sys.total_memory() as f64; // bytes
        r[Metric::Ram.slot()] = if total > 0.0 {
            used / total * 100.0
        } else {
            0.0
        };

        // CPU needs two refreshes spaced apart; we refresh every tick, so
        // the value is the load since the previous tick.
        self.sys.refresh_cpu_usage();
        r[Metric::Cpu.slot()] = self.sys.global_cpu_usage() as f64;

        // None on the first tick (the delta needs two samples) and if
        // IOReport refused to subscribe at startup.
        if let Some(w) = self.soc_power.as_mut().and_then(|p| p.watts()) {
            r[Metric::Power.slot()] = w;
        }

        // Hottest CPU die sensor. The PMU exposes several tdie probes; max
        // is what "CPU temp" colloquially means. NaN if there are none.
        self.components.refresh(false);
        r[Metric::Temp.slot()] = self
            .components
            .iter()
            .filter(|c| c.label().contains("tdie"))
            .filter_map(|c| c.temperature())
            .fold(f32::NAN, f32::max) as f64;

        if let Some(pct) = battery::percent() {
            r[Metric::Battery.slot()] = pct;
        }

        r
    }
}

/// One labeled, color-coded fragment per metric, e.g. `CPU  15%`.
fn fragment(metric: Metric, reading: &Reading, out: &mut Vec<Seg>) {
    let v = reading[metric.slot()];
    match metric {
        Metric::Ram => {
            out.push(Seg::new("R", Level::Neutral));
            out.push(Seg::new(format!("{}%", pad(v, 3, 0)), metric.level(v)));
        }
        Metric::Cpu => {
            out.push(Seg::new("C", Level::Neutral));
            out.push(Seg::new(format!("{}%", pad(v, 3, 0)), metric.level(v)));
        }
        Metric::Power if v.is_nan() => out.push(Seg::new("   —W", Level::Neutral)),
        Metric::Power => out.push(Seg::new(format!("{}W", pad(v, 4, 0)), metric.level(v))),
        Metric::Temp if v.is_nan() => out.push(Seg::new("  —°C", Level::Neutral)),
        Metric::Temp => out.push(Seg::new(format!("{}°C", pad(v, 3, 0)), metric.level(v))),
        Metric::Battery if v.is_nan() => out.push(Seg::new("B  —%", Level::Neutral)),
        Metric::Battery => {
            out.push(Seg::new("B", Level::Neutral));
            out.push(Seg::new(format!("{}%", pad(v, 3, 0)), metric.level(v)));
        }
    }
}

/// Compose one menu-bar line from a list of metrics.
fn line(reading: &Reading, metrics: &[Metric]) -> Vec<Seg> {
    let mut out = Vec::with_capacity(metrics.len() * 2 + 1);
    for (i, m) in metrics.iter().enumerate() {
        if i > 0 {
            out.push(Seg::new(" ", Level::Neutral));
        }
        fragment(*m, reading, &mut out);
    }
    out
}

/// Current wall-clock time in unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Right-align a number in `width` chars. The title renders in a fully
/// monospaced font, so space padding keeps `3%` and `15%` the same width and
/// nothing shifts as values change.
fn pad(value: f64, width: usize, decimals: usize) -> String {
    format!("{value:>width$.decimals$}")
}

/// Flatten segments to plain text for the single-line fallback path.
fn plain(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

/// Events delivered to the main loop from outside it.
enum UserEvent {
    Menu(MenuEvent),
}

/// Everything the main loop owns.
struct App {
    sampler: Sampler,
    /// Font + attribute dictionaries + line geometry, built once.
    renderer: two_line::Renderer,
    /// Shared with the graph window, which reads it while drawing.
    history: Rc<RefCell<History>>,
    /// Created lazily on StartCause::Init (must exist while loop is running).
    tray: Option<TrayIcon>,
    /// The NSStatusBarButton behind the tray icon, found once after creation;
    /// needed because the two-line display is drawn into its image.
    button: Option<Retained<NSStatusBarButton>>,
    /// Only exists while the window is open, so a closed window costs nothing.
    graphs: Option<graphs::GraphsWindow>,
    login_item: CheckMenuItem,
    login_id: MenuId,
    graphs_id: MenuId,
    quit_id: MenuId,
}

impl App {
    /// Sample, record, redraw the menu bar, and refresh the graphs if open.
    fn tick(&mut self) {
        // The pool guarantees ObjC temporaries (image, strings) die now,
        // not whenever the loop feels like draining — keeps RSS flat.
        objc2::rc::autoreleasepool(|_| {
            let reading = self.sampler.read();
            self.history
                .borrow_mut()
                .record(unix_now(), reading.map(|v| v as f32));

            let (top, bottom) = (line(&reading, LINE_TOP), line(&reading, LINE_BOTTOM));
            match &self.button {
                Some(b) => self.renderer.set_title(b, &top, &bottom),
                // Fallback: single line via the plain title, colors dropped.
                None => {
                    if let Some(t) = &self.tray {
                        t.set_title(Some(format!("{} · {}", plain(&top), plain(&bottom))));
                    }
                }
            }

            // Drop a window the user closed so its memory comes back.
            if self.graphs.as_ref().is_some_and(|g| !g.is_open()) {
                self.graphs = None;
            }
            if let Some(g) = &self.graphs {
                g.refresh();
            }
        });
    }

    /// Returns true when the app should quit.
    fn on_menu(&mut self, ev: MenuEvent) -> bool {
        if ev.id == self.quit_id {
            return true;
        }
        if ev.id == self.graphs_id {
            objc2::rc::autoreleasepool(|_| {
                if self.graphs.as_ref().is_some_and(|g| !g.is_open()) {
                    self.graphs = None;
                }
                let mtm = MainThreadMarker::new().expect("event loop runs on the main thread");
                let history = &self.history;
                self.graphs
                    .get_or_insert_with(|| graphs::GraphsWindow::new(mtm, history.clone()))
                    .show(mtm);
            });
        }
        if ev.id == self.login_id {
            // The native menu already flipped the checkmark; make the
            // registration match it, and re-sync on failure.
            let want = self.login_item.is_checked();
            match login_item::set_enabled(want) {
                Ok(now) => self.login_item.set_checked(now),
                Err(e) => {
                    eprintln!("launch-at-login: {e}");
                    self.login_item.set_checked(login_item::is_enabled());
                }
            }
        }
        false
    }
}

fn main() {
    // Accessory => menu-bar only, no Dock icon. The graphs window switches
    // to Regular (Dock icon, Cmd-Tab) while it's open — see graphs.rs.
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    // Wake the loop the moment a menu item is clicked, rather than waiting
    // for the next 2s tick. The handler runs on the main thread inside the
    // menu's action; it only forwards.
    let proxy = std::sync::Mutex::new(event_loop.create_proxy());
    MenuEvent::set_event_handler(Some(move |ev| {
        if let Ok(p) = proxy.lock() {
            let _ = p.send_event(UserEvent::Menu(ev));
        }
    }));

    let mut sampler = Sampler::new();
    // Prime CPU so the first reading is a real delta, not 0%/garbage.
    sampler.sys.refresh_cpu_usage();

    // Right-click menu: disabled "Featherbar" header (so the app name is
    // visible — the menu-bar icon is a numeric readout, not a glyph) +
    // Show Graphs + launch-at-login toggle + Quit.
    // SMAppService needs a real .app bundle (scripts/bundle.sh); from a bare
    // `cargo run` binary the item is shown disabled so it can't silently fail.
    let title_item = MenuItem::new("Featherbar", false, None);
    let graphs_item = MenuItem::new("Show Graphs", true, None);
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
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append(&title_item).expect("failed to build menu");
    menu.append(&PredefinedMenuItem::separator())
        .expect("failed to build menu");
    menu.append(&graphs_item).expect("failed to build menu");
    menu.append(&PredefinedMenuItem::separator())
        .expect("failed to build menu");
    menu.append(&login_item).expect("failed to build menu");
    menu.append(&quit_item).expect("failed to build menu");

    // Debug builds only: FEATHERBAR_HISTORY points at a scratch history
    // file, and FEATHERBAR_SEED fills it with a fake day of data.
    #[cfg(debug_assertions)]
    let history_path = std::env::var_os("FEATHERBAR_HISTORY")
        .map(std::path::PathBuf::from)
        .or_else(History::default_path);
    #[cfg(not(debug_assertions))]
    let history_path = History::default_path();
    #[allow(unused_mut)]
    let mut history = History::open(history_path.as_deref());
    #[cfg(debug_assertions)]
    if std::env::var_os("FEATHERBAR_SEED").is_some() {
        history.seed_demo(unix_now());
    }

    let mut app = App {
        sampler,
        renderer: two_line::Renderer::new(),
        history: Rc::new(RefCell::new(history)),
        tray: None,
        button: None,
        graphs: None,
        login_id: login_item.id().clone(),
        graphs_id: graphs_item.id().clone(),
        quit_id: quit_item.id().clone(),
        login_item,
    };

    event_loop.run(move |event, _target, control_flow| match event {
        Event::NewEvents(StartCause::Init) => {
            app.tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .build()
                    .expect("failed to create tray icon"),
            );
            let mtm = MainThreadMarker::new().expect("event loop runs on the main thread");
            app.button = two_line::status_button(mtm);
            app.tick();
            #[cfg(debug_assertions)]
            if let Some(dir) = std::env::var_os("FEATHERBAR_SNAPSHOT") {
                let g = graphs::GraphsWindow::new(mtm, app.history.clone());
                g.show(mtm);
                g.snapshot(std::path::Path::new(&dir));
                *control_flow = ControlFlow::Exit;
                return;
            }
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
            app.tick();
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        // control_flow is left alone, so the pending tick deadline stands.
        Event::UserEvent(UserEvent::Menu(ev)) => {
            if app.on_menu(ev) {
                *control_flow = ControlFlow::Exit;
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_thresholds() {
        assert!(matches!(Metric::Cpu.level(10.0), Level::Good));
        assert!(matches!(Metric::Cpu.level(40.0), Level::Warn));
        assert!(matches!(Metric::Cpu.level(95.0), Level::Crit));
        // Battery is inverted: low is bad.
        assert!(matches!(Metric::Battery.level(80.0), Level::Good));
        assert!(matches!(Metric::Battery.level(20.0), Level::Warn));
        assert!(matches!(Metric::Battery.level(5.0), Level::Crit));
    }
}
