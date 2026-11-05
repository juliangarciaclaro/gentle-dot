//! The windows and outputs the compositor reported (S2), and every decision made from them:
//! which app is in front, which app owns a point, the stable ids apps get (the protocol has no
//! pid), the app list, and which output a screenshot covers. Pure, so it is tested everywhere;
//! the Wayland client only fills these lists in.
//!
//! Every answer fails closed: when the lists do not single out one app, there is no app.

use super::apps::Catalog;
use super::pointer::Rect;
use crate::computer::blocklist::{AppIdentity, UNIDENTIFIED};
use crate::computer::coords::Point;
use crate::computer::AppInfo;
use std::time::{Duration, Instant};

/// Title of the glow overlay window: it covers a whole output, click-through, so it never owns
/// a point and is never the app in front.
pub const OVERLAY_TITLE: &str = "Gentle Dot Glow";

/// Synthetic ids start here, far above any real pid (`pid_max` tops out at 4 194 304), so an id
/// never equals Gentle Dot's own pid.
const PID_BASE: u32 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// The compositor's name for the output's global.
    pub id: u32,
    pub name: String,
    /// Logical position and size in the global layout.
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub app_id: String,
    pub title: String,
    pub activated: bool,
    pub minimized: bool,
    /// The window's geometry on each output it is visible on, relative to that output.
    pub frames: Vec<(u32, Rect)>,
}

pub(super) fn contains(rect: &Rect, at: Point) -> bool {
    at.x >= f64::from(rect.x)
        && at.y >= f64::from(rect.y)
        && at.x < f64::from(rect.x) + f64::from(rect.width)
        && at.y < f64::from(rect.y) + f64::from(rect.height)
}

/// The window's geometry in the global layout, one rectangle per output it is on.
pub fn global_rects(window: &WindowInfo, outputs: &[OutputInfo]) -> Vec<Rect> {
    window
        .frames
        .iter()
        .filter_map(|(output, frame)| {
            let output = outputs.iter().find(|o| o.id == *output)?;
            Some(Rect { x: output.rect.x + frame.x, y: output.rect.y + frame.y, width: frame.width, height: frame.height })
        })
        .collect()
}

/// The windows that can be a target: not Gentle Dot's own overlay.
pub fn without_overlay(windows: Vec<WindowInfo>) -> Vec<WindowInfo> {
    windows.into_iter().filter(|window| window.title != OVERLAY_TITLE).collect()
}

/// Whether `app_id` is Gentle Dot's own (the bundle identifier or the binary name; the class an
/// XWayland window reports may add spaces or change case).
pub fn is_own_app_id(app_id: &str) -> bool {
    let normalized: String = app_id.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    normalized == "gentledot" || normalized == "devgentlemangentledot"
}

/// The app in front: the one activated, not minimized window, or several of one app. `None`
/// when no window or windows of different apps claim to be active.
pub fn frontmost(windows: &[WindowInfo]) -> Option<&WindowInfo> {
    let mut active = windows.iter().filter(|w| w.activated && !w.minimized);
    let first = active.next()?;
    active.all(|other| other.app_id == first.app_id).then_some(first)
}

/// The window owning `at`: the only visible window whose geometry holds it; with several, the
/// activated one among them, or any of them when they are all one app's; else `None`.
pub fn owner_at<'a>(windows: &'a [WindowInfo], outputs: &[OutputInfo], at: Point) -> Option<&'a WindowInfo> {
    let under: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| !w.minimized && global_rects(w, outputs).iter().any(|rect| contains(rect, at)))
        .collect();
    match under.as_slice() {
        [] => None,
        [only] => Some(*only),
        [first, rest @ ..] if rest.iter().all(|w| w.app_id == first.app_id) => {
            under.iter().find(|w| w.activated).or(Some(first)).copied()
        }
        _ => {
            let mut active = under.iter().filter(|w| w.activated);
            let one = active.next()?;
            active.next().is_none().then_some(*one)
        }
    }
}

