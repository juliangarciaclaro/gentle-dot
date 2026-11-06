//! The Dot and the panel as layer surfaces (S9, T14): on a compositor that offers
//! `zwlr_layer_shell_v1` and places no windows for us (COSMIC), gtk-layer-shell is how a window
//! stays on top and gets a position. A layer surface is anchored to the `Top` and `Left` edges of
//! one output and moved by its margins, so these calls take a global rectangle in logical points
//! (the space `geometry` works in) and turn it into an output and margins. The decision to use
//! them is `platform::uses_layer_shell`; the placement math is `geometry::layer_margins`.
//!
//! GTK calls: the main thread only. Every window is initialized once, before its first show.

use crate::geometry::{self, DotDrag, DragStep, Rect};
use gtk::gdk;
use gtk::glib::Propagation;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub const NAMESPACE: &str = "gentle-dot";
const DIALOG_NAMESPACE: &str = "gentle-dot-dialog";

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

/// The computer-control glow (T15h): above everything, filling its output, and it never takes
/// the keyboard, so the app being controlled keeps its focus and its open menus. The pointer goes
/// through it by its input region, which the caller sets.
pub fn init_glow(window: &gtk::ApplicationWindow) {
    init(window, (1, 1), Layer::Overlay, KeyboardMode::None);
    for edge in [Edge::Top, Edge::Left, Edge::Bottom, Edge::Right] {
        window.set_anchor(edge, true);
    }
}

