//! Our own Wayland client for COSMIC (S2), on a thread of its own: the outputs with their
//! logical geometry (`zxdg_output_v1`), the windows (`ext_foreign_toplevel_list_v1` extended by
//! `zcosmic_toplevel_info_v1`), activating a window (`zcosmic_toplevel_manager_v1`), and the
//! virtual keyboard (`zwp_virtual_keyboard_v1`). The thread keeps `Shared` current as events
//! arrive; callers ask it to act through a channel and never wait longer than `TIMEOUT`.

use super::pointer::Rect;
use super::toplevels::{OutputInfo, WindowInfo};
use super::typing::KeyStep;
use cosmic_protocols::toplevel_info::v1::client::{zcosmic_toplevel_handle_v1, zcosmic_toplevel_info_v1};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{wl_callback, wl_output, wl_registry, wl_seat};
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};

/// How long a caller waits for the compositor to answer a request.
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// `xkb_keymap_format::XKB_V1`, the format of the keymap text.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;

/// What the thread has learned so far.
#[derive(Default)]
pub struct Shared {
    pub outputs: Vec<OutputInfo>,
    pub windows: Vec<WindowInfo>,
    /// The compositor lists windows (`zcosmic_toplevel_info_v1` v2+) and can activate them.
    pub toplevels: bool,
    /// The virtual keyboard is bound to a seat.
    pub keyboard: bool,
}

enum Command {
    /// Answer once the compositor handled everything sent before.
    Sync(Reply),
    Activate { window: u32, reply: Reply },
    /// Upload `keymap` (if any), play `steps`, answer once the compositor handled them.
    Keys { keymap: Option<String>, steps: Vec<KeyStep>, reply: Reply },
}

type Reply = Sender<Result<(), String>>;

/// The user data of a `wl_callback` that answers a caller.
struct Answer(Reply);

struct OutputEntry {
    name: u32,
    wl: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    label: String,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

struct TopEntry {
    id: u32,
    ext: ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    cosmic: Option<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1>,
    app_id: String,
    title: String,
    activated: bool,
    minimized: bool,
    /// Geometry per `wl_output`, relative to it.
    frames: Vec<(ObjectId, Rect)>,
}

struct State {
    shared: Arc<Mutex<Shared>>,
    qh: QueueHandle<State>,
    started: Instant,
    outputs: Vec<OutputEntry>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard_manager: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    list: Option<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    info: Option<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1>,
    manager: Option<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1>,
    toplevels: Vec<TopEntry>,
    next_window: u32,
    dirty: bool,
}

impl State {
    fn new(shared: Arc<Mutex<Shared>>, qh: QueueHandle<State>) -> Self {
        State {
            shared,
            qh,
            started: Instant::now(),
            outputs: Vec::new(),
            xdg_manager: None,
            seat: None,
            keyboard_manager: None,
            keyboard: None,
            list: None,
            info: None,
            manager: None,
            toplevels: Vec::new(),
            next_window: 1,
            dirty: true,
        }
    }

    /// Creates what needed two globals once both are bound, whichever arrived first.
    fn link(&mut self) {
        if let Some(manager) = &self.xdg_manager {
            for output in self.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
                output.xdg = Some(manager.get_xdg_output(&output.wl, &self.qh, output.name));
            }
        }
        if self.keyboard.is_none() {
            if let (Some(manager), Some(seat)) = (&self.keyboard_manager, &self.seat) {
                self.keyboard = Some(manager.create_virtual_keyboard(seat, &self.qh, ()));
                self.dirty = true;
            }
        }
        if let Some(info) = &self.info {
            for top in self.toplevels.iter_mut().filter(|t| t.cosmic.is_none()) {
                top.cosmic = Some(info.get_cosmic_toplevel(&top.ext, &self.qh, ()));
            }
        }
    }

    fn output_name(&self, wl: &ObjectId) -> Option<u32> {
        self.outputs.iter().find(|o| o.wl.id() == *wl).map(|o| o.name)
    }