/// A stable id for an app_id (FNV-1a), in `PID_BASE..=i32::MAX`: the protocol has no pid, and
/// the helper tells apps apart by it.
pub fn synthetic_pid(app_id: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in app_id.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    (PID_BASE + hash % (PID_BASE - 1)) as i32
}

/// The app_id behind a synthetic id, among the running windows; `None` when no window or two
/// different app_ids map to it.
pub fn app_for_pid(windows: &[WindowInfo], pid: i32) -> Option<&str> {
    let mut found = windows.iter().map(|w| w.app_id.as_str()).filter(|app_id| synthetic_pid(app_id) == pid);
    let first = found.next()?;
    found.all(|other| other == first).then_some(first)
}

/// The window to bring forward for an app: its activated one, else a visible one, else any.
pub fn window_to_activate<'a>(windows: &'a [WindowInfo], app_id: &str) -> Option<&'a WindowInfo> {
    let own = || windows.iter().filter(|w| w.app_id == app_id);
    own().find(|w| w.activated).or_else(|| own().find(|w| !w.minimized)).or_else(|| own().next())
}

/// What the helper knows an app by: the display name from its `.desktop` entry (else the
/// app_id), the app_id as the bundle id, and the synthetic id. An app without an app_id is
/// `UNIDENTIFIED`, which the blocklist refuses; Gentle Dot's own windows carry its real pid.
pub fn identity(app_id: &str, catalog: &Catalog, own_pid: i32) -> AppIdentity {
    if app_id.is_empty() {
        return AppIdentity { pid: 0, bundle_id: None, name: UNIDENTIFIED.into() };
    }
    let pid = if is_own_app_id(app_id) { own_pid } else { synthetic_pid(app_id) };
    AppIdentity { pid, bundle_id: Some(app_id.into()), name: catalog.name_for(app_id).unwrap_or(app_id).into() }
}

/// The running apps: windows grouped by app_id in order of first appearance, `active` when one
/// of the app's windows is.
pub fn group_apps(windows: &[WindowInfo], catalog: &Catalog) -> Vec<AppInfo> {
    let mut apps: Vec<AppInfo> = Vec::new();
    for window in windows.iter().filter(|w| !w.app_id.is_empty()) {
        let active = window.activated && !window.minimized;
        match apps.iter_mut().find(|app| app.bundle_id.as_deref() == Some(window.app_id.as_str())) {
            Some(app) => app.active |= active,
            None => apps.push(AppInfo {
                name: catalog.name_for(&window.app_id).unwrap_or(&window.app_id).into(),
                bundle_id: Some(window.app_id.clone()),
                active,
            }),
        }
    }
    apps
}

/// The output a screenshot covers: the one under the last position the pointer was moved to,
/// else the one with the window in front, else the first.
pub fn output_for_capture<'a>(
    outputs: &'a [OutputInfo],
    pointer: Option<Point>,
    front: Option<&WindowInfo>,
) -> Option<&'a OutputInfo> {
    let under_pointer = pointer.and_then(|at| outputs.iter().find(|o| contains(&o.rect, at)));
    let with_front = || {
        let front = front?;
        let frame = front.frames.first()?;
        outputs.iter().find(|o| o.id == frame.0)
    };
    under_pointer.or_else(with_front).or_else(|| outputs.first())
}

/// The output containing `at`.
pub fn output_at(outputs: &[OutputInfo], at: Point) -> Option<&OutputInfo> {
    outputs.iter().find(|o| contains(&o.rect, at))
}

/// How long a batch of events may stay open before its state is published anyway, in case the
/// compositor never sends the closing `done`.
const BATCH_LIMIT: Duration = Duration::from_millis(250);

/// Decides when what the compositor reported may be published: a change opens a batch that the
/// compositor's `done` closes, and nothing is published while one is open, so a reader never
/// sees a half-applied update.
#[derive(Debug, Default)]
pub struct Settle {
    dirty: bool,
    open_since: Option<Instant>,
}