/// Moves the glow onto the output the global `rect` is mostly on; its anchors make it fill it.
pub fn cover(window: &gtk::ApplicationWindow, rect: Rect) -> Result<(), String> {
    let monitors = monitors();
    let rects: Vec<Rect> = monitors.iter().map(rect_of).collect();
    let (index, _) = geometry::layer_margins(rect, &rects).ok_or("no output to cover")?;
    if window.monitor().as_ref() != Some(&monitors[index]) {
        window.set_monitor(&monitors[index]);
    }
    Ok(())
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

/// Which output a confirmation dialog opens on: the one under the pointer, else the Dot's, else
/// `None` (the compositor picks). A pointer that is on no output (stale or not reported) does not
/// count.
fn dialog_output(pointer: Option<(i32, i32)>, dot_output: Option<usize>, outputs: &[Rect]) -> Option<usize> {
    let under_pointer = pointer.and_then(|(x, y)| {
        outputs.iter().position(|o| x >= o.x && x < o.x + o.width && y >= o.y && y < o.y + o.height)
    });
    under_pointer.or(dot_output.filter(|index| *index < outputs.len()))
}

/// The pointer's global position in logical points, as GDK last saw it.
fn pointer_position() -> Option<(i32, i32)> {
    let (_, x, y) = pointer_seat()?.pointer()?.position();
    Some((x, y))
}

/// Makes a confirmation dialog a layer surface: `Overlay` (above every window, which
/// `set_keep_above` cannot do on Wayland), centred on one output (no anchors), keyboard on demand
/// so it takes the focus when shown and Return/Escape work while the user can still reach other
/// apps. `dot` is the Dot's window, whose output is used when the pointer's is unknown. Must run
/// before the dialog is realized; `size` is pinned first for the same reason as in `init` (T15b).
pub fn init_dialog(dialog: &gtk::Window, size: (i32, i32), dot: Option<&gtk::ApplicationWindow>) {
    dialog.set_size_request(size.0, size.1);
    dialog.init_layer_shell();
    dialog.set_namespace(DIALOG_NAMESPACE);
    dialog.set_layer(Layer::Overlay);
    dialog.set_keyboard_mode(KeyboardMode::OnDemand);
    dialog.set_exclusive_zone(IGNORE_OTHER_ZONES);
    for edge in [Edge::Top, Edge::Left, Edge::Bottom, Edge::Right] {
        dialog.set_anchor(edge, false);
    }
    let monitors = monitors();
    let rects: Vec<Rect> = monitors.iter().map(rect_of).collect();
    let dot_output = dot.and_then(|dot| dot.monitor()).and_then(|monitor| monitors.iter().position(|m| *m == monitor));
    if let Some(index) = dialog_output(pointer_position(), dot_output, &rects) {
        dialog.set_monitor(&monitors[index]);
    }
}

/// What the pointer does to the Dot once a press on it becomes a drag.
pub enum DotEvent {
    /// Put the Dot at `to`; `started` on the first step of the drag.
    Moved(DragStep),
    /// The button went up after a drag.
    Dropped,
}

/// The web view inside `widget`, if any.
fn find_web_view(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.type_().name() == "WebKitWebView" {
        return Some(widget.clone());
    }
    widget.downcast_ref::<gtk::Container>()?.children().iter().find_map(find_web_view)
}

/// Whether this is the first widget to report the event: the web view and the window both see a
/// press that propagates up, with the same time stamp. Events without a time stamp always count.
fn first_report(seen: &Cell<Option<(gdk::EventType, u32)>>, kind: gdk::EventType, time: u32) -> bool {
    if time != 0 && seen.get() == Some((kind, time)) {
        return false;
    }
    seen.set(Some((kind, time)));
    true
}

fn pointer_seat() -> Option<gdk::Seat> {
    gdk::Display::default()?.default_seat()
}

/// Keeps every pointer event of the drag coming to this client. The press gives an implicit grab
/// on the web view's own GdkWindow, and that one stops getting motion once the pointer leaves the
/// surface (the release still arrives), so the Dot would move only as far as its own square (L39,
/// T15c). An explicit grab with `owner_events` sends the motion outside the surface to the window
/// and leaves the events inside it to the web view.
fn grab_pointer(widget: &gtk::Widget) {
    let (Some(seat), Some(window)) = (pointer_seat(), widget.toplevel().and_then(|top| top.window())) else {
        return;
    };
    let status = seat.grab(&window, gdk::SeatCapabilities::POINTER, true, None, None, None);
    if status != gdk::GrabStatus::Success {
        eprintln!("gentle-dot: cannot grab the pointer for the drag: {status:?}");
    }
}

fn release_pointer() {
    if let Some(seat) = pointer_seat() {
        seat.ungrab();
    }
}

/// Follows the pointer for the Dot's whole drag from GTK, with an explicit pointer grab from the
/// moment the press becomes a drag (see `grab_pointer`). `dot` is where the Dot is now. The
/// handlers run before the widgets' own, never stop an event, and so leave the click to the web
/// view. Call before the window is realized.
pub fn watch_dot_drag(
    window: &gtk::ApplicationWindow,
    dot: impl Fn() -> (i32, i32) + 'static,
    on: impl Fn(DotEvent) + 'static,
) {
    let drag = Rc::new(RefCell::new(DotDrag::default()));
    let seen = Rc::new(Cell::new(None));
    let (dot, on) = (Rc::new(dot), Rc::new(on));
    let view = find_web_view(window.upcast_ref());
    let mut widgets: Vec<gtk::Widget> = vec![window.clone().upcast()];
    widgets.extend(view.clone());
    let primary = |button: u32| button == 1;
    for widget in widgets {
        widget.add_events(gdk::EventMask::BUTTON_PRESS_MASK | gdk::EventMask::BUTTON_RELEASE_MASK | gdk::EventMask::POINTER_MOTION_MASK);
        widget.connect_button_press_event({
            let (drag, seen) = (drag.clone(), seen.clone());
            move |_, event| {
                let kind = event.event_type();
                if primary(event.button()) && kind == gdk::EventType::ButtonPress && first_report(&seen, kind, event.time()) {
                    drag.borrow_mut().press(event.position());
                }
                Propagation::Proceed
            }
        });
        widget.connect_motion_notify_event({
            let (drag, seen, dot, on) = (drag.clone(), seen.clone(), dot.clone(), on.clone());
            move |widget, event| {
                if !first_report(&seen, event.event_type(), event.time()) {
                    return Propagation::Proceed;
                }
                if !event.state().contains(gdk::ModifierType::BUTTON1_MASK) {
                    // The release went elsewhere: the press is over.
                    if drag.borrow_mut().release() {
                        release_pointer();
                        on(DotEvent::Dropped);
                    }
                    return Propagation::Proceed;
                }
                let step = drag.borrow_mut().motion(event.position(), dot());
                if let Some(step) = step {
                    if step.started {
                        grab_pointer(widget.upcast_ref());
                    }
                    on(DotEvent::Moved(step));
                }
                Propagation::Proceed
            }
        });
        widget.connect_button_release_event({
            let (drag, seen, on, view) = (drag.clone(), seen.clone(), on.clone(), view.clone());
            move |widget, event| {
                if primary(event.button()) && first_report(&seen, event.event_type(), event.time()) && drag.borrow_mut().release() {
                    release_pointer();
                    // Under the grab the window gets the release and the web view never does: it
                    // would keep the button down and swallow the next click. Hand it over.
                    if let Some(view) = view.as_ref().filter(|view| *view != widget) {
                        view.event(event);
                    }
                    on(DotEvent::Dropped);
                }
                Propagation::Proceed
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dialog_opens_on_the_pointers_output_else_the_dots() {
        let outputs = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1920, 1080)];
        // (pointer, the Dot's output, expected)
        let cases = [
            (Some((2000, 10)), Some(0), Some(1)),
            (Some((10, 10)), Some(1), Some(0)),
            (None, Some(1), Some(1)),
            (Some((-50, 5000)), Some(1), Some(1)),
            (None, None, None),
            (Some((5000, 5000)), None, None),
            (None, Some(7), None),
        ];
        for (pointer, dot, expected) in cases {
            assert_eq!(dialog_output(pointer, dot, &outputs), expected, "{pointer:?} {dot:?}");
        }
        assert_eq!(dialog_output(Some((1, 1)), Some(0), &[]), None);
    }
}
