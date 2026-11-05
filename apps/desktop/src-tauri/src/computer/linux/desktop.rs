//! `LinuxDesktop`: the `Desktop` for a Linux session (S2). On Wayland (COSMIC) the windows,
//! activation, and keyboard come from our own Wayland client, screenshots from the portal; the
//! pointer is a seam (`Pointer`) the opt-in uinput device fills in. On X11 everything comes from
//! the X server (EWMH, XTest, `GetImage`) and the pointer needs no opt-in. Every answer about
//! which app is in front or under a point fails closed when the session does not say.

use super::apps::Catalog;
use super::atspi::Atspi;
use super::portal::Portal;
use super::session::{self, Session};
use super::wayland::Client;
use super::x11_client::X11;
use super::{atspi, desktop_entry, shot, toplevels, typing, x11};
use crate::computer::blocklist::AppIdentity;
use crate::computer::coords::Point;
use crate::computer::glow::Frame;
use crate::computer::risk::{AxElement, Focus};
use crate::computer::{AppInfo, Capture, Desktop, InputEvent, Permissions, ScreenshotsUnsupported};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Pointer actions need the opt-in uinput device (full desktop control).
pub const POINTER_OFF: &str = "Moving and clicking the pointer needs full desktop control. Ask the user to turn it on in \
Connectors → Computer; keyboard input and screenshots work without it.";
const NO_SESSION: &str = "Computer control needs a Wayland or X11 session.";
const NO_WINDOWS: &str = "This desktop does not tell Gentle Dot which window is in front, so it cannot list apps. \
Computer control supports COSMIC and X11 window managers that follow EWMH.";
const NO_XTEST: &str = "The X server has no XTest extension, so Gentle Dot cannot send input.";
/// How long the catalog of `.desktop` entries is reused.
const CATALOG_REUSE: Duration = Duration::from_secs(60);
/// The compositor needs a frame or two to take the glow overlay off the screen.
pub const OVERLAY_GONE: Duration = Duration::from_millis(150);

/// Why full desktop control cannot be turned on: the account cannot write `/dev/uinput`.
pub const UDEV_HELP: &str = "Full desktop control needs write access to /dev/uinput, which this account does not have. \
Add a udev rule such as KERNEL==\"uinput\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\" to \
/etc/udev/rules.d/60-gentle-dot-uinput.rules, run `sudo udevadm control --reload && sudo udevadm trigger`, \
then turn it on again.";
const NOT_CONFIRMED: &str = "Full desktop control stays off.";

/// The pointer, once the user turned on full desktop control (the uinput device).
pub trait Pointer: Send + Sync {
    /// Posts a `Move`, `Click`, `Drag`, or `Scroll`; `outputs` is the layout it moves over.
    fn post(&self, event: &InputEvent, outputs: &[super::pointer::Rect]) -> Result<(), String>;
    /// Where the pointer is, when the device knows.
    fn position(&self) -> Option<Point>;
}

/// Takes Gentle Dot's glow overlay off the screen around a capture: Linux cannot leave a window
/// out of a screenshot, so the overlay is hidden while it is taken.
pub trait CaptureGuard: Send + Sync {
    /// Hides the overlay and returns once it is gone from the screen.
    fn hide(&self) -> Result<(), String>;
    /// Shows it again if it was showing.
    fn restore(&self);
}

/// What the session is reached through.
enum Backend {
    Wayland(Client),
    X11(Box<X11>),
}

impl Backend {
    /// Outputs and windows now; the X11 windows are ordered bottom to top.
    fn snapshot(&self) -> Result<(Vec<toplevels::OutputInfo>, Vec<toplevels::WindowInfo>), String> {
        match self {
            Backend::Wayland(client) => client.snapshot(),
            Backend::X11(x11) => x11.snapshot(),
        }
    }

    fn can_list_windows(&self) -> bool {
        match self {
            Backend::Wayland(client) => client.can_list_windows(),
            Backend::X11(x11) => x11.can_list_windows(),
        }
    }

