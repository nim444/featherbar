//! The "Show Graphs" window: four stacked graphs over a chosen time span.
//!
//! One custom view (`GraphView`) draws all four graphs; a standard
//! segmented control above it picks the span. The window is built when
//! opened and dropped by main.rs once it's closed, so it costs nothing while
//! hidden. Position and size are saved by AppKit's frame autosave; the
//! chosen span is saved in user defaults.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSColor, NSEvent,
    NSEventModifierFlags, NSFont, NSFontAttributeName, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSGraphicsContext, NSSegmentSwitchTracking,
    NSSegmentedControl, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_foundation::{
    ns_string, NSArray, NSDictionary, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSUserDefaults,
};

use crate::history::{downsample, History, Point, RAW_STEP};
use crate::{unix_now, Metric};

/// The selectable spans: control label, seconds, left-hand axis label.
const SPANS: [(&str, u64, &str); 5] = [
    ("Minute", 60, "1 min ago"),
    ("5 Minutes", 300, "5 min ago"),
    ("30 Minutes", 1800, "30 min ago"),
    ("Hour", 3600, "1 hr ago"),
    ("Day", 86_400, "24 hr ago"),
];
const DEFAULT_SPAN: usize = 1;

/// User-defaults key for the chosen span. Stored as index + 1 so that 0
/// (what an unset key reads as) means "use the default".
const SPAN_KEY: &str = "GraphSpan";
const AUTOSAVE_NAME: &str = "FeatherbarGraphs";

// Layout, in points.
const PAD_X: f64 = 16.0;
const PAD_TOP: f64 = 6.0;
const GAP: f64 = 14.0;
const HEADER_H: f64 = 20.0;
const AXIS_H: f64 = 22.0;
/// Width of one smoothing bucket. Longer spans merge many samples into
/// each bucket; the line shows their average and a band their low–high.
const BUCKET_PT: f64 = 3.0;

fn label(metric: Metric) -> &'static str {
    match metric {
        Metric::Cpu => "CPU",
        Metric::Power => "Power",
        Metric::Ram => "Memory",
        Metric::Temp => "Temperature",
        Metric::Battery => "Battery",
    }
}

fn fmt(metric: Metric, v: f32) -> String {
    match metric {
        Metric::Cpu | Metric::Ram | Metric::Battery => format!("{v:.0}%"),
        Metric::Power => format!("{v:.1} W"),
        Metric::Temp => format!("{v:.0}°C"),
    }
}

/// Whole-number label for an axis or threshold value.
fn axis_label(metric: Metric, v: f64) -> String {
    match metric {
        Metric::Cpu | Metric::Ram | Metric::Battery => format!("{v:.0}%"),
        Metric::Power => format!("{v:.0} W"),
        Metric::Temp => format!("{v:.0}°C"),
    }
}

/// Vertical range of a graph. Percentages are fixed at 0–100; power and
/// temperature stretch to fit a high peak but never shrink below a range
/// that shows all three bands.
fn scale(metric: Metric, peak: Option<f32>) -> (f64, f64) {
    let peak = peak.unwrap_or(0.0) as f64;
    match metric {
        Metric::Cpu | Metric::Ram | Metric::Battery => (0.0, 100.0),
        Metric::Power => (0.0, (peak * 1.15 / 5.0).ceil().max(5.0) * 5.0),
        Metric::Temp => (20.0, (peak / 10.0).ceil().max(10.0) * 10.0),
    }
}

/// Local wall-clock time, `HH:MM` or `HH:MM:SS`.
fn clock(t: u64, seconds: bool) -> String {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    if seconds {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    } else {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

type Attrs = Retained<NSDictionary<NSString, AnyObject>>;

fn attrs(font: &NSFont, color: &NSColor) -> Attrs {
    use objc2::Message;
    NSDictionary::from_retained_objects(
        &[unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }],
        &[
            Retained::into_super(Retained::into_super(font.retain())),
            Retained::into_super(Retained::into_super(color.retain())),
        ],
    )
}

fn text_width(s: &str, a: &Attrs) -> f64 {
    unsafe { NSString::from_str(s).sizeWithAttributes(Some(a)) }.width
}

fn draw_text(s: &str, x: f64, y: f64, a: &Attrs) {
    unsafe { NSString::from_str(s).drawAtPoint_withAttributes(NSPoint::new(x, y), Some(a)) };
}

/// Text styles for one draw pass. Built per draw (cheap) so dynamic system
/// colors always match the current light/dark appearance.
struct Styles {
    title: Attrs,
    stats: Attrs,
    small: Attrs,
    stats_font: Retained<NSFont>,
}

impl Styles {
    fn new() -> Self {
        let title_font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightSemibold });
        let stats_font =
            NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { NSFontWeightRegular });
        let small_font =
            NSFont::monospacedDigitSystemFontOfSize_weight(10.0, unsafe { NSFontWeightRegular });
        Self {
            title: attrs(&title_font, &NSColor::labelColor()),
            stats: attrs(&stats_font, &NSColor::secondaryLabelColor()),
            small: attrs(&small_font, &NSColor::tertiaryLabelColor()),
            stats_font,
        }
    }
}

