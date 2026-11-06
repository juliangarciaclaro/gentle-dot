//! The Dot and the panel as layer surfaces (S9, T14): on a compositor that offers
//! `zwlr_layer_shell_v1` and places no windows for us (COSMIC), gtk-layer-shell is how a window
//! stays on top and gets a position. A layer surface is anchored to the `Top` and `Left` edges of
//! one output and moved by its margins, so these calls take a global rectangle in logical points
//! (the space `geometry` works in) and turn it into an output and margins. The decision to use
//! them is `platform::uses_layer_shell`; the placement math is `geometry::layer_margins`.
//!
//! GTK calls: the main thread only. Every window is initialized once, before its first show.

use crate::geometry::{self, Rect};
use gtk::gdk;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

pub const NAMESPACE: &str = "gentle-dot";

/// The exclusive zone that ignores other surfaces' zones, so margins count from the output's real
/// corner (a panel or dock at an edge would otherwise move the origin by an amount we cannot read).
const IGNORE_OTHER_ZONES: i32 = -1;
/// A zone of 0 keeps clear of other surfaces' zones: the full-screen panel leaves the dock visible.
const KEEP_CLEAR: i32 = 0;

/// Whether the compositor offers the layer shell. Needs GTK started, on the main thread.
pub fn supported() -> bool {
    gtk_layer_shell::is_supported()
}

fn monitors() -> Vec<gdk::Monitor> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    (0..display.n_monitors()).filter_map(|index| display.monitor(index)).collect()
}

fn rect_of(monitor: &gdk::Monitor) -> Rect {
    let area = monitor.geometry();
    Rect::new(area.x(), area.y(), area.width(), area.height())
}

/// The logical rectangle of every output, in the global space the compositor lays them out in.
pub fn outputs() -> Vec<Rect> {
    monitors().iter().map(rect_of).collect()
}

/// The primary output, or the first one when the compositor names none.
pub fn primary_output() -> Option<Rect> {
    let primary = gdk::Display::default().and_then(|display| display.primary_monitor());
    primary.as_ref().map(rect_of).or_else(|| outputs().first().copied())
}

fn anchor_top_left(window: &gtk::ApplicationWindow) {
    for (edge, anchored) in [(Edge::Top, true), (Edge::Left, true), (Edge::Bottom, false), (Edge::Right, false)] {
        window.set_anchor(edge, anchored);
    }
}

/// Makes the window a layer surface in `layer`, `size` logical points. Must run before the window
/// is realized.
///
/// The size request pins the window's requested size to `size`. Without it a WebKitWebView child
/// changes the request after the first show; GtkWindow then asks for a resize, freezes the
/// toplevel's updates until the matching `configure-event`, and a layer surface never sends one,
/// so the window never paints (T15b). It is a minimum: the full-screen panel still grows.
fn init(window: &gtk::ApplicationWindow, size: (i32, i32), layer: Layer, keyboard: KeyboardMode) {
    window.set_size_request(size.0, size.1);
    window.init_layer_shell();
    window.set_namespace(NAMESPACE);
    window.set_layer(layer);
    window.set_keyboard_mode(keyboard);
    window.set_exclusive_zone(IGNORE_OTHER_ZONES);
    anchor_top_left(window);
}

/// The Dot: above everything, on every workspace, never takes the keyboard.
pub fn init_dot(window: &gtk::ApplicationWindow, size: (i32, i32)) {
    init(window, size, Layer::Overlay, KeyboardMode::None);
}

/// The panel: above the windows, and it takes keys when the user clicks into it.
pub fn init_panel(window: &gtk::ApplicationWindow, size: (i32, i32)) {
    init(window, size, Layer::Top, KeyboardMode::OnDemand);
}

/// Puts the window's top-left corner at the global `rect` corner: on the output it is mostly on,
/// by margins from that output's origin. Moving to another output remaps the surface.
pub fn place(window: &gtk::ApplicationWindow, rect: Rect) -> Result<(), String> {
    let monitors = monitors();
    let rects: Vec<Rect> = monitors.iter().map(rect_of).collect();
    let (index, (left, top)) = geometry::layer_margins(rect, &rects).ok_or("no output to place the window on")?;
    if window.monitor().as_ref() != Some(&monitors[index]) {
        window.set_monitor(&monitors[index]);
    }
    window.set_layer_shell_margin(Edge::Left, left);
    window.set_layer_shell_margin(Edge::Top, top);
    Ok(())
}

/// The full-screen panel (S26.2): anchored to all four edges of `output` with no margins, inside
/// the work area, so the dock and top bar stay visible. `restore_corner` undoes it.
pub fn fill_output(window: &gtk::ApplicationWindow, output: usize) -> Result<(), String> {
    let monitors = monitors();
    let monitor = monitors.get(output).ok_or("the output is gone")?;
    if window.monitor().as_ref() != Some(monitor) {
        window.set_monitor(monitor);
    }
    for edge in [Edge::Top, Edge::Left, Edge::Bottom, Edge::Right] {
        window.set_anchor(edge, true);
        window.set_layer_shell_margin(edge, 0);
    }
    window.set_exclusive_zone(KEEP_CLEAR);
    Ok(())
}

/// Back to a corner-anchored window after `fill_output`; the caller places it right after.
pub fn restore_corner(window: &gtk::ApplicationWindow) {
    window.set_exclusive_zone(IGNORE_OTHER_ZONES);
    anchor_top_left(window);
}
