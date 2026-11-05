//! The X11 connection (S2): EWMH properties for the windows, RandR for the outputs, XTest for the
//! pointer and keys, `GetImage` for screenshots. It only asks the server and plays what `x11`
//! decided; the decisions are tested there.

use super::toplevels::{OutputInfo, WindowInfo};
use super::x11::{self, KeyAction, Mapping, PixelFormat, PointerAction, RawWindow};
use super::pointer::Rect;
use crate::computer::coords::Point;
use std::fmt::Display;
use std::sync::Mutex;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Properties are read whole; no window list or class is longer.
const PROPERTY_WORDS: u32 = 1 << 16;
/// The source indication for `_NET_ACTIVE_WINDOW`: a pager, which window managers honor.
const SOURCE_PAGER: u32 = 2;

struct Atoms {
    active_window: u32,
    client_list_stacking: u32,
    wm_pid: u32,
    wm_name: u32,
    utf8_string: u32,
    wm_state: u32,
    state_hidden: u32,
    frame_extents: u32,
}

pub struct X11 {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    xtest: bool,
    /// The window manager keeps `_NET_CLIENT_LIST_STACKING`.
    ewmh: bool,
    /// One input sequence at a time: keys and buttons of two calls never interleave.
    input: Mutex<()>,
    own_pid: u32,
}

fn failed(error: impl Display) -> String {
    format!("X11: {error}")
}