    fn activate(&self, window: u32) -> Result<(), String> {
        match self {
            Backend::Wayland(client) => client.activate(window),
            Backend::X11(x11) => x11.activate(window),
        }
    }
}

/// The backend for a session: only the one the session names is started.
fn select<T>(
    session: Session,
    wayland: impl FnOnce() -> Result<T, String>,
    x11: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match session {
        Session::Wayland => wayland(),
        Session::X11 => x11(),
        Session::None => Err(NO_SESSION.to_string()),
    }
}

pub struct LinuxDesktop {
    /// `Err` is why there is no backend (no session, or the compositor or server refused us).
    backend: Result<Backend, String>,
    portal: Portal,
    catalog: Mutex<(Instant, Arc<Catalog>)>,
    /// Present only while full desktop control is on; nothing moves the pointer without it.
    pointer: Mutex<Option<Arc<dyn Pointer>>>,
    /// Held while full desktop control is being turned on, so two calls never open two devices.
    enabling: Mutex<()>,
    last_pointer: Mutex<Option<Point>>,
    guard: Mutex<Option<Arc<dyn CaptureGuard>>>,
    /// Accessibility (AT-SPI) for the risk checks.
    atspi: Atspi,
    own_pid: i32,
}

fn load_catalog() -> Catalog {
    Catalog::load(&|key| std::env::var(key).ok())
}

impl LinuxDesktop {
    /// Connects for the current session. Never fails: a session that is not supported yields a
    /// desktop that reports itself unavailable.
    pub fn start(pointer: Option<Arc<dyn Pointer>>) -> Self {
        let backend = select(
            session::detect(|key| std::env::var(key).ok()),
            || Client::connect().map(Backend::Wayland),
            || X11::connect().map(|x11| Backend::X11(Box::new(x11))),
        );
        if let Err(error) = &backend {
            eprintln!("gentle-dot: computer control is limited: {error}");
        }
        Self::with_backend(backend, pointer)
    }

    fn with_backend(backend: Result<Backend, String>, pointer: Option<Arc<dyn Pointer>>) -> Self {
        LinuxDesktop {
            backend,
            portal: Portal::default(),
            catalog: Mutex::new((Instant::now(), Arc::new(load_catalog()))),
            pointer: Mutex::new(pointer),
            enabling: Mutex::new(()),
            last_pointer: Mutex::new(None),
            guard: Mutex::new(None),
            atspi: Atspi::default(),
            own_pid: std::process::id() as i32,
        }
    }