pub struct Ivars {
    history: Rc<RefCell<History>>,
    span: Cell<usize>,
    /// Mouse x in view coordinates while hovering.
    hover: Cell<Option<f64>>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; GraphView has no Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FeatherbarGraphView"]
    #[ivars = Ivars]
    pub struct GraphView;

    impl GraphView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            self.ivars().hover.set(Some(p.x));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover.set(None);
            self.setNeedsDisplay(true);
        }

        /// Action of the span segmented control.
        #[unsafe(method(spanChanged:))]
        fn span_changed(&self, sender: &NSSegmentedControl) {
            let i = (sender.selectedSegment().max(0) as usize).min(SPANS.len() - 1);
            self.ivars().span.set(i);
            NSUserDefaults::standardUserDefaults()
                .setInteger_forKey(i as isize + 1, &NSString::from_str(SPAN_KEY));
            self.setNeedsDisplay(true);
        }

        /// ⌘W closes the window (an accessory app has no menu bar to
        /// provide the usual File ▸ Close shortcut).
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let cmd = event.modifierFlags().contains(NSEventModifierFlags::Command);
            let w = event
                .charactersIgnoringModifiers()
                .is_some_and(|c| c.to_string() == "w");
            match self.window() {
                Some(window) if cmd && w => {
                    window.performClose(None);
                    true
                }
                _ => unsafe { msg_send![super(self), performKeyEquivalent: event] },
            }
        }
    }

    unsafe impl NSObjectProtocol for GraphView {}

    // The view doubles as its window's delegate, to hear about the close.
    unsafe impl NSWindowDelegate for GraphView {
        /// Drop the Dock icon (and Cmd-Tab entry) the moment the window closes.
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            NSApplication::sharedApplication(mtm)
                .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    }
);