    fn top_of_cosmic(&mut self, handle: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1) -> Option<&mut TopEntry> {
        self.toplevels.iter_mut().find(|t| t.cosmic.as_ref().is_some_and(|c| c.id() == handle.id()))
    }

    fn top_of_ext(&mut self, handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1) -> Option<&mut TopEntry> {
        self.toplevels.iter_mut().find(|t| t.ext.id() == handle.id())
    }

    /// Copies what changed into `Shared`.
    fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let outputs: Vec<OutputInfo> = self
            .outputs
            .iter()
            .filter_map(|o| {
                let ((x, y), (width, height)) = (o.position?, o.size?);
                (width > 0 && height > 0).then(|| OutputInfo {
                    id: o.name,
                    name: o.label.clone(),
                    rect: Rect { x, y, width, height },
                })
            })
            .collect();
        let windows = self
            .toplevels
            .iter()
            .map(|t| WindowInfo {
                id: t.id,
                app_id: t.app_id.clone(),
                title: t.title.clone(),
                activated: t.activated,
                minimized: t.minimized,
                frames: t
                    .frames
                    .iter()
                    .filter_map(|(wl, rect)| Some((self.output_name(wl)?, *rect)))
                    .collect(),
            })
            .collect();
        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        shared.outputs = outputs;
        shared.windows = windows;
        shared.toplevels = self.info.is_some() && self.list.is_some() && self.manager.is_some() && self.seat.is_some();
        shared.keyboard = self.keyboard.is_some();
    }

    fn command(&mut self, command: Command, conn: &Connection) {
        match command {
            Command::Sync(reply) => self.sync(conn, reply),
            Command::Activate { window, reply } => {
                let target = self.toplevels.iter().find(|t| t.id == window).and_then(|t| t.cosmic.as_ref());
                match (target, &self.manager, &self.seat) {
                    (Some(handle), Some(manager), Some(seat)) => {
                        manager.activate(handle, seat);
                        self.sync(conn, reply);
                    }
                    _ => {
                        let _ = reply.send(Err("the window cannot be activated".into()));
                    }
                }
            }
            Command::Keys { keymap, steps, reply } => match self.play(keymap, &steps) {
                Ok(()) => self.sync(conn, reply),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
        }
    }

    fn sync(&self, conn: &Connection, reply: Reply) {
        conn.display().sync(&self.qh, Answer(reply));
    }

    fn play(&self, keymap: Option<String>, steps: &[KeyStep]) -> Result<(), String> {
        let keyboard = self.keyboard.as_ref().ok_or("the virtual keyboard is not available")?;
        if let Some(text) = keymap {
            let (fd, size) = keymap_file(&text)?;
            keyboard.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), size);
        }
        for step in steps {
            let time = self.started.elapsed().as_millis() as u32;
            match *step {
                KeyStep::Modifiers(mask) => keyboard.modifiers(mask, 0, 0, 0),
                KeyStep::Key { code, down } => keyboard.key(time, u32::from(code), u32::from(down)),
            }
        }
        Ok(())
    }
}