    fn pointer(&self) -> Option<Arc<dyn Pointer>> {
        self.pointer.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Turns full desktop control on after the user agreed, or explains why not. Nothing is
    /// opened, saved, or enabled unless every step before it passed (fail closed).
    pub fn enable_full_control(&self, steps: &FullControl) -> Result<(), String> {
        // A second call waits for the first, then finds the pointer on and opens nothing.
        let _single_flight = self.enabling.lock().unwrap_or_else(|p| p.into_inner());
        if self.pointer().is_some() {
            return Ok(());
        }
        if !(steps.writable)() {
            return Err(UDEV_HELP.into());
        }
        if !(steps.confirm)() {
            return Err(NOT_CONFIRMED.into());
        }
        let pointer = (steps.open)()?;
        // Saved before it is used: a choice that cannot be remembered is not made.
        (steps.save)(true).map_err(|e| format!("cannot save the choice: {e}"))?;
        *self.pointer.lock().unwrap_or_else(|p| p.into_inner()) = Some(pointer);
        Ok(())
    }

    pub fn set_capture_guard(&self, guard: Arc<dyn CaptureGuard>) {
        *self.guard.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
    }

    /// Records where the pointer was moved to, which picks the output a screenshot covers.
    pub fn note_pointer(&self, at: Point) {
        *self.last_pointer.lock().unwrap_or_else(|p| p.into_inner()) = Some(at);
    }

    fn backend(&self) -> Result<&Backend, String> {
        self.backend.as_ref().map_err(Clone::clone)
    }

    /// The X server, when the session is X11.
    fn x11(&self) -> Option<&X11> {
        match &self.backend {
            Ok(Backend::X11(x11)) => Some(x11),
            _ => None,
        }
    }

    /// The window owning `at`: on X11 the topmost one (the window manager stacks them), else the
    /// only or the activated one.
    fn owner_at<'a>(
        &self,
        windows: &'a [toplevels::WindowInfo],
        outputs: &[toplevels::OutputInfo],
        at: Point,
    ) -> Option<&'a toplevels::WindowInfo> {
        if self.x11().is_some() {
            x11::topmost_at(windows, outputs, at)
        } else {
            toplevels::owner_at(windows, outputs, at)
        }
    }

    fn catalog(&self, refresh: bool) -> Arc<Catalog> {
        let mut slot = self.catalog.lock().unwrap_or_else(|p| p.into_inner());
        if refresh || slot.0.elapsed() > CATALOG_REUSE {
            *slot = (Instant::now(), Arc::new(load_catalog()));
        }
        slot.1.clone()
    }

    /// Outputs and the windows that can be a target, as the compositor has them now.
    fn snapshot(&self) -> Result<(Vec<toplevels::OutputInfo>, Vec<toplevels::WindowInfo>), String> {
        let (outputs, windows) = self.backend()?.snapshot()?;
        Ok((outputs, toplevels::without_overlay(windows)))
    }

    fn pointer_position(&self) -> Option<Point> {
        // The server knows where the pointer is, whoever moved it.
        if let Some(at) = self.x11().and_then(X11::pointer) {
            return Some(at);
        }
        self.pointer()
            .and_then(|pointer| pointer.position())
            .or(*self.last_pointer.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn identity(&self, window: &toplevels::WindowInfo) -> AppIdentity {
        toplevels::identity(&window.app_id, &self.catalog(false), self.own_pid)
    }

    /// The frame of the output a screenshot (and the overlay) covers.
    pub fn capture_frame(&self) -> Option<Frame> {
        let (outputs, windows) = self.snapshot().ok()?;
        let output = toplevels::output_for_capture(&outputs, self.pointer_position(), toplevels::frontmost(&windows))?;
        Some(frame_of(&output.rect))
    }

    /// The frame of the output holding `at`.
    pub fn display_frame_at(&self, at: Point) -> Option<Frame> {
        let (outputs, _) = self.snapshot().ok()?;
        toplevels::output_at(&outputs, at).map(|output| frame_of(&output.rect))
    }

    /// The frame of the window in front, for keyboard marks.
    pub fn focused_frame(&self) -> Option<Frame> {
        let (outputs, windows) = self.snapshot().ok()?;
        let front = toplevels::frontmost(&windows)?;
        toplevels::global_rects(front, &outputs).first().map(frame_of)
    }

    fn send(&self, batches: Vec<typing::Batch>) -> Result<(), String> {
        let Backend::Wayland(client) = self.backend()? else { return Err("this session types through X11".into()) };
        for batch in batches {
            client.keys(Some(batch.keymap), batch.steps)?;
        }
        Ok(())
    }

    /// Plays key steps planned from the server's current keyboard map.
    fn send_x11(&self, x11: &X11, plan: impl FnOnce(&x11::Mapping) -> Result<Vec<x11::KeyAction>, String>) -> Result<(), String> {
        x11.keys(&plan(&x11.mapping()?)?)
    }

    /// X11: XTest keys and pointer, no opt-in.
    fn post_x11(&self, x11: &X11, event: &InputEvent) -> Result<(), String> {
        if !x11.has_input() {
            return Err(NO_XTEST.into());
        }
        match event {
            InputEvent::Key(combo) => self.send_x11(x11, |map| x11::combo_actions(combo, map)),
            InputEvent::Text(text) => self.send_x11(x11, |map| x11::text_actions(text, map)),
            InputEvent::Move(_) | InputEvent::Click { .. } | InputEvent::Drag { .. } | InputEvent::Scroll { .. } => {
                let (outputs, _) = self.snapshot()?;
                let layout: Vec<_> = outputs.iter().map(|output| output.rect).collect();
                x11.play_pointer(&x11::pointer_actions(event, &layout)?)?;
                match event {
                    InputEvent::Move(at) | InputEvent::Click { at, .. } | InputEvent::Scroll { at, .. } => self.note_pointer(*at),
                    InputEvent::Drag { to, .. } => self.note_pointer(*to),
                    _ => {}
                }
                Ok(())
            }
        }
    }

    fn launch(&self, id: &str, entry: &desktop_entry::DesktopEntry) -> Result<(), String> {
        let argv = entry.exec.as_deref().map(desktop_entry::exec_argv).transpose()?.unwrap_or_default();
        let mut direct = argv.split_first().map(|(program, args)| {
            let mut command = std::process::Command::new(program);
            command.args(args);
            command
        });
        let started = match direct.as_mut() {
            Some(command) => detached(command),
            None => Err("the app has no command to run".into()),
        };
        started.or_else(|error| {
            // A launcher that knows the entry (it handles `Exec` quirks, D-Bus activation).
            let mut command = std::process::Command::new("gtk-launch");
            command.arg(id.strip_suffix(".desktop").unwrap_or(id));
            detached(&mut command).map_err(|_| error)
        })
    }
}

/// Starts a command with no terminal, in its own process group, and reaps it in the background.
/// A GTK app started from here must not inherit the `GDK_BACKEND=x11` Gentle Dot set for itself.
fn detached(command: &mut std::process::Command) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    if std::env::var("GDK_BACKEND").is_ok_and(|backend| backend == "x11") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        command.env_remove("GDK_BACKEND");
    }
    let mut child = command.spawn().map_err(|e| format!("cannot start the app: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn frame_of(rect: &super::pointer::Rect) -> Frame {
    Frame { x: f64::from(rect.x), y: f64::from(rect.y), width: f64::from(rect.width), height: f64::from(rect.height) }
}

impl Desktop for LinuxDesktop {
    fn permissions(&self) -> Permissions {
        // X11: input and screenshots go through the server, with nothing to turn on.
        if let Some(x11) = self.x11() {
            return Permissions {
                accessibility: x11.has_input(),
                screen_recording: true,
                screenshots_supported: true,
                screenshots_unsupported: None,
            };
        }
        let screenshots = self.backend.is_ok() && self.portal.available();
        let keyboard = matches!(&self.backend, Ok(Backend::Wayland(client)) if client.has_keyboard());
        Permissions {
            accessibility: input_reachable(keyboard, self.pointer().is_some()),
            screen_recording: screenshots,
            screenshots_supported: screenshots,
            screenshots_unsupported: (!screenshots).then_some(ScreenshotsUnsupported::TooOld),
        }
    }

    fn capture(&self) -> Result<Capture, String> {
        let (outputs, windows) = self.snapshot()?;
        let output = toplevels::output_for_capture(&outputs, self.pointer_position(), toplevels::frontmost(&windows))
            .ok_or("no screen is known")?;
        // A session starts with a look at the screen: the toolkits get time to expose themselves.
        self.atspi.enable();
        let guard = self.guard.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let hide = || guard.as_ref().map_or(Ok(()), |guard| guard.hide());
        let captured = match self.x11() {
            Some(x11) => {
                let pixels = super::super::capture_order::capture(hide, || x11.grab(output.rect));
                pixels.and_then(|(format, data)| x11::capture_of_pixels(&data, format, output))
            }
            None => {
                let png = super::super::capture_order::capture(hide, || self.portal.screenshot());
                png.and_then(|png| shot::capture_of(&png, output, &outputs))
            }
        };
        if let Some(guard) = &guard {
            guard.restore();
        }
        captured
    }

    fn post(&self, event: &InputEvent) -> Result<(), String> {
        if let Some(x11) = self.x11() {
            return self.post_x11(x11, event);
        }
        match event {
            InputEvent::Key(combo) => self.send(vec![typing::combo_batch(combo)?]),
            InputEvent::Text(text) => self.send(typing::text_batches(text)?),
            InputEvent::Move(_) | InputEvent::Click { .. } | InputEvent::Drag { .. } | InputEvent::Scroll { .. } => {
                let pointer = self.pointer().ok_or(POINTER_OFF)?;
                let (outputs, _) = self.snapshot()?;
                let layout: Vec<_> = outputs.iter().map(|output| output.rect).collect();
                pointer.post(event, &layout)?;
                match event {
                    InputEvent::Move(at) | InputEvent::Click { at, .. } | InputEvent::Scroll { at, .. } => self.note_pointer(*at),
                    InputEvent::Drag { to, .. } => self.note_pointer(*to),
                    _ => {}
                }
                Ok(())
            }
        }
    }

    fn list_apps(&self) -> Result<Vec<AppInfo>, String> {
        if !self.backend()?.can_list_windows() {
            return Err(NO_WINDOWS.into());
        }
        let (_, windows) = self.snapshot()?;
        Ok(toplevels::group_apps(&windows, &self.catalog(false)))
    }

    fn open_app(&self, name: &str) -> Result<(), String> {
        let mut catalog = self.catalog(false);
        if catalog.find(name).is_none() {
            // An app installed since the catalog was read.
            catalog = self.catalog(true);
        }
        let (id, entry) = catalog.find(name).ok_or_else(|| format!("No app named `{name}` is installed."))?;
        // Already running: bring it forward instead of starting a second copy.
        if let Ok((_, windows)) = self.snapshot() {
            let running = windows.iter().find(|w| desktop_entry::matches_app_id(id, entry, &w.app_id));
            if let Some(window) = running.and_then(|w| toplevels::window_to_activate(&windows, &w.app_id)) {
                return self.backend()?.activate(window.id);
            }
        }
        self.launch(id, entry)
    }

    fn element_at(&self, at: Point) -> Vec<AxElement> {
        let Ok((outputs, windows)) = self.snapshot() else { return Vec::new() };
        let Some(owner) = self.owner_at(&windows, &outputs, at) else { return Vec::new() };
        let Some(point) = atspi::window_point((at.x, at.y), &toplevels::global_rects(owner, &outputs)) else {
            return Vec::new();
        };
        self.atspi.enable();
        self.atspi.element_at(&owner.title, owner.activated, point)
    }

    fn focused_element(&self) -> Option<Focus> {
        let (_, windows) = self.snapshot().ok()?;
        let front = toplevels::frontmost(&windows)?;
        self.atspi.enable();
        self.atspi.focused(&front.title)
    }

    fn frontmost_app(&self) -> Option<AppIdentity> {
        let (_, windows) = self.snapshot().ok()?;
        toplevels::frontmost(&windows).map(|window| self.identity(window))
    }

    fn focused_app(&self) -> Option<AppIdentity> {
        self.frontmost_app()
    }

    fn window_owner_at(&self, at: Point) -> Option<AppIdentity> {
        let (outputs, windows) = self.snapshot().ok()?;
        self.owner_at(&windows, &outputs, at).map(|window| self.identity(window))
    }

    fn activate(&self, pid: i32) {
        let Ok((_, windows)) = self.snapshot() else { return };
        let target = toplevels::app_for_pid(&windows, pid).and_then(|app_id| toplevels::window_to_activate(&windows, app_id));
        if let (Some(window), Ok(backend)) = (target, self.backend()) {
            if let Err(error) = backend.activate(window.id) {
                eprintln!("gentle-dot: cannot bring the app forward: {error}");
            }
        }
    }
}

/// Input is reachable when both the keyboard and a pointer are (the macOS Accessibility
/// counterpart).
fn input_reachable(keyboard: bool, pointer: bool) -> bool {
    keyboard && pointer
}

/// The steps of turning on full desktop control, apart from the desktop so the order is tested.
pub struct FullControl<'a> {
    /// `/dev/uinput` can be written (checked without opening it).
    pub writable: &'a dyn Fn() -> bool,
    /// The native confirmation, refusing by default.
    pub confirm: &'a dyn Fn() -> bool,
    /// Opens the device.
    pub open: &'a dyn Fn() -> Result<Arc<dyn Pointer>, String>,
    /// Saves the choice in `desktop.json`.
    pub save: &'a dyn Fn(bool) -> std::io::Result<()>,
}