impl GraphView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        history: Rc<RefCell<History>>,
        span: usize,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            history,
            span: Cell::new(span),
            hover: Cell::new(None),
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // InVisibleRect keeps the area matched to the view as it resizes.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(view.as_ref()),
                None,
            )
        };
        view.addTrackingArea(&area);
        view
    }

    fn draw(&self) {
        let iv = self.ivars();
        let (_, span, ago) = SPANS[iv.span.get()];
        let day = span > 3600;
        let now = unix_now();
        let start = now.saturating_sub(span);
        let history = iv.history.borrow();
        let styles = Styles::new();

        let b = self.bounds();
        let n = Metric::ALL.len() as f64;
        let cell_h = ((b.size.height - PAD_TOP - AXIS_H - GAP * (n - 1.0)) / n).max(HEADER_H + 10.0);
        let plot_w = (b.size.width - 2.0 * PAD_X).max(10.0);
        let x_of = |t: u64| PAD_X + (t.saturating_sub(start)) as f64 / span as f64 * plot_w;

        // Hover → the time under the mouse, if it's over the plots.
        let hover_t = iv
            .hover
            .get()
            .filter(|&x| x >= PAD_X && x <= PAD_X + plot_w)
            .map(|x| start as f64 + (x - PAD_X) / plot_w * span as f64);
        // Seconds per smoothing bucket: about BUCKET_PT wide, never finer
        // than the data itself.
        let step = if day { 60 } else { RAW_STEP };
        let bucket = ((span as f64 * BUCKET_PT / plot_w).ceil() as u64).max(step);
        let precise = bucket < 60; // show seconds in hover times
        // How far from the mouse a point may be and still count as "under" it.
        let tolerance = bucket as f64 * 1.5;

        let mut last_plot = NSRect::ZERO;
        for (i, &metric) in Metric::ALL.iter().enumerate() {
            let y0 = PAD_TOP + i as f64 * (cell_h + GAP);
            let plot = NSRect::new(
                NSPoint::new(PAD_X, y0 + HEADER_H),
                NSSize::new(plot_w, cell_h - HEADER_H),
            );
            last_plot = plot;
            let series = history.series(metric.slot(), now, span);
            let points = || series.iter().flatten();
            let drawn = downsample(&series, start, bucket);

            let peak = points().map(|p| p.max).reduce(f32::max);
            let avg = {
                let (sum, count) = points().fold((0.0f64, 0u32), |(s, c), p| (s + p.avg as f64, c + 1));
                (count > 0).then(|| (sum / count as f64) as f32)
            };
            let (lo, hi) = scale(metric, peak);
            let y_of = |v: f32| {
                let v = (v as f64).clamp(lo, hi);
                plot.origin.y + plot.size.height - (v - lo) / (hi - lo) * plot.size.height
            };

            // Header: name on the left; stats (or the hovered value) on the right.
            draw_text(label(metric), PAD_X, y0 + 2.0, &styles.title);
            let hovered = hover_t.and_then(|ht| {
                drawn
                    .iter()
                    .flatten()
                    .min_by(|a, b| {
                        (a.t as f64 - ht).abs().total_cmp(&(b.t as f64 - ht).abs())
                    })
                    .filter(|p| (p.t as f64 - ht).abs() <= tolerance)
                    .copied()
            });
            let right = match (hover_t, hovered) {
                (Some(_), Some(p)) if p.min < p.max => format!(
                    "{}   avg {}   low {}   peak {}",
                    clock(p.t, precise),
                    fmt(metric, p.avg),
                    fmt(metric, p.min),
                    fmt(metric, p.max)
                ),
                (Some(_), Some(p)) => format!("{}   {}", clock(p.t, precise), fmt(metric, p.avg)),
                (Some(ht), None) => format!("{}   no data", clock(ht as u64, precise)),
                (None, _) => {
                    let f = |v: Option<f32>| v.map_or("—".to_string(), |v| fmt(metric, v));
                    // For battery the worst value is the lowest, so show that.
                    let (extreme_label, extreme) = if metric.low_is_bad() {
                        ("low", points().map(|p| p.min).reduce(f32::min))
                    } else {
                        ("peak", peak)
                    };
                    format!(
                        "now {}   avg {}   {extreme_label} {}",
                        f(history.latest(metric.slot(), now)),
                        f(avg),
                        f(extreme)
                    )
                }
            };
            let rw = text_width(&right, &styles.stats);
            let baseline_fix = 12.0 - styles.stats_font.pointSize(); // align 11pt with 12pt title
            draw_text(&right, PAD_X + plot_w - rw, y0 + 3.0 + baseline_fix, &styles.stats);

            // Background bands at the menu bar's colour thresholds.
            let (warn, crit) = metric.thresholds();
            let band = |from: f64, to: f64, color: Retained<NSColor>| {
                let (a, b) = (from.clamp(lo, hi), to.clamp(lo, hi));
                if b <= a {
                    return;
                }
                let top = y_of(b as f32);
                let bottom = y_of(a as f32);
                color.colorWithAlphaComponent(0.13).setFill();
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(plot.origin.x, top),
                    NSSize::new(plot.size.width, bottom - top),
                ));
            };
            // `low`/`high` are the two threshold lines in value order; for
            // battery (low is bad) the colours run red → orange → green.
            let (low, high) = (warn.min(crit), warn.max(crit));
            let (green, orange, red) = (
                NSColor::systemGreenColor(),
                NSColor::systemOrangeColor(),
                NSColor::systemRedColor(),
            );
            let (below, above) = if metric.low_is_bad() { (red, green) } else { (green, red) };
            band(lo, low, below);
            band(low, high, orange);
            band(high, hi, above);

            // Threshold values: the upper line's sits just above it, the
            // lower line's just below it, so they never collide however
            // close the lines are. The scale top is shown only when it has room.
            let lx = plot.origin.x + 4.0;
            let high_y = y_of(high as f32) - 13.0;
            draw_text(&axis_label(metric, high), lx, high_y, &styles.small);
            draw_text(&axis_label(metric, low), lx, y_of(low as f32) + 1.0, &styles.small);
            if plot.origin.y + 1.0 + 12.0 <= high_y {
                draw_text(&axis_label(metric, hi), lx, plot.origin.y + 1.0, &styles.small);
            }

            NSColor::separatorColor().setStroke();
            NSBezierPath::strokeRect(NSRect::new(
                NSPoint::new(plot.origin.x + 0.5, plot.origin.y + 0.5),
                NSSize::new(plot.size.width - 1.0, plot.size.height - 1.0),
            ));

            NSGraphicsContext::saveGraphicsState_class();
            NSBezierPath::clipRect(plot);
            draw_series(&drawn, &x_of, &y_of);
            if let Some(p) = hovered {
                let r = 3.0;
                NSColor::controlAccentColor().setFill();
                NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                    NSPoint::new(x_of(p.t) - r, y_of(p.avg) - r),
                    NSSize::new(2.0 * r, 2.0 * r),
                ))
                .fill();
            }
            NSGraphicsContext::restoreGraphicsState_class();

            if points().next().is_none() {
                let msg = "No data in this period";
                let w = text_width(msg, &styles.stats);
                draw_text(
                    msg,
                    plot.origin.x + (plot.size.width - w) / 2.0,
                    plot.origin.y + plot.size.height / 2.0 - 7.0,
                    &styles.stats,
                );
            }

            // Hover crosshair through every graph.
            if let Some(x) = iv.hover.get().filter(|_| hover_t.is_some()) {
                let path = NSBezierPath::bezierPath();
                path.moveToPoint(NSPoint::new(x.round() + 0.5, plot.origin.y));
                path.lineToPoint(NSPoint::new(x.round() + 0.5, plot.origin.y + plot.size.height));
                path.setLineWidth(1.0);
                NSColor::secondaryLabelColor().setStroke();
                path.stroke();
            }
        }

        // Time axis under the last graph.
        let ay = last_plot.origin.y + last_plot.size.height + 4.0;
        draw_text(ago, PAD_X, ay, &styles.small);
        let w = text_width("now", &styles.small);
        draw_text("now", PAD_X + plot_w - w, ay, &styles.small);
        if day || span >= 1800 {
            // Midpoint clock time helps orient the longer spans.
            let mid = clock(start + span / 2, false);
            let mw = text_width(&mid, &styles.small);
            draw_text(&mid, PAD_X + (plot_w - mw) / 2.0, ay, &styles.small);
        }
    }
}

