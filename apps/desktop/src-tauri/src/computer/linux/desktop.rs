//! `LinuxDesktop`: the `Desktop` for a Linux session (S2). On Wayland (COSMIC) the windows,
//! activation, and keyboard come from our own Wayland client, screenshots from the portal; the
//! pointer is a seam (`Pointer`) the opt-in uinput device fills in. Every answer about which app
//! is in front or under a point fails closed when the compositor does not say.

use super::apps::Catalog;
use super::portal::Portal;
use super::session::{self, Session};
use super::wayland::Client;
use super::{desktop_entry, shot, toplevels, typing};
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
const NO_SESSION: &str = "Computer control needs a Wayland session; no other session is supported yet.";
const NO_WINDOWS: &str = "This desktop does not tell Gentle Dot which window is in front, so it cannot list apps. \
Computer control supports COSMIC.";
/// How long the catalog of `.desktop` entries is reused.
const CATALOG_REUSE: Duration = Duration::from_secs(60);
/// The compositor needs a frame or two to take the glow overlay off the screen.
pub const OVERLAY_GONE: Duration = Duration::from_millis(150);

/// The pointer, once the user turned on full desktop control (T3 provides it).
pub trait Pointer: Send + Sync {
    /// Posts a `Move`, `Click`, `Drag`, or `Scroll`.
    fn post(&self, event: &InputEvent) -> Result<(), String>;
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

pub struct LinuxDesktop {
    /// `Err` is why there is no backend (not Wayland, or the compositor refused us).
    client: Result<Client, String>,
    portal: Portal,
    catalog: Mutex<(Instant, Arc<Catalog>)>,
    pointer: Option<Box<dyn Pointer>>,
    last_pointer: Mutex<Option<Point>>,
    guard: Mutex<Option<Arc<dyn CaptureGuard>>>,
    own_pid: i32,
}

fn load_catalog() -> Catalog {
    Catalog::load(&|key| std::env::var(key).ok())
}

impl LinuxDesktop {
    /// Connects for the current session. Never fails: a session that is not supported yields a
    /// desktop that reports itself unavailable.
    pub fn start(pointer: Option<Box<dyn Pointer>>) -> Self {
        let client = match session::detect(|key| std::env::var(key).ok()) {
            Session::Wayland => Client::connect(),
            Session::X11 | Session::None => Err(NO_SESSION.to_string()),
        };
        if let Err(error) = &client {
            eprintln!("gentle-dot: computer control is limited: {error}");
        }
        LinuxDesktop {
            client,
            portal: Portal::default(),
            catalog: Mutex::new((Instant::now(), Arc::new(load_catalog()))),
            pointer,
            last_pointer: Mutex::new(None),
            guard: Mutex::new(None),
            own_pid: std::process::id() as i32,
        }
    }

    pub fn set_capture_guard(&self, guard: Arc<dyn CaptureGuard>) {
        *self.guard.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
    }

    /// Records where the pointer was moved to, which picks the output a screenshot covers.
    pub fn note_pointer(&self, at: Point) {
        *self.last_pointer.lock().unwrap_or_else(|p| p.into_inner()) = Some(at);
    }

    fn client(&self) -> Result<&Client, String> {
        self.client.as_ref().map_err(Clone::clone)
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
        let (outputs, windows) = self.client()?.snapshot()?;
        Ok((outputs, toplevels::without_overlay(windows)))
    }

    fn pointer_position(&self) -> Option<Point> {
        self.pointer
            .as_ref()
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
        let client = self.client()?;
        for batch in batches {
            client.keys(Some(batch.keymap), batch.steps)?;
        }
        Ok(())
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
        let screenshots = self.client.is_ok() && self.portal.available();
        let keyboard = self.client.as_ref().is_ok_and(Client::has_keyboard);
        Permissions {
            // Input is reachable when both the keyboard and a pointer are.
            accessibility: keyboard && self.pointer.is_some(),
            screen_recording: screenshots,
            screenshots_supported: screenshots,
            screenshots_unsupported: (!screenshots).then_some(ScreenshotsUnsupported::TooOld),
        }
    }

    fn capture(&self) -> Result<Capture, String> {
        let (outputs, windows) = self.snapshot()?;
        let output = toplevels::output_for_capture(&outputs, self.pointer_position(), toplevels::frontmost(&windows))
            .ok_or("no screen is known")?;
        let guard = self.guard.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let png = super::super::capture_order::capture(
            || guard.as_ref().map_or(Ok(()), |guard| guard.hide()),
            || self.portal.screenshot(),
        );
        if let Some(guard) = &guard {
            guard.restore();
        }
        shot::capture_of(&png?, output, &outputs)
    }

    fn post(&self, event: &InputEvent) -> Result<(), String> {
        match event {
            InputEvent::Key(combo) => self.send(vec![typing::combo_batch(combo)?]),
            InputEvent::Text(text) => self.send(typing::text_batches(text)?),
            InputEvent::Move(_) | InputEvent::Click { .. } | InputEvent::Drag { .. } | InputEvent::Scroll { .. } => {
                let pointer = self.pointer.as_ref().ok_or(POINTER_OFF)?;
                pointer.post(event)?;
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
        let client = self.client()?;
        if !client.can_list_windows() {
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
                return self.client()?.activate(window.id);
            }
        }
        self.launch(id, entry)
    }

    fn element_at(&self, _at: Point) -> Vec<AxElement> {
        Vec::new()
    }

    fn focused_element(&self) -> Option<Focus> {
        None
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
        toplevels::owner_at(&windows, &outputs, at).map(|window| self.identity(window))
    }

    fn activate(&self, pid: i32) {
        let Ok((_, windows)) = self.snapshot() else { return };
        let target = toplevels::app_for_pid(&windows, pid).and_then(|app_id| toplevels::window_to_activate(&windows, app_id));
        if let (Some(window), Ok(client)) = (target, self.client()) {
            if let Err(error) = client.activate(window.id) {
                eprintln!("gentle-dot: cannot bring the app forward: {error}");
            }
        }
    }
}

/// `computer_request_permission` on Linux: there is no screen-recording permission to ask for
/// (the portal decides per request), and full desktop control is not available yet.
pub fn request_permission(kind: crate::computer::PermissionKind) -> Result<(), String> {
    match kind {
        crate::computer::PermissionKind::ScreenRecording => Ok(()),
        crate::computer::PermissionKind::Accessibility => Err("Full desktop control is not available yet.".into()),
    }
}