/// An anonymous file holding the keymap text and a closing NUL, and its size.
fn keymap_file(text: &str) -> Result<(OwnedFd, u32), String> {
    // SAFETY: a plain syscall with a valid NUL-terminated name; the fd it returns is owned below.
    let raw = unsafe { libc::memfd_create(c"gentle-dot-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(format!("cannot create the keymap file: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `raw` is a fresh descriptor that nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    file.write_all(&bytes).map_err(|e| format!("cannot write the keymap: {e}"))?;
    Ok((OwnedFd::from(file), bytes.len() as u32))
}

/// (activated, minimized) in a `state` array of native-endian u32s.
fn state_flags(bytes: &[u8]) -> (bool, bool) {
    let states: Vec<u32> = bytes.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
    (states.contains(&STATE_ACTIVATED), states.contains(&STATE_MINIMIZED))
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                match interface.as_str() {
                    "wl_output" => {
                        let wl = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                        state.outputs.push(OutputEntry { name, wl, xdg: None, label: String::new(), position: None, size: None });
                    }
                    "zxdg_output_manager_v1" => {
                        state.xdg_manager =
                            Some(registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
                    }
                    "wl_seat" if state.seat.is_none() => {
                        state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ()));
                    }
                    "zwp_virtual_keyboard_manager_v1" => {
                        state.keyboard_manager = Some(
                            registry.bind::<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(name, 1, qh, ()),
                        );
                    }
                    "ext_foreign_toplevel_list_v1" => {
                        state.list = Some(
                            registry.bind::<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, _, _>(name, 1, qh, ()),
                        );
                    }
                    // Version 2 added `get_cosmic_toplevel`, which the list above needs.
                    "zcosmic_toplevel_info_v1" if version >= 2 => {
                        state.info = Some(
                            registry.bind::<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, _, _>(name, version.min(3), qh, ()),
                        );
                    }
                    "zcosmic_toplevel_manager_v1" => {
                        state.manager = Some(
                            registry.bind::<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1, _, _>(name, version.min(4), qh, ()),
                        );
                    }
                    _ => return,
                }
                state.link();
                state.dirty = true;
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.name != name);
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, name: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name: label } = event {
            if let Some(output) = state.outputs.iter_mut().find(|o| o.name == *name) {
                output.label = label;
                state.dirty = true;
            }
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event, name: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(output) = state.outputs.iter_mut().find(|o| o.name == *name) else { return };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => output.position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => output.size = Some((width, height)),
            zxdg_output_v1::Event::Name { name } if output.label.is_empty() => output.label = name,
            _ => return,
        }
        state.dirty = true;
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let cosmic = state.info.as_ref().map(|info| info.get_cosmic_toplevel(&toplevel, qh, ()));
            let id = state.next_window;
            state.next_window += 1;
            state.toplevels.push(TopEntry {
                id,
                ext: toplevel,
                cosmic,
                app_id: String::new(),
                title: String::new(),
                activated: false,
                minimized: false,
                frames: Vec::new(),
            });
            state.dirty = true;
        }
    }

    event_created_child!(State, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        match event {
            Event::Closed => {
                state.toplevels.retain(|t| t.ext.id() != handle.id());
            }
            Event::Title { title } => {
                if let Some(top) = state.top_of_ext(handle) {
                    top.title = title;
                }
            }
            Event::AppId { app_id } => {
                if let Some(top) = state.top_of_ext(handle) {
                    top.app_id = app_id;
                }
            }
            _ => return,
        }
        state.dirty = true;
    }
}

impl Dispatch<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            state.dirty = true;
        }
    }

    // Only sent to clients that bind version 1, which this one never does.
    event_created_child!(State, zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, [
        zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zcosmic_toplevel_handle_v1::Event;
        match event {
            Event::State { state: bytes } => {
                if let Some(top) = state.top_of_cosmic(handle) {
                    (top.activated, top.minimized) = state_flags(&bytes);
                }
            }
            Event::Geometry { output, x, y, width, height } => {
                let wl = output.id();
                if let Some(top) = state.top_of_cosmic(handle) {
                    let rect = Rect { x, y, width, height };
                    match top.frames.iter_mut().find(|(known, _)| *known == wl) {
                        Some((_, frame)) => *frame = rect,
                        None => top.frames.push((wl, rect)),
                    }
                }
            }
            Event::OutputLeave { output } => {
                let wl = output.id();
                if let Some(top) = state.top_of_cosmic(handle) {
                    top.frames.retain(|(known, _)| *known != wl);
                }
            }
            _ => return,
        }
        state.dirty = true;
    }
}

impl Dispatch<wl_callback::WlCallback, Answer> for State {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, answer: &Answer, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            let _ = answer.0.send(Ok(()));
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(State: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1);

/// The client: a handle on the Wayland thread.
pub struct Client {
    shared: Arc<Mutex<Shared>>,
    commands: Sender<Command>,
    wake: OwnedFd,
}

impl Client {
    /// Connects to `WAYLAND_DISPLAY` and starts the thread once the first globals arrived.
    pub fn connect() -> Result<Client, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("cannot connect to the Wayland display: {e}"))?;
        let queue = conn.new_event_queue::<State>();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (commands, inbox) = channel();
        let (wake_read, wake_write) = wake_pipe()?;
        let (ready_tx, ready_rx) = channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("computer-wayland".into())
            .spawn(move || run(conn, queue, thread_shared, inbox, wake_read, ready_tx))
            .map_err(|e| format!("cannot start the Wayland thread: {e}"))?;
        match ready_rx.recv_timeout(TIMEOUT) {
            Ok(Ok(())) => Ok(Client { shared, commands, wake: wake_write }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the compositor did not answer in time".into()),
        }
    }

