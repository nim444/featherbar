//! featherbar — a tiny, modular macOS menu-bar system monitor.
//!
//! Design goals: flat memory, no background work, easy to extend.
//! - Everything runs on the main thread (required by tray-icon on macOS).
//! - One owner (`Sampler`) holds all sampling state, so no per-tick allocation
//!   and no borrow-checker pain. RSS stays in the single-digit MB.
//! - Add a metric by adding a `Metric` variant + a `Sampler::fragment` arm.

use std::time::{Duration, Instant};

use sysinfo::System;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// How often to refresh the reading.
const REFRESH: Duration = Duration::from_secs(2);

/// Which stats to show, in order. Add a variant + a `fragment` arm to extend.
/// Do NOT add SoC package power or temperature (undocumented IOKit, breaks per
/// chip generation).
#[derive(Clone, Copy)]
enum Metric {
    Ram,
    Cpu,
    Power, // battery discharge wattage; only meaningful while on battery
}

const ENABLED: &[Metric] = &[Metric::Ram, Metric::Cpu, Metric::Power];

/// Single owner of all sampling state.
struct Sampler {
    sys: System,
    battery: Option<starship_battery::Manager>,
}

impl Sampler {
    fn new() -> Self {
        let sys = System::new_all();
        let battery = starship_battery::Manager::new().ok();
        Self { sys, battery }
    }

    /// One short fragment per metric, e.g. "RAM 47%", "CPU 12%", "8.3W".
    fn fragment(&mut self, metric: Metric) -> String {
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
                format!("RAM {pct:.0}%")
            }
            Metric::Cpu => {
                // CPU needs two refreshes spaced apart; we refresh every tick, so
                // the value is the load since the previous tick.
                self.sys.refresh_cpu_usage();
                format!("CPU {:.0}%", self.sys.global_cpu_usage())
            }
            Metric::Power => match &self.battery {
                Some(mgr) => match mgr.batteries().ok().and_then(|mut b| b.next()) {
                    // energy_rate() is a uom Power quantity; .value is watts.
                    Some(Ok(bat)) => format!("{:.1}W", bat.energy_rate().value),
                    _ => "—W".to_string(),
                },
                None => "—W".to_string(),
            },
        }
    }

    /// Compose the menu-bar title from the enabled metrics.
    fn title(&mut self) -> String {
        ENABLED
            .iter()
            .map(|m| self.fragment(*m))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn main() {
    // Accessory => menu-bar only, no Dock icon, no app window.
    let mut event_loop = EventLoopBuilder::new().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let mut sampler = Sampler::new();
    // Prime CPU so the first reading is a real delta, not 0%/garbage.
    sampler.sys.refresh_cpu_usage();

    // One-item right-click menu so the app is quittable.
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id: MenuId = quit_item.id().clone();
    let menu = Menu::new();
    menu.append(&quit_item).expect("failed to build menu");

    // Created lazily on StartCause::Init (must exist while loop is running).
    let mut tray: Option<TrayIcon> = None;

    event_loop.run(move |event, _target, control_flow| match event {
        Event::NewEvents(StartCause::Init) => {
            tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_title(sampler.title())
                    .build()
                    .expect("failed to create tray icon"),
            );
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
            if let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == quit_id {
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            if let Some(t) = &tray {
                t.set_title(Some(sampler.title()));
            }
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        }
        _ => {}
    });
}