/// Draw one metric's average line, breaking at gaps, over a lighter band
/// from each point's low to its high so spikes and dips stay visible.
fn draw_series(
    series: &[Option<Point>],
    x_of: &dyn Fn(u64) -> f64,
    y_of: &dyn Fn(f32) -> f64,
) {
    let accent = NSColor::controlAccentColor();
    for run in series.split(Option::is_none) {
        let run: Vec<Point> = run.iter().flatten().copied().collect();
        let Some(first) = run.first() else { continue };

        if run.len() == 1 {
            // A lone sample between gaps: a dot, since a line would be invisible.
            let r = 1.5;
            accent.setFill();
            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(x_of(first.t) - r, y_of(first.avg) - r),
                NSSize::new(2.0 * r, 2.0 * r),
            ))
            .fill();
            continue;
        }

        if run.iter().any(|p| p.min < p.max) {
            let band = NSBezierPath::bezierPath();
            band.moveToPoint(NSPoint::new(x_of(first.t), y_of(first.max)));
            for p in &run[1..] {
                band.lineToPoint(NSPoint::new(x_of(p.t), y_of(p.max)));
            }
            for p in run.iter().rev() {
                band.lineToPoint(NSPoint::new(x_of(p.t), y_of(p.min)));
            }
            band.closePath();
            accent.colorWithAlphaComponent(0.25).setFill();
            band.fill();
        }

        let line = NSBezierPath::bezierPath();
        line.moveToPoint(NSPoint::new(x_of(first.t), y_of(first.avg)));
        for p in &run[1..] {
            line.lineToPoint(NSPoint::new(x_of(p.t), y_of(p.avg)));
        }
        line.setLineWidth(1.5);
        accent.setStroke();
        line.stroke();
    }
}

/// The graphs window. Dropping it releases the window and everything in it.
pub struct GraphsWindow {
    window: Retained<NSWindow>,
    view: Retained<GraphView>,
}