    /// The outputs and windows now, after the compositor handled everything sent so far.
    pub fn snapshot(&self) -> Result<(Vec<OutputInfo>, Vec<WindowInfo>), String> {
        self.request(Command::Sync)?;
        let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        Ok((shared.outputs.clone(), shared.windows.clone()))
    }

    pub fn can_list_windows(&self) -> bool {
        self.shared.lock().unwrap_or_else(|p| p.into_inner()).toplevels
    }

    pub fn has_keyboard(&self) -> bool {
        self.shared.lock().unwrap_or_else(|p| p.into_inner()).keyboard
    }

    pub fn activate(&self, window: u32) -> Result<(), String> {
        self.request(|reply| Command::Activate { window, reply })
    }

    pub fn keys(&self, keymap: Option<String>, steps: Vec<KeyStep>) -> Result<(), String> {
        self.request(|reply| Command::Keys { keymap, steps, reply })
    }

    fn request(&self, make: impl FnOnce(Reply) -> Command) -> Result<(), String> {
        let (reply, answer) = channel();
        self.commands.send(make(reply)).map_err(|_| closed())?;
        // SAFETY: a one-byte write to our own pipe; a full pipe already has a wake-up pending.
        unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        match answer.recv_timeout(TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err("the compositor did not answer in time".into()),
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }
}

fn closed() -> String {
    "the Wayland connection closed".into()
}

fn wake_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
        return Err(format!("cannot create a pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: both descriptors are fresh and owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn drain(fd: &OwnedFd) {
    let mut buffer = [0u8; 64];
    // SAFETY: reads into our own buffer from our own non-blocking pipe.
    while unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
}

fn run(
    conn: Connection,
    mut queue: EventQueue<State>,
    shared: Arc<Mutex<Shared>>,
    inbox: Receiver<Command>,
    wake: OwnedFd,
    ready: Sender<Result<(), String>>,
) {
    let mut state = State::new(shared.clone(), queue.handle());
    let _registry = conn.display().get_registry(&queue.handle(), ());
    // The first round trip delivers the globals (and binds them), the second their first events.
    for _ in 0..2 {
        if let Err(error) = queue.roundtrip(&mut state) {
            let _ = ready.send(Err(format!("the Wayland handshake failed: {error}")));
            return;
        }
    }
    state.publish();
    let _ = ready.send(Ok(()));
    if let Err(error) = serve(&conn, &mut queue, &mut state, &inbox, &wake) {
        eprintln!("gentle-dot: the Wayland connection for computer control ended: {error}");
    }
    let mut shared = shared.lock().unwrap_or_else(|p| p.into_inner());
    *shared = Shared::default();
}

fn serve(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    inbox: &Receiver<Command>,
    wake: &OwnedFd,
) -> Result<(), String> {
    let would_block = |e: &WaylandError| matches!(e, WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock);
    loop {
        loop {
            match inbox.try_recv() {
                Ok(command) => state.command(command, conn),
                Err(TryRecvError::Empty) => break,
                // The client was dropped.
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        state.publish();
        match conn.flush() {
            Err(e) if !would_block(&e) => return Err(e.to_string()),
            _ => {}
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let mut fds = [
            libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // A short timeout also retries a flush that would have blocked.
        // SAFETY: `fds` is a valid array of two pollfd entries.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Err("the compositor closed the connection".into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            match guard.read() {
                Err(e) if !would_block(&e) => return Err(e.to_string()),
                _ => {}
            }
        } else {
            drop(guard);
        }
        if fds[1].revents & libc::POLLIN != 0 {
            drain(wake);
        }
    }
}