impl Settle {
    /// Something changed that stands alone.
    pub fn changed(&mut self) {
        self.dirty = true;
    }

    /// Something changed that belongs to a batch the compositor has yet to close.
    pub fn opened(&mut self, now: Instant) {
        self.dirty = true;
        self.open_since.get_or_insert(now);
    }

    /// The compositor closed the batch.
    pub fn closed(&mut self) {
        self.open_since = None;
    }

    /// Whether to publish now; true at most once per change.
    pub fn take(&mut self, now: Instant) -> bool {
        let open = self.open_since.is_some_and(|since| now.saturating_duration_since(since) < BATCH_LIMIT);
        if !self.dirty || open {
            return false;
        }
        self.dirty = false;
        self.open_since = None;
        true
    }

    /// Whether to publish regardless of an open batch: everything sent before is handled.
    pub fn take_settled(&mut self) -> bool {
        self.closed();
        self.take(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::linux::desktop_entry::DesktopEntry;

    const OWN_PID: i32 = 4242;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect { x, y, width, height }
    }

    fn outputs() -> Vec<OutputInfo> {
        vec![
            OutputInfo { id: 1, name: "HDMI-A-1".into(), rect: rect(0, 0, 1920, 1080) },
            OutputInfo { id: 2, name: "eDP-1".into(), rect: rect(1920, 0, 1920, 1080) },
        ]
    }

    fn window(id: u32, app_id: &str, frames: &[(u32, Rect)]) -> WindowInfo {
        WindowInfo {
            id,
            app_id: app_id.into(),
            title: format!("{app_id} window"),
            activated: false,
            minimized: false,
            frames: frames.to_vec(),
        }
    }

    fn active(mut w: WindowInfo) -> WindowInfo {
        w.activated = true;
        w
    }

    fn minimized(mut w: WindowInfo) -> WindowInfo {
        w.minimized = true;
        w
    }

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn catalog() -> Catalog {
        let entry = |name: &str| DesktopEntry {
            name: Some(name.into()),
            exec: Some("x".into()),
            is_application: true,
            ..DesktopEntry::default()
        };
        Catalog::from_entries(vec![("firefox.desktop".into(), entry("Firefox")), ("org.gnome.Nautilus.desktop".into(), entry("Files"))])
    }

    #[test]
    fn window_geometry_is_placed_on_its_output_in_the_global_layout() {
        let w = window(1, "a", &[(2, rect(100, 50, 800, 600)), (9, rect(0, 0, 10, 10))]);
        // The frame on an output that is gone is dropped.
        assert_eq!(global_rects(&w, &outputs()), [rect(2020, 50, 800, 600)]);
    }

    #[test]
    fn the_window_owning_a_point_is_the_only_one_there() {
        let windows = [
            window(1, "firefox", &[(1, rect(0, 0, 960, 1080))]),
            window(2, "nautilus", &[(1, rect(960, 0, 960, 1080))]),
        ];
        assert_eq!(owner_at(&windows, &outputs(), at(100.0, 100.0)).map(|w| w.id), Some(1));
        assert_eq!(owner_at(&windows, &outputs(), at(960.0, 100.0)).map(|w| w.id), Some(2));
        // Nothing there, or outside every window: fail closed.
        assert_eq!(owner_at(&windows, &outputs(), at(2500.0, 100.0)), None);
        assert_eq!(owner_at(&windows, &outputs(), at(-5.0, 100.0)), None);
    }

    #[test]
    fn overlapping_windows_resolve_to_the_activated_one_or_to_nobody() {
        let a = || window(1, "firefox", &[(1, rect(0, 0, 1000, 1000))]);
        let b = || window(2, "nautilus", &[(1, rect(500, 500, 1000, 500))]);
        let point = at(600.0, 600.0);
        assert_eq!(owner_at(&[a(), active(b())], &outputs(), point).map(|w| w.id), Some(2));
        assert_eq!(owner_at(&[active(a()), b()], &outputs(), point).map(|w| w.id), Some(1));
        assert_eq!(owner_at(&[a(), b()], &outputs(), point), None);
        // Two activated windows of different apps are no answer either.
        assert_eq!(owner_at(&[active(a()), active(b())], &outputs(), point), None);
        // Only one of them holds the point: no ambiguity.
        assert_eq!(owner_at(&[a(), b()], &outputs(), at(100.0, 100.0)).map(|w| w.id), Some(1));
    }

    #[test]
    fn several_windows_of_one_app_are_that_app_and_minimized_ones_never_own_a_point() {
        let one = window(1, "firefox", &[(1, rect(0, 0, 1000, 1000))]);
        let two = window(2, "firefox", &[(1, rect(0, 0, 1000, 1000))]);
        assert_eq!(owner_at(&[one.clone(), active(two.clone())], &outputs(), at(5.0, 5.0)).map(|w| w.id), Some(2));
        assert_eq!(owner_at(&[one.clone(), two.clone()], &outputs(), at(5.0, 5.0)).map(|w| w.app_id.as_str()), Some("firefox"));
        let other = window(3, "nautilus", &[(1, rect(0, 0, 1000, 1000))]);
        assert_eq!(owner_at(&[minimized(one), other], &outputs(), at(5.0, 5.0)).map(|w| w.id), Some(3));
        assert_eq!(owner_at(&[minimized(two)], &outputs(), at(5.0, 5.0)), None);
    }

    #[test]
    fn the_front_app_is_the_one_activated_visible_window() {
        let a = || window(1, "firefox", &[]);
        let b = || window(2, "nautilus", &[]);
        assert_eq!(frontmost(&[a(), active(b())]).map(|w| w.id), Some(2));
        assert_eq!(frontmost(&[a(), b()]), None);
        assert_eq!(frontmost(&[minimized(active(a())), b()]), None);
        assert_eq!(frontmost(&[active(a()), active(b())]), None);
        assert_eq!(frontmost(&[active(a()), active(window(3, "firefox", &[]))]).map(|w| w.id), Some(1));
        assert_eq!(frontmost(&[]), None);
    }

    #[test]
    fn the_overlay_is_never_a_candidate() {
        let mut overlay = window(9, "gentle-dot", &[(1, rect(0, 0, 1920, 1080))]);
        overlay.title = OVERLAY_TITLE.into();
        let kept = without_overlay(vec![overlay, window(1, "firefox", &[])]);
        assert_eq!(kept.iter().map(|w| w.id).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn synthetic_ids_are_stable_distinct_and_never_a_real_pid() {
        assert_eq!(synthetic_pid("firefox"), synthetic_pid("firefox"));
        assert_ne!(synthetic_pid("firefox"), synthetic_pid("org.gnome.Nautilus"));
        for app_id in ["firefox", "", "org.gnome.Nautilus", "ünïcode", "x"] {
            let pid = synthetic_pid(app_id);
            assert!(((1 << 30)..i32::MAX).contains(&pid), "{app_id}: {pid}");
        }
    }

    #[test]
    fn an_id_maps_back_to_its_app_only_when_one_app_has_it() {
        let windows = [window(1, "firefox", &[]), window(2, "nautilus", &[]), window(3, "firefox", &[])];
        assert_eq!(app_for_pid(&windows, synthetic_pid("firefox")), Some("firefox"));
        assert_eq!(app_for_pid(&windows, synthetic_pid("gone")), None);
        assert_eq!(app_for_pid(&windows, OWN_PID), None);
        assert_eq!(app_for_pid(&[], synthetic_pid("firefox")), None);
    }

    #[test]
    fn activating_an_app_picks_its_active_then_visible_then_any_window() {
        let windows = [
            minimized(window(1, "firefox", &[])),
            window(2, "firefox", &[]),
            active(window(3, "firefox", &[])),
            window(4, "nautilus", &[]),
        ];
        assert_eq!(window_to_activate(&windows, "firefox").map(|w| w.id), Some(3));
        assert_eq!(window_to_activate(&windows[..2], "firefox").map(|w| w.id), Some(2));
        assert_eq!(window_to_activate(&windows[..1], "firefox").map(|w| w.id), Some(1));
        assert_eq!(window_to_activate(&windows, "gone").map(|w| w.id), None);
    }

    #[test]
    fn identities_use_the_entry_name_the_app_id_and_the_synthetic_id() {
        let firefox = identity("firefox", &catalog(), OWN_PID);
        assert_eq!(firefox.name, "Firefox");
        assert_eq!(firefox.bundle_id.as_deref(), Some("firefox"));
        assert_eq!(firefox.pid, synthetic_pid("firefox"));
        let unlisted = identity("some.tool", &catalog(), OWN_PID);
        assert_eq!((unlisted.name.as_str(), unlisted.bundle_id.as_deref()), ("some.tool", Some("some.tool")));
        // Gentle Dot's own windows carry its real pid; no app_id means an app nobody identifies.
        for own in ["gentle-dot", "Gentle Dot", "dev.gentleman.gentle-dot"] {
            assert_eq!(identity(own, &catalog(), OWN_PID).pid, OWN_PID, "{own}");
        }
        let nobody = identity("", &catalog(), OWN_PID);
        assert_eq!((nobody.pid, nobody.bundle_id, nobody.name.as_str()), (0, None, UNIDENTIFIED));
    }

    #[test]
    fn apps_are_grouped_by_app_id_and_named_from_entries() {
        let windows = [
            window(1, "firefox", &[]),
            window(2, "org.gnome.Nautilus", &[]),
            active(window(3, "firefox", &[])),
            window(4, "", &[]),
            minimized(active(window(5, "unlisted", &[]))),
        ];
        let apps = group_apps(&windows, &catalog());
        let summary: Vec<_> = apps.iter().map(|a| (a.name.as_str(), a.bundle_id.as_deref(), a.active)).collect();
        assert_eq!(
            summary,
            [
                ("Firefox", Some("firefox"), true),
                ("Files", Some("org.gnome.Nautilus"), false),
                ("unlisted", Some("unlisted"), false),
            ]
        );
    }

    #[test]
    fn a_screenshot_covers_the_pointer_output_then_the_front_window_output_then_the_first() {
        let outputs = outputs();
        let front = active(window(1, "a", &[(2, rect(0, 0, 100, 100))]));
        let id = |o: Option<&OutputInfo>| o.map(|o| o.id);
        assert_eq!(id(output_for_capture(&outputs, Some(at(100.0, 100.0)), Some(&front))), Some(1));
        assert_eq!(id(output_for_capture(&outputs, Some(at(2000.0, 10.0)), None)), Some(2));
        // A pointer position on no output (an output went away) falls back.
        assert_eq!(id(output_for_capture(&outputs, Some(at(9000.0, 0.0)), Some(&front))), Some(2));
        assert_eq!(id(output_for_capture(&outputs, None, Some(&front))), Some(2));
        assert_eq!(id(output_for_capture(&outputs, None, None)), Some(1));
        assert_eq!(id(output_for_capture(&[], None, None)), None);
        assert_eq!(id(output_at(&outputs, at(1919.0, 5.0))), Some(1));
        assert_eq!(id(output_at(&outputs, at(1920.0, 5.0))), Some(2));
    }

    #[test]
    fn nothing_is_published_while_a_batch_is_open() {
        let start = Instant::now();
        let mut settle = Settle::default();
        assert!(!settle.take(start));
        settle.opened(start);
        settle.opened(start);
        assert!(!settle.take(start));
        settle.closed();
        assert!(settle.take(start));
        assert!(!settle.take(start));
        // A change that stands alone is published at once.
        settle.changed();
        assert!(settle.take(start));
        // A batch the compositor never closes is published after the limit.
        settle.opened(start);
        assert!(!settle.take(start + BATCH_LIMIT / 2));
        assert!(settle.take(start + BATCH_LIMIT));
        // A forced publish ignores an open batch.
        settle.opened(start);
        assert!(settle.take_settled());
    }
}