/// The pointer at launch: opened only when the saved choice is on, and a device that will not
/// open leaves the pointer off.
pub fn startup_pointer(
    saved: bool,
    open: impl FnOnce() -> Result<Arc<dyn Pointer>, String>,
) -> Option<Arc<dyn Pointer>> {
    if !saved {
        return None;
    }
    open().map_err(|error| eprintln!("gentle-dot: full desktop control is off: {error}")).ok()
}

/// `computer_request_permission` on Linux: there is no screen-recording permission to ask for
/// (the portal decides per request); accessibility is full desktop control, turned on after the
/// native confirmation.
pub fn request_permission(
    kind: crate::computer::PermissionKind,
    desktop: &LinuxDesktop,
    steps: &FullControl,
) -> Result<(), String> {
    match kind {
        crate::computer::PermissionKind::ScreenRecording => Ok(()),
        // X11 sends input through XTest: nothing to turn on, nothing to confirm.
        crate::computer::PermissionKind::Accessibility => match desktop.x11() {
            Some(x11) if x11.has_input() => Ok(()),
            Some(_) => Err(NO_XTEST.into()),
            None => desktop.enable_full_control(steps),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::linux::pointer::Rect;
    use crate::computer::MouseButton;
    use std::cell::{Cell, RefCell};

    struct Fake;

    impl Pointer for Fake {
        fn post(&self, _event: &InputEvent, _outputs: &[Rect]) -> Result<(), String> {
            Ok(())
        }

        fn position(&self) -> Option<Point> {
            None
        }
    }

    fn offline(pointer: Option<Arc<dyn Pointer>>) -> LinuxDesktop {
        LinuxDesktop::with_backend(Err("no compositor".into()), pointer)
    }

    /// Runs `enable_full_control` with the given answers and returns the result and the calls made.
    fn enable(desktop: &LinuxDesktop, writable: bool, agree: bool, opens: bool, saves: bool) -> (Result<(), String>, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let result = desktop.enable_full_control(&FullControl {
            writable: &|| {
                calls.borrow_mut().push("writable");
                writable
            },
            confirm: &|| {
                calls.borrow_mut().push("confirm");
                agree
            },
            open: &|| {
                calls.borrow_mut().push("open");
                if opens { Ok(Arc::new(Fake)) } else { Err("busy".into()) }
            },
            save: &|on| {
                calls.borrow_mut().push(if on { "save" } else { "unsave" });
                if saves { Ok(()) } else { Err(std::io::Error::other("read-only")) }
            },
        });
        (result, calls.into_inner())
    }

    #[test]
    fn the_session_picks_one_backend_and_wayland_still_wins() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
        };
        let started = |pairs| {
            let calls = RefCell::new(Vec::new());
            let result = select(
                session::detect(env(pairs)),
                || {
                    calls.borrow_mut().push("wayland");
                    Ok(1)
                },
                || {
                    calls.borrow_mut().push("x11");
                    Ok(2)
                },
            );
            (result, calls.into_inner())
        };
        // XWayland sets DISPLAY inside a Wayland session: still the Wayland client, never X11.
        assert_eq!(started(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":1")]), (Ok(1), vec!["wayland"]));
        assert_eq!(started(&[("DISPLAY", ":0")]), (Ok(2), vec!["x11"]));
        assert_eq!(started(&[]), (Err(NO_SESSION.to_string()), vec![]));
    }

    #[test]
    fn pointer_actions_are_refused_while_full_control_is_off() {
        let desktop = offline(None);
        let at = Point { x: 1.0, y: 1.0 };
        for event in [
            InputEvent::Move(at),
            InputEvent::Click { at, button: MouseButton::Left, count: 1 },
            InputEvent::Drag { from: at, to: at },
            InputEvent::Scroll { at, dx: 0, dy: 5 },
        ] {
            assert_eq!(desktop.post(&event), Err(POINTER_OFF.to_string()), "{event:?}");
        }
    }

    #[test]
    fn accessibility_needs_the_keyboard_and_a_pointer() {
        for (keyboard, pointer, expected) in [(true, true, true), (true, false, false), (false, true, false), (false, false, false)] {
            assert_eq!(input_reachable(keyboard, pointer), expected, "keyboard {keyboard}, pointer {pointer}");
        }
        // Without a compositor there is no keyboard, whatever the pointer.
        assert!(!offline(Some(Arc::new(Fake))).permissions().accessibility);
        assert!(!offline(None).permissions().accessibility);
    }

    #[test]
    fn turning_on_asks_then_opens_then_saves_then_enables() {
        let desktop = offline(None);
        let (result, calls) = enable(&desktop, true, true, true, true);
        assert_eq!(result, Ok(()));
        assert_eq!(calls, ["writable", "confirm", "open", "save"]);
        assert!(desktop.pointer().is_some());
        // Already on: nothing is asked or opened again.
        let (result, calls) = enable(&desktop, true, true, true, true);
        assert_eq!((result, calls.len()), (Ok(()), 0));
    }

    #[test]
    fn two_concurrent_turn_ons_open_one_device() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let desktop = offline(None);
        let (opens, saves) = (AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let result = desktop.enable_full_control(&FullControl {
                        writable: &|| true,
                        // Long enough for the other call to reach its own check.
                        confirm: &|| {
                            std::thread::sleep(Duration::from_millis(100));
                            true
                        },
                        open: &|| {
                            opens.fetch_add(1, Ordering::SeqCst);
                            Ok(Arc::new(Fake))
                        },
                        save: &|_| {
                            saves.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    });
                    assert_eq!(result, Ok(()));
                });
            }
        });
        assert_eq!((opens.into_inner(), saves.into_inner()), (1, 1));
        assert!(desktop.pointer().is_some());
    }

    #[test]
    fn turning_on_fails_closed_at_every_step() {
        // (writable, agree, opens, saves) and the last call made before it stopped.
        for ((writable, agree, opens, saves), last) in [
            ((false, true, true, true), "writable"),
            ((true, false, true, true), "confirm"),
            ((true, true, false, true), "open"),
            ((true, true, true, false), "save"),
        ] {
            let desktop = offline(None);
            let (result, calls) = enable(&desktop, writable, agree, opens, saves);
            assert!(result.is_err(), "{last}");
            assert_eq!(calls.last(), Some(&last));
            assert!(desktop.pointer().is_none(), "{last} left the pointer on");
        }
        // The udev explanation is what the user reads when the device is not writable.
        let (result, calls) = enable(&offline(None), false, true, true, true);
        assert_eq!(result, Err(UDEV_HELP.to_string()));
        assert_eq!(calls, ["writable"]);
        assert!(UDEV_HELP.contains("TAG+=\"uaccess\""));
    }

    #[test]
    fn the_device_is_opened_at_launch_only_when_the_saved_choice_is_on() {
        let opened = Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Ok(Arc::new(Fake) as Arc<dyn Pointer>)
        };
        assert!(startup_pointer(false, open).is_none());
        assert_eq!(opened.get(), 0);
        assert!(startup_pointer(true, open).is_some());
        assert_eq!(opened.get(), 1);
        assert!(startup_pointer(true, || Err("denied".into())).is_none());
    }

    #[test]
    fn only_accessibility_asks_for_full_control() {
        let desktop = offline(None);
        let steps = FullControl {
            writable: &|| panic!("screen recording asks nothing"),
            confirm: &|| panic!("screen recording asks nothing"),
            open: &|| panic!("screen recording opens nothing"),
            save: &|_| panic!("screen recording saves nothing"),
        };
        assert_eq!(request_permission(crate::computer::PermissionKind::ScreenRecording, &desktop, &steps), Ok(()));
    }
}