impl X11 {
    /// Connects to `DISPLAY`.
    pub fn connect() -> Result<X11, String> {
        let (conn, screen) = RustConnection::connect(None).map_err(|e| format!("cannot connect to the X display: {e}"))?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &[u8]| -> Result<u32, String> {
            Ok(conn.intern_atom(false, name).map_err(failed)?.reply().map_err(failed)?.atom)
        };
        let atoms = Atoms {
            active_window: atom(b"_NET_ACTIVE_WINDOW")?,
            client_list_stacking: atom(b"_NET_CLIENT_LIST_STACKING")?,
            wm_pid: atom(b"_NET_WM_PID")?,
            wm_name: atom(b"_NET_WM_NAME")?,
            utf8_string: atom(b"UTF8_STRING")?,
            wm_state: atom(b"_NET_WM_STATE")?,
            state_hidden: atom(b"_NET_WM_STATE_HIDDEN")?,
            frame_extents: atom(b"_NET_FRAME_EXTENTS")?,
        };
        let xtest = conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME).ok().flatten().is_some();
        let mut x11 = X11 { conn, root, atoms, xtest, ewmh: false, input: Mutex::new(()), own_pid: std::process::id() };
        x11.ewmh = x11.property(x11.root, x11.atoms.client_list_stacking, AtomEnum::WINDOW.into()).is_ok_and(|p| p.is_some());
        Ok(x11)
    }

    /// XTest is there: keys and the pointer can be sent.
    pub fn has_input(&self) -> bool {
        self.xtest
    }

    /// The window manager tells which windows exist and which is in front.
    pub fn can_list_windows(&self) -> bool {
        self.ewmh
    }

    fn property(&self, window: Window, property: u32, kind: u32) -> Result<Option<Vec<u8>>, String> {
        let reply = self
            .conn
            .get_property(false, window, property, kind, 0, PROPERTY_WORDS)
            .map_err(failed)?
            .reply()
            .map_err(failed)?;
        Ok((reply.value_len > 0).then_some(reply.value))
    }

    fn cardinals(&self, window: Window, property: u32, kind: AtomEnum) -> Option<Vec<u32>> {
        self.property(window, property, kind.into()).ok().flatten().map(|bytes| x11::cardinals(&bytes))
    }

    /// The outputs (RandR CRTCs; the root when RandR knows none) and the managed windows, bottom
    /// to top, as the window manager has them now. A window manager without EWMH lists none, so
    /// no app is ever in front or under a point.
    pub fn snapshot(&self) -> Result<(Vec<OutputInfo>, Vec<WindowInfo>), String> {
        let outputs = self.outputs()?;
        if !self.ewmh {
            return Ok((outputs, Vec::new()));
        }
        let active = self.cardinals(self.root, self.atoms.active_window, AtomEnum::WINDOW).and_then(|ids| ids.first().copied());
        let ids = self.cardinals(self.root, self.atoms.client_list_stacking, AtomEnum::WINDOW).unwrap_or_default();
        let windows = ids
            .into_iter()
            .filter_map(|id| self.raw_window(id))
            .map(|raw| x11::window_info(&raw, active.filter(|&id| id != 0), &outputs, self.own_pid))
            .collect();
        Ok((outputs, windows))
    }

    fn outputs(&self) -> Result<Vec<OutputInfo>, String> {
        let root_geometry = self.conn.get_geometry(self.root).map_err(failed)?.reply().map_err(failed)?;
        let root = Rect { x: 0, y: 0, width: i32::from(root_geometry.width), height: i32::from(root_geometry.height) };
        let crtcs = (|| -> Result<Vec<x11::Crtc>, String> {
            let resources = self.conn.randr_get_screen_resources_current(self.root).map_err(failed)?.reply().map_err(failed)?;
            resources
                .crtcs
                .iter()
                .map(|&id| {
                    let info = self.conn.randr_get_crtc_info(id, resources.config_timestamp).map_err(failed)?.reply().map_err(failed)?;
                    let rect = Rect {
                        x: i32::from(info.x),
                        y: i32::from(info.y),
                        width: i32::from(info.width),
                        height: i32::from(info.height),
                    };
                    Ok(x11::Crtc { id, rect, active: info.mode != 0 && !info.outputs.is_empty() })
                })
                .collect()
        })();
        // Without RandR the root is the one screen.
        Ok(x11::outputs_from_crtcs(&crtcs.unwrap_or_default(), root))
    }

    /// What a managed window reports; `None` when it went away or cannot be placed.
    fn raw_window(&self, id: Window) -> Option<RawWindow> {
        let wm_class = self.property(id, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into()).ok()?.unwrap_or_default();
        let title = self
            .property(id, self.atoms.wm_name, self.atoms.utf8_string)
            .ok()
            .flatten()
            .or_else(|| self.property(id, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into()).ok().flatten())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let pid = self.cardinals(id, self.atoms.wm_pid, AtomEnum::CARDINAL).and_then(|v| v.first().copied());
        let hidden = self
            .cardinals(id, self.atoms.wm_state, AtomEnum::ATOM)
            .is_some_and(|states| states.contains(&self.atoms.state_hidden));
        let extents = self
            .cardinals(id, self.atoms.frame_extents, AtomEnum::CARDINAL)
            .and_then(|v| <[u32; 4]>::try_from(v).ok());
        let viewable = self.conn.get_window_attributes(id).ok()?.reply().ok()?.map_state == MapState::VIEWABLE;
        let geometry = self.conn.get_geometry(id).ok()?.reply().ok()?;
        let origin = self.conn.translate_coordinates(id, self.root, 0, 0).ok()?.reply().ok()?;
        let rect = Rect {
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: i32::from(geometry.width),
            height: i32::from(geometry.height),
        };
        Some(RawWindow { id, wm_class, title, pid, hidden, viewable, rect, extents })
    }

    /// Where the pointer is.
    pub fn pointer(&self) -> Option<Point> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some(Point { x: f64::from(reply.root_x), y: f64::from(reply.root_y) })
    }

    /// Asks the window manager to bring a window forward (EWMH `_NET_ACTIVE_WINDOW`).
    pub fn activate(&self, window: u32) -> Result<(), String> {
        let event = ClientMessageEvent::new(32, window, self.atoms.active_window, [SOURCE_PAGER, 0, 0, 0, 0]);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        self.conn.send_event(false, self.root, mask, event).map_err(failed)?;
        self.conn.flush().map_err(failed)
    }

    /// The keyboard map now.
    pub fn mapping(&self) -> Result<Mapping, String> {
        let setup = self.conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let reply = self.conn.get_keyboard_mapping(first, last - first + 1).map_err(failed)?.reply().map_err(failed)?;
        Ok(Mapping { first, per: usize::from(reply.keysyms_per_keycode), keysyms: reply.keysyms })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.conn.xtest_fake_input(kind, detail, 0, self.root, x, y, 0).map_err(failed)?;
        Ok(())
    }

    fn settle(&self, pause: std::time::Duration) -> Result<(), String> {
        self.conn.flush().map_err(failed)?;
        std::thread::sleep(pause);
        Ok(())
    }

    /// Waits until the server handled everything sent.
    fn sync(&self) -> Result<(), String> {
        self.conn.get_input_focus().map_err(failed)?.reply().map_err(failed)?;
        Ok(())
    }

    fn bind(&self, keycode: u8, keysym: u32, per: usize) -> Result<(), String> {
        self.conn.change_keyboard_mapping(1, keycode, per.max(1) as u8, &vec![keysym; per.max(1)]).map_err(failed)?;
        Ok(())
    }

    /// Plays key steps. A keycode still bound to a character when something fails is freed again.
    pub fn keys(&self, actions: &[KeyAction]) -> Result<(), String> {
        if !self.xtest {
            return Err("the X server has no XTest extension".into());
        }
        let _one_at_a_time = self.input.lock().unwrap_or_else(|p| p.into_inner());
        let per = self.mapping()?.per;
        let mut bound: Vec<u8> = Vec::new();
        let played = (|| -> Result<(), String> {
            for action in actions {
                match action {
                    KeyAction::Remap(pairs) => {
                        for &(keycode, keysym) in pairs {
                            self.bind(keycode, keysym, per)?;
                            if keysym != 0 {
                                bound.push(keycode);
                            } else {
                                bound.retain(|&k| k != keycode);
                            }
                        }
                        self.conn.flush().map_err(failed)?;
                    }
                    KeyAction::Key { keycode, down } => {
                        let kind = if *down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
                        self.fake(kind, *keycode, 0, 0)?;
                    }
                    KeyAction::Pause(pause) => self.settle(*pause)?,
                }
            }
            self.sync()
        })();
        if played.is_err() {
            for keycode in bound {
                let _ = self.bind(keycode, 0, per);
            }
            let _ = self.conn.flush();
        }
        played
    }

    /// Plays pointer steps. A button still held when something fails is released.
    pub fn play_pointer(&self, actions: &[PointerAction]) -> Result<(), String> {
        if !self.xtest {
            return Err("the X server has no XTest extension".into());
        }
        let _one_at_a_time = self.input.lock().unwrap_or_else(|p| p.into_inner());
        let mut held: Vec<u8> = Vec::new();
        let played = (|| -> Result<(), String> {
            for action in actions {
                match action {
                    PointerAction::Move { x, y } => self.fake(xproto::MOTION_NOTIFY_EVENT, 0, *x, *y)?,
                    PointerAction::Button { button, down } => {
                        let kind = if *down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
                        self.fake(kind, *button, 0, 0)?;
                        if *down {
                            held.push(*button);
                        } else {
                            held.retain(|b| b != button);
                        }
                    }
                    PointerAction::Pause(pause) => self.settle(*pause)?,
                }
            }
            self.sync()
        })();
        if played.is_err() {
            for button in held {
                let _ = self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
            }
            let _ = self.conn.flush();
        }
        played
    }

    /// The pixels of `rect` on the root window, and how the server lays them out.
    pub fn grab(&self, rect: Rect) -> Result<(PixelFormat, Vec<u8>), String> {
        let (x, y) = (i16::try_from(rect.x).map_err(failed)?, i16::try_from(rect.y).map_err(failed)?);
        let (width, height) = (u16::try_from(rect.width).map_err(failed)?, u16::try_from(rect.height).map_err(failed)?);
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, width, height, !0)
            .map_err(failed)?
            .reply()
            .map_err(failed)?;
        let setup = self.conn.setup();
        let bits_per_pixel = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == image.depth)
            .map(|format| format.bits_per_pixel)
            .ok_or("the screen's pixel format is unknown")?;
        let visual = setup
            .roots
            .iter()
            .flat_map(|screen| &screen.allowed_depths)
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == image.visual)
            .ok_or("the screen's visual is unknown")?;
        let format = PixelFormat {
            bits_per_pixel,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
            msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
        };
        Ok((format, image.data))
    }
}