impl GraphsWindow {
    pub fn new(mtm: MainThreadMarker, history: Rc<RefCell<History>>) -> Self {
        let size = NSSize::new(640.0, 720.0);
        let rect = NSRect::new(NSPoint::ZERO, size);
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // We own the window through `Retained`; AppKit mustn't free it on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(ns_string!("Featherbar"));
        window.setContentMinSize(NSSize::new(420.0, 500.0));
        window.setTabbingMode(NSWindowTabbingMode::Disallowed);

        let saved = NSUserDefaults::standardUserDefaults()
            .integerForKey(&NSString::from_str(SPAN_KEY));
        let span = if (1..=SPANS.len() as isize).contains(&saved) {
            saved as usize - 1
        } else {
            DEFAULT_SPAN
        };

        let content = NSView::initWithFrame(NSView::alloc(mtm), rect);

        let labels: Vec<Retained<NSString>> =
            SPANS.iter().map(|(l, _, _)| NSString::from_str(l)).collect();
        let labels = NSArray::from_retained_slice(&labels);
        // Target is set once the view exists.
        let control = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels,
                NSSegmentSwitchTracking::SelectOne,
                None,
                Some(sel!(spanChanged:)),
                mtm,
            )
        };
        control.setSelectedSegment(span as isize);
        let cs = control.frame().size;
        control.setFrame(NSRect::new(
            NSPoint::new(((size.width - cs.width) / 2.0).round(), size.height - 12.0 - cs.height),
            cs,
        ));
        // Stay centered horizontally and pinned to the top.
        control.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin,
        );

        let graph_frame = NSRect::new(
            NSPoint::ZERO,
            NSSize::new(size.width, size.height - cs.height - 22.0),
        );
        let view = GraphView::new(mtm, graph_frame, history, span);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let target: &AnyObject = view.as_ref();
        unsafe { control.setTarget(Some(target)) };
        // Weak reference; the window keeps the view alive.
        window.setDelegate(Some(ProtocolObject::from_ref(&*view)));

        content.addSubview(&control);
        content.addSubview(&view);
        window.setContentView(Some(&content));

        // First open: centered. After that AppKit restores and keeps saving
        // the frame under this name.
        window.center();
        let name = ns_string!(AUTOSAVE_NAME);
        window.setFrameUsingName(name);
        window.setFrameAutosaveName(name);

        Self { window, view }
    }

    /// Bring the window to the front, taking focus from the current app.
    /// While it's open the app is a regular app (Dock icon, Cmd-Tab entry)
    /// so the window can be switched to like any other; closing it goes
    /// back to menu-bar only.
    pub fn show(&self, mtm: MainThreadMarker) {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        self.window.makeKeyAndOrderFront(None);
    }

    /// False once the user has closed it (minimized still counts as open).
    pub fn is_open(&self) -> bool {
        self.window.isVisible() || self.window.isMiniaturized()
    }

    /// Redraw with the latest samples.
    pub fn refresh(&self) {
        self.view.setNeedsDisplay(true);
    }
}

/// Debug builds only: draw the window into PNGs without touching the
/// screen, for checking layout. `FEATHERBAR_SNAPSHOT=/dir` writes one image
/// per span plus a hovered one, then the app exits.
#[cfg(debug_assertions)]
impl GraphsWindow {
    pub fn snapshot(&self, dir: &std::path::Path) {
        use objc2_app_kit::NSBitmapImageFileType;
        let Some(content) = self.window.contentView() else { return };
        let write = |name: &str| {
            let b = content.bounds();
            let Some(rep) = content.bitmapImageRepForCachingDisplayInRect(b) else { return };
            content.cacheDisplayInRect_toBitmapImageRep(b, &rep);
            let png = unsafe {
                rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
            };
            if let Some(png) = png {
                let _ = std::fs::write(dir.join(name), png.to_vec());
            }
        };
        let iv = self.view.ivars();
        for (i, (label, _, _)) in SPANS.iter().enumerate() {
            iv.span.set(i);
            write(&format!("{i}-{}.png", label.replace(' ', "")));
        }
        iv.span.set(1);
        iv.hover.set(Some(self.view.bounds().size.width * 0.62));
        write("hover-5min.png");
        iv.span.set(4);
        write("hover-day.png");
        iv.hover.set(None);
    }
}
