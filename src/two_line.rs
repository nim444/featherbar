//! Two stacked, color-coded lines in the menu bar.
//!
//! NSStatusItem text titles are vertically centered by the button cell with
//! no working override (baseline offsets and padding lines both get
//! neutralized), so the two lines are drawn into an NSImage instead — pixels
//! land exactly where we put them. The image is rebuilt each tick from the
//! current readings and set on the status item's NSStatusBarButton, which
//! tray-icon doesn't expose but which lives in this process's own
//! NSStatusBarWindow.
//!
//! Each line is a list of [`Seg`]ments so values can carry their own
//! severity color while labels stay neutral.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, Message};
use objc2_app_kit::{
    NSApplication, NSAttributedStringNSStringDrawing, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSImage, NSStatusBarButton, NSView,
};
use objc2_foundation::{NSDictionary, NSMutableAttributedString, NSPoint, NSSize, NSString};

/// Title font size (SF Mono, Medium).
const FONT_SIZE: f64 = 12.0;

/// Vertical gap between the two lines, in points.
const LINE_GAP: f64 = 2.0;

/// Height of the rendered image — the menu bar's usable content height.
const HEIGHT: f64 = 22.0;

/// Manual nudge in points; positive moves the text up. Start at 0 — the
/// glyph-metric math below centers exactly, this is taste adjustment only.
const NUDGE: f64 = 0.0;

/// Severity of a value, mapped to a color in the menu bar.
#[derive(Clone, Copy)]
pub enum Level {
    Neutral, // labels, separators, missing readings
    Good,    // green
    Warn,    // orange
    Crit,    // red
}

/// One run of text with one color.
pub struct Seg {
    pub text: String,
    pub level: Level,
}

impl Seg {
    pub fn new(text: impl Into<String>, level: Level) -> Self {
        Self {
            text: text.into(),
            level,
        }
    }
}

/// Depth-first search for the status-bar button inside a view tree.
fn find_button(view: &NSView) -> Option<Retained<NSStatusBarButton>> {
    if let Ok(button) = view.retain().downcast::<NSStatusBarButton>() {
        return Some(button);
    }
    for sub in view.subviews() {
        if let Some(button) = find_button(&sub) {
            return Some(button);
        }
    }
    None
}

/// Locate this app's NSStatusBarButton (the tray icon tray-icon created).
pub fn status_button(mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
    let app = NSApplication::sharedApplication(mtm);
    for window in app.windows() {
        // The status item's host window is a private NSStatusBarWindow.
        if window.class().name().to_bytes() == b"NSStatusBarWindow" {
            if let Some(view) = window.contentView() {
                if let Some(button) = find_button(&view) {
                    return Some(button);
                }
            }
        }
    }
    None
}

fn color(level: Level) -> Retained<NSColor> {
    match level {
        // labelColor adapts to the menu bar's light/dark appearance.
        Level::Neutral => NSColor::labelColor(),
        Level::Good => NSColor::systemGreenColor(),
        Level::Warn => NSColor::systemOrangeColor(),
        Level::Crit => NSColor::systemRedColor(),
    }
}

/// Immutable drawing state built once at startup: the font, its vertical
/// metrics, and one attribute dictionary per severity level. Per tick only
/// the attributed strings and the image are created (and autoreleased).
pub struct Renderer {
    attrs: [Retained<NSDictionary<NSString, AnyObject>>; 4],
    y_top: f64,
    y_bottom: f64,
}

impl Renderer {
    pub fn new() -> Self {
        // Full monospace (SF Mono): every glyph — digits, %, W, °C, labels —
        // is the same width, so the two lines grid perfectly.
        let font = NSFont::monospacedSystemFontOfSize_weight(FONT_SIZE, unsafe {
            objc2_app_kit::NSFontWeightMedium
        });

        let attrs = [Level::Neutral, Level::Good, Level::Warn, Level::Crit].map(|level| {
            NSDictionary::from_retained_objects(
                &[unsafe { NSFontAttributeName }, unsafe {
                    NSForegroundColorAttributeName
                }],
                &[
                    Retained::into_super(Retained::into_super(font.retain())),
                    Retained::into_super(Retained::into_super(color(level))),
                ],
            )
        });

        // Center the *visible* glyphs: our text is caps/digits only (no
        // descenders), so each line's ink spans capHeight starting at the
        // baseline. drawAtPoint's y is the line box bottom = baseline -
        // descent.
        let cap = font.capHeight();
        let descent = -font.descender();
        let margin = (HEIGHT - 2.0 * cap - LINE_GAP) / 2.0;
        Self {
            attrs,
            y_bottom: margin - descent + NUDGE,
            y_top: margin + cap + LINE_GAP - descent + NUDGE,
        }
    }

    /// Build one line's attributed string from its segments.
    fn line_string(&self, segs: &[Seg]) -> Retained<NSMutableAttributedString> {
        let astr = NSMutableAttributedString::new();
        for seg in segs {
            let run = unsafe {
                objc2_foundation::NSAttributedString::new_with_attributes(
                    &NSString::from_str(&seg.text),
                    &self.attrs[seg.level as usize],
                )
            };
            astr.appendAttributedString(&run);
        }
        astr
    }

    /// Render the two stacked lines into an image and set it on the button.
    pub fn set_title(&self, button: &NSStatusBarButton, top: &[Seg], bottom: &[Seg]) {
        let l_top = self.line_string(top);
        let l_bottom = self.line_string(bottom);

        let width = l_top.size().width.max(l_bottom.size().width).ceil();

        let image = NSImage::initWithSize(
            NSImage::alloc(),
            NSSize {
                width,
                height: HEIGHT,
            },
        );
        // lockFocus is deprecated in favor of block-based drawing, but it's
        // the simplest path without pulling in the block2 crate, and renders
        // at the screen's backing scale (Retina-sharp).
        #[allow(deprecated)]
        {
            image.lockFocus();
            l_bottom.drawAtPoint(NSPoint {
                x: 0.0,
                y: self.y_bottom,
            });
            l_top.drawAtPoint(NSPoint {
                x: 0.0,
                y: self.y_top,
            });
            image.unlockFocus();
        }

        button.setImage(Some(&image));
    }
}
