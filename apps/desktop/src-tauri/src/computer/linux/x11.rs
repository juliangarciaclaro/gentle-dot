//! The X11 session's decisions (S2): what EWMH and the X server reported, turned into the same
//! outputs and windows the Wayland client fills in, which window is on top at a point, what the
//! XTest pointer and keys are sent, and how a `GetImage` reply becomes a screenshot. Pure, so it
//! is tested everywhere; `x11_client` only talks to the server.
//!
//! Every answer fails closed like the Wayland ones: a point nobody is known to own has no owner.

use super::pointer::Rect;
use super::toplevels::{self, OutputInfo, WindowInfo};
use super::{evdev, keymap, uinput};
use crate::computer::coords::Point;
use crate::computer::keys::KeyCombo;
use crate::computer::{Capture, InputEvent, MouseButton};
use std::time::Duration;

/// X keycode = evdev code + 8 on every modern server; the fallback when a key is not in the map.
const KEYCODE_OFFSET: u16 = 8;
const SHIFT_L: u32 = 0xffe1;
const CONTROL_L: u32 = 0xffe3;
const ALT_L: u32 = 0xffe9;
/// Most spare keycodes one chunk of text remaps at once.
pub const MAX_SPARE: usize = 16;
/// Clients re-read the keyboard map when they see the change; they need a moment before the keys.
const REMAP_SETTLE: Duration = Duration::from_millis(30);
/// Wait for the pointer to arrive before a button or the wheel acts.
const ARRIVE: Duration = Duration::from_millis(30);
/// Wait between the press and the release of a click, and between the clicks of a double click.
const CLICK_GAP: Duration = Duration::from_millis(30);
/// Wait between the motion events of a drag, which apps need to see the item move.
const DRAG_GAP: Duration = Duration::from_millis(20);

/// The id given to Gentle Dot's own windows (found by `_NET_WM_PID`), whatever class they carry.
pub const OWN_APP_ID: &str = "dev.gentleman.gentledot";

/// The CARD32 values of a format-32 property.
pub fn cardinals(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|&value| u32::from_ne_bytes(value)).collect()
}

/// The app id from `WM_CLASS` (`instance\0Class\0`): the class, else the instance, else empty.
pub fn app_id(wm_class: &[u8]) -> String {
    let instance = wm_class.split(|&b| b == 0).find(|field| !field.is_empty());
    // `instance\0\0` has no class; the second field is the class when there is one.
    let class = wm_class.split(|&b| b == 0).nth(1).filter(|field| !field.is_empty());
    class.or(instance).map(|field| String::from_utf8_lossy(field).into_owned()).unwrap_or_default()
}

/// What one managed window reported, before it is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWindow {
    pub id: u32,
    pub wm_class: Vec<u8>,
    pub title: String,
    pub pid: Option<u32>,
    /// `_NET_WM_STATE` holds `_NET_WM_STATE_HIDDEN`.
    pub hidden: bool,
    /// The window is mapped (a window on another workspace or iconified is not).
    pub viewable: bool,
    /// The client area, in root coordinates.
    pub rect: Rect,
    /// `_NET_FRAME_EXTENTS`: left, right, top, bottom.
    pub extents: Option<[u32; 4]>,
}

/// A CRTC as RandR reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crtc {
    pub id: u32,
    pub rect: Rect,
    /// A mode is set and an output is attached.
    pub active: bool,
}

/// The outputs of the layout: the active CRTCs (mirrored ones once), or the whole root when
/// RandR knows none.
pub fn outputs_from_crtcs(crtcs: &[Crtc], root: Rect) -> Vec<OutputInfo> {
    let mut outputs: Vec<OutputInfo> = Vec::new();
    for crtc in crtcs.iter().filter(|c| c.active && c.rect.width > 0 && c.rect.height > 0) {
        if !outputs.iter().any(|o| o.rect == crtc.rect) {
            outputs.push(OutputInfo { id: crtc.id, name: format!("crtc-{}", crtc.id), rect: crtc.rect });
        }
    }
    if outputs.is_empty() {
        outputs.push(OutputInfo { id: 0, name: "screen".into(), rect: root });
    }
    outputs
}

/// The window as the shared decisions see it: one frame per output it touches, relative to it.
pub fn window_info(raw: &RawWindow, active: Option<u32>, outputs: &[OutputInfo], own_pid: u32) -> WindowInfo {
    let app_id = if raw.pid == Some(own_pid) { OWN_APP_ID.to_string() } else { app_id(&raw.wm_class) };
    let [left, right, top, bottom] = raw.extents.unwrap_or_default().map(|v| i32::try_from(v).unwrap_or(0));
    let outer = Rect {
        x: raw.rect.x - left,
        y: raw.rect.y - top,
        width: raw.rect.width + left + right,
        height: raw.rect.height + top + bottom,
    };
    let frames = outputs
        .iter()
        .filter(|o| outer.x < o.rect.x + o.rect.width && o.rect.x < outer.x + outer.width)
        .filter(|o| outer.y < o.rect.y + o.rect.height && o.rect.y < outer.y + outer.height)
        .map(|o| (o.id, Rect { x: outer.x - o.rect.x, y: outer.y - o.rect.y, ..outer }))
        .collect();
    WindowInfo {
        id: raw.id,
        app_id,
        title: raw.title.clone(),
        activated: active == Some(raw.id),
        minimized: raw.hidden || !raw.viewable,
        frames,
    }
}

/// The topmost window holding `at`, among windows ordered bottom to top; minimized ones never own
/// a point.
pub fn topmost_at<'a>(stacked: &'a [WindowInfo], outputs: &[OutputInfo], at: Point) -> Option<&'a WindowInfo> {
    stacked
        .iter()
        .rev()
        .find(|w| !w.minimized && toplevels::global_rects(w, outputs).iter().any(|rect| toplevels::contains(rect, at)))
}

/// One child of the root window as `QueryTree` lists it (bottom to top), before it is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootChild {
    pub id: u32,
    pub rect: Rect,
    pub override_redirect: bool,
    pub viewable: bool,
    /// An `InputOnly` window draws nothing.
    pub input_only: bool,
    /// Gentle Dot's own (its overlay is click-through).
    pub own: bool,
}

/// The mapped override-redirect windows (menus, tooltips) with their place among the root's
/// children, as windows with no app: the window manager does not list them, and nobody can
/// name their owner, so a point they hold fails closed (an empty app id is the unidentified app).
/// A window covering the whole root (a desktop, the compositor overlay) is not a popup.
pub fn popups(children: &[RootChild], root: Rect, outputs: &[OutputInfo]) -> Vec<(usize, WindowInfo)> {
    let covers_root = |r: &Rect| r.x <= root.x && r.y <= root.y && r.width >= root.width && r.height >= root.height;
    children
        .iter()
        .enumerate()
        .filter(|(_, c)| c.override_redirect && c.viewable && !c.input_only && !c.own && !covers_root(&c.rect))
        .map(|(index, c)| {
            let anonymous = RawWindow {
                id: c.id,
                wm_class: Vec::new(),
                title: String::new(),
                pid: None,
                hidden: false,
                viewable: true,
                rect: c.rect,
                extents: None,
            };
            (index, window_info(&anonymous, None, outputs, 0))
        })
        .collect()
}

/// The managed windows (bottom to top, each with its frame's place among the root's children)
/// with the popups put where the server stacks them: before the first window whose frame is
/// above them. A window whose frame is unknown is taken to be below every popup.
pub fn stack_with_popups(managed: Vec<(WindowInfo, Option<usize>)>, popups: Vec<(usize, WindowInfo)>) -> Vec<WindowInfo> {
    let mut stacked = managed;
    for (index, popup) in popups {
        let at = stacked.iter().position(|(_, frame)| frame.is_some_and(|frame| frame > index)).unwrap_or(stacked.len());
        stacked.insert(at, (popup, Some(index)));
    }
    stacked.into_iter().map(|(window, _)| window).collect()
}

/// The server's keyboard map: `per` keysyms for each keycode from `first`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub first: u8,
    pub per: usize,
    pub keysyms: Vec<u32>,
}

impl Mapping {
    /// The keycode that types `keysym` and whether Shift is needed: on the first level, else the second.
    pub fn find(&self, keysym: u32) -> Option<(u8, bool)> {
        if self.per == 0 || keysym == 0 {
            return None;
        }
        let column = |level: usize| {
            let at = self.keysyms.chunks(self.per).position(|syms| syms.get(level) == Some(&keysym));
            at.map(|index| self.first + index as u8)
        };
        column(0).map(|keycode| (keycode, false)).or_else(|| column(1).map(|keycode| (keycode, true)))
    }

    /// Up to `max` keycodes bound to nothing, highest first.
    pub fn spare(&self, max: usize) -> Vec<u8> {
        if self.per == 0 {
            return Vec::new();
        }
        let free = self.keysyms.chunks(self.per).enumerate().filter(|(_, syms)| syms.iter().all(|&sym| sym == 0));
        free.map(|(index, _)| self.first + index as u8).rev().take(max).collect()
    }
}

/// One step of a key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Rebinds keycodes to keysyms (0 frees one).
    Remap(Vec<(u8, u32)>),
    Key { keycode: u8, down: bool },
    Pause(Duration),
}

/// The presses for a `key` combo.
pub fn combo_actions(combo: &KeyCombo, map: &Mapping) -> Result<Vec<KeyAction>, String> {
    let code = evdev::key_code(combo.key).ok_or_else(|| format!("no key code for `{}`", combo.key))?;
    let key = base_keycode(map, named_keysym(combo.key), code);
    let held: Vec<u8> = evdev::held_modifiers(&combo.modifiers)
        .into_iter()
        .map(|code| {
            let keysym = match code {
                evdev::KEY_LEFTCTRL => CONTROL_L,
                evdev::KEY_LEFTSHIFT => SHIFT_L,
                _ => ALT_L,
            };
            base_keycode(map, Some(keysym), code)
        })
        .collect();
    let mut actions: Vec<KeyAction> = held.iter().map(|&keycode| KeyAction::Key { keycode, down: true }).collect();
    actions.extend(tap(key));
    actions.extend(held.iter().rev().map(|&keycode| KeyAction::Key { keycode, down: false }));
    Ok(actions)
}

/// The presses that type `text`: characters the map has go through their own keycode (with
/// Shift when they sit on the shifted level), the others through spare keycodes remapped for a
/// chunk of up to `MAX_SPARE` of them and freed after. Newlines and tabs are Return and Tab; any
/// other control character is an error before anything is sent.
pub fn text_actions(text: &str, map: &Mapping) -> Result<Vec<KeyAction>, String> {
    let shift = base_keycode(map, Some(SHIFT_L), evdev::KEY_LEFTSHIFT);
    let named = |key: &str| -> Result<u8, String> {
        let code = evdev::key_code(key).ok_or_else(|| format!("no key code for `{key}`"))?;
        Ok(base_keycode(map, named_keysym(key), code))
    };
    let spare = map.spare(MAX_SPARE);
    let (mut out, mut chunk, mut bound): (Vec<KeyAction>, Vec<KeyAction>, Vec<(u8, u32)>) = Default::default();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // "\r\n" and a lone "\r" are one newline, as in the other backends.
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => chunk.extend(tap(named("return")?)),
            '\t' => chunk.extend(tap(named("tab")?)),
            c if c.is_control() => return Err(format!("cannot type control character U+{:04X}", c as u32)),
            c => {
                let keysym = keymap::keysym(c);
                if let Some((keycode, shifted)) = map.find(keysym) {
                    if shifted {
                        chunk.push(KeyAction::Key { keycode: shift, down: true });
                    }
                    chunk.extend(tap(keycode));
                    if shifted {
                        chunk.push(KeyAction::Key { keycode: shift, down: false });
                    }
                    continue;
                }
                let keycode = match bound.iter().find(|(_, bound)| *bound == keysym) {
                    Some(&(keycode, _)) => keycode,
                    None => {
                        if spare.is_empty() {
                            return Err(format!("no free keycode to type `{c}`"));
                        }
                        if bound.len() == spare.len() {
                            flush(&mut out, &mut chunk, &mut bound);
                        }
                        bound.push((spare[bound.len()], keysym));
                        spare[bound.len() - 1]
                    }
                };
                chunk.extend(tap(keycode));
            }
        }
    }
    flush(&mut out, &mut chunk, &mut bound);
    Ok(out)
}

/// One step of a pointer action, in root coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerAction {
    Move { x: i16, y: i16 },
    Button { button: u8, down: bool },
    Pause(Duration),
}

/// What a pointer `InputEvent` becomes. A target outside every output is an error, and nothing
/// is sent.
pub fn pointer_actions(event: &InputEvent, layout: &[Rect]) -> Result<Vec<PointerAction>, String> {
    let target = |at: &Point| -> Result<(i16, i16), String> {
        if !layout.iter().any(|rect| toplevels::contains(rect, *at)) {
            return Err(format!("({}, {}) is outside every screen", at.x, at.y));
        }
        Ok((at.x.round() as i16, at.y.round() as i16))
    };
    let go = |at: &Point| -> Result<PointerAction, String> {
        let (x, y) = target(at)?;
        Ok(PointerAction::Move { x, y })
    };
    let press = |button: u8| [PointerAction::Button { button, down: true }, PointerAction::Button { button, down: false }];
    match event {
        InputEvent::Move(at) => Ok(vec![go(at)?]),
        InputEvent::Click { at, button, count } => {
            if !(1..=uinput::MAX_CLICKS).contains(count) {
                return Err(format!("a click of {count} presses is not supported"));
            }
            let button = match button {
                MouseButton::Left => 1,
                MouseButton::Right => 3,
            };
            let mut actions = vec![go(at)?, PointerAction::Pause(ARRIVE)];
            for _ in 0..*count {
                let [down, up] = press(button);
                actions.extend([down, PointerAction::Pause(CLICK_GAP), up, PointerAction::Pause(CLICK_GAP)]);
            }
            Ok(actions)
        }
        InputEvent::Drag { from, to } => {
            // Halfway first, so the app sees the item travel.
            let half = Point { x: (from.x + to.x) / 2.0, y: (from.y + to.y) / 2.0 };
            Ok(vec![
                go(from)?,
                PointerAction::Pause(ARRIVE),
                PointerAction::Button { button: 1, down: true },
                PointerAction::Pause(DRAG_GAP),
                go(&half)?,
                PointerAction::Pause(DRAG_GAP),
                go(to)?,
                PointerAction::Pause(DRAG_GAP),
                PointerAction::Button { button: 1, down: false },
            ])
        }
        InputEvent::Scroll { at, dx, dy } => {
            let mut actions = vec![go(at)?];
            // Buttons 4 and 5 are the wheel up and down, 6 and 7 left and right.
            for (pixels, negative, positive) in [(*dy, 4, 5), (*dx, 6, 7)] {
                let notches = uinput::notches(pixels);
                for _ in 0..notches.unsigned_abs() {
                    actions.extend(press(if notches < 0 { negative } else { positive }));
                }
            }
            if actions.len() > 1 {
                actions.insert(1, PointerAction::Pause(ARRIVE));
            }
            Ok(actions)
        }
        InputEvent::Key(_) | InputEvent::Text(_) => Err("the pointer does not type".into()),
    }
}

/// How the server lays out the pixels of a `GetImage` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub msb_first: bool,
}

/// The screenshot of `output` from the pixels of its `GetImage` reply (ZPixmap).
pub fn capture_of_pixels(data: &[u8], format: PixelFormat, output: &OutputInfo) -> Result<Capture, String> {
    // The common layout: 32 bits per pixel, little-endian B, G, R, unused.
    if format.bits_per_pixel != 32
        || format.msb_first
        || (format.red_mask, format.green_mask, format.blue_mask) != (0xff_0000, 0xff00, 0xff)
    {
        return Err(format!("the screen's pixel format is not supported: {format:?}"));
    }
    let (width, height) = (output.rect.width.max(0) as usize, output.rect.height.max(0) as usize);
    if width == 0 || height == 0 || data.len() != width * height * 4 {
        return Err(format!("unexpected screenshot: {width} × {height} pixels, {} bytes", data.len()));
    }
    let mut rgba = Vec::with_capacity(data.len());
    for pixel in data.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    let region = image::RgbaImage::from_raw(width as u32, height as u32, rgba).ok_or("unexpected screenshot size")?;
    super::shot::capture_of_region(region, output)
}

fn tap(keycode: u8) -> [KeyAction; 2] {
    [KeyAction::Key { keycode, down: true }, KeyAction::Key { keycode, down: false }]
}

/// The keycode for a key: where the map has its keysym on the first level, else its evdev
/// position.
fn base_keycode(map: &Mapping, keysym: Option<u32>, evdev_code: u16) -> u8 {
    let found = keysym.and_then(|keysym| map.find(keysym)).filter(|(_, shifted)| !shifted);
    found.map_or((evdev_code + KEYCODE_OFFSET) as u8, |(keycode, _)| keycode)
}

/// The keysym of a key name from `keys`.
fn named_keysym(key: &str) -> Option<u32> {
    Some(match key {
        "return" => 0xff0d, "delete" => 0xff08, "forwarddelete" => 0xffff, "tab" => 0xff09,
        "escape" => 0xff1b, "home" => 0xff50, "left" => 0xff51, "up" => 0xff52, "right" => 0xff53,
        "down" => 0xff54, "pageup" => 0xff55, "pagedown" => 0xff56, "end" => 0xff57,
        f if f.len() > 1 && f.starts_with('f') => 0xffbd + f[1..].parse::<u32>().ok().filter(|n| (1..=12).contains(n))?,
        // Letters, digits, `space`, and punctuation are their own ASCII keysyms.
        "space" => 0x20,
        single if single.len() == 1 => u32::from(single.as_bytes()[0]),
        _ => return None,
    })
}

/// Binds a chunk's keycodes, plays it, and frees them again.
fn flush(out: &mut Vec<KeyAction>, chunk: &mut Vec<KeyAction>, bound: &mut Vec<(u8, u32)>) {
    if bound.is_empty() {
        out.append(chunk);
        return;
    }
    out.extend([KeyAction::Remap(bound.clone()), KeyAction::Pause(REMAP_SETTLE)]);
    out.append(chunk);
    out.extend([KeyAction::Pause(REMAP_SETTLE), KeyAction::Remap(bound.drain(..).map(|(keycode, _)| (keycode, 0)).collect())]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::keys::parse_combo;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect { x, y, width, height }
    }

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn outputs() -> Vec<OutputInfo> {
        vec![
            OutputInfo { id: 10, name: "crtc-10".into(), rect: rect(0, 0, 1920, 1080) },
            OutputInfo { id: 11, name: "crtc-11".into(), rect: rect(1920, 0, 1920, 1080) },
        ]
    }

    fn raw(id: u32, class: &str, rect: Rect) -> RawWindow {
        RawWindow {
            id,
            wm_class: format!("inst\0{class}\0").into_bytes(),
            title: format!("{class} window"),
            pid: Some(900 + id),
            hidden: false,
            viewable: true,
            rect,
            extents: None,
        }
    }

    #[test]
    fn properties_decode_to_cardinals_and_app_ids() {
        let bytes: Vec<u8> = [7u32, 0x0040_0003, 9].iter().flat_map(|v| v.to_ne_bytes()).collect();
        // A trailing partial value is dropped.
        for (input, expected) in [(&bytes[..], vec![7, 0x0040_0003, 9]), (&bytes[..10], vec![7, 0x0040_0003]), (&[][..], vec![])] {
            assert_eq!(cardinals(input), expected);
        }
        for (class, expected) in [
            (&b"firefox\0Firefox\0"[..], "Firefox"),
            (b"navigator\0Navigator", "Navigator"),
            (b"onlyinstance\0", "onlyinstance"),
            (b"onlyinstance", "onlyinstance"),
            (b"inst\0\0", "inst"),
            (b"\0", ""),
            (b"", ""),
        ] {
            assert_eq!(app_id(class), expected, "{class:?}");
        }
    }

    #[test]
    fn crtcs_become_outputs_without_the_inactive_or_mirrored_ones() {
        let crtc = |id, active, r| Crtc { id, rect: r, active };
        let root = rect(0, 0, 3840, 1080);
        let found = outputs_from_crtcs(
            &[
                crtc(10, true, rect(0, 0, 1920, 1080)),
                crtc(11, true, rect(1920, 0, 1920, 1080)),
                // Mirrors the first; an unused CRTC; an attached one with no size.
                crtc(12, true, rect(0, 0, 1920, 1080)),
                crtc(13, false, rect(0, 0, 1280, 720)),
                crtc(14, true, rect(0, 0, 0, 0)),
            ],
            root,
        );
        assert_eq!(found, outputs());
        // RandR knows nothing usable: the root is the one screen.
        let whole = outputs_from_crtcs(&[crtc(13, false, rect(0, 0, 10, 10))], root);
        assert_eq!(whole.iter().map(|o| o.rect).collect::<Vec<_>>(), [root]);
    }

    #[test]
    fn a_window_is_framed_on_each_output_it_touches_and_judged_by_its_flags() {
        let outs = outputs();
        let straddling = raw(1, "Firefox", rect(1800, 100, 400, 300));
        let info = window_info(&straddling, Some(1), &outs, 4242);
        assert_eq!((info.id, info.app_id.as_str(), info.activated, info.minimized), (1, "Firefox", true, false));
        assert_eq!(info.frames, [(10, rect(1800, 100, 400, 300)), (11, rect(-120, 100, 400, 300))]);
        assert_eq!(toplevels::global_rects(&info, &outs), [rect(1800, 100, 400, 300), rect(1800, 100, 400, 300)]);

        // Not the active one; hidden or unmapped counts as minimized.
        let mut hidden = raw(2, "Files", rect(0, 0, 100, 100));
        hidden.hidden = true;
        let mut unmapped = raw(3, "Files", rect(0, 0, 100, 100));
        unmapped.viewable = false;
        for window in [hidden, unmapped] {
            let info = window_info(&window, Some(1), &outs, 4242);
            assert!(info.minimized && !info.activated, "{window:?}");
        }
        // Off every output: no frame, so it owns no point.
        assert!(window_info(&raw(4, "Files", rect(9000, 0, 10, 10)), None, &outs, 4242).frames.is_empty());
    }

    #[test]
    fn the_frame_extents_widen_the_window_to_its_decorations() {
        let mut decorated = raw(1, "Files", rect(100, 100, 400, 300));
        decorated.extents = Some([1, 2, 30, 4]);
        let info = window_info(&decorated, None, &outputs(), 4242);
        assert_eq!(info.frames, [(10, rect(99, 70, 403, 334))]);
    }

    #[test]
    fn own_windows_are_found_by_pid_and_a_window_without_a_class_has_no_app() {
        let outs = outputs();
        let mut own = raw(1, "Whatever", rect(0, 0, 10, 10));
        own.pid = Some(4242);
        let info = window_info(&own, None, &outs, 4242);
        assert!(toplevels::is_own_app_id(&info.app_id), "{}", info.app_id);
        let mut anonymous = raw(2, "x", rect(0, 0, 10, 10));
        anonymous.wm_class.clear();
        assert_eq!(window_info(&anonymous, None, &outs, 4242).app_id, "");
        // No pid at all is not Gentle Dot.
        let mut unknown = raw(3, "Files", rect(0, 0, 10, 10));
        unknown.pid = None;
        assert_eq!(window_info(&unknown, None, &outs, 4242).app_id, "Files");
    }

    #[test]
    fn the_topmost_visible_window_owns_the_point() {
        let outs = outputs();
        let info = |id, class: &str, r| window_info(&raw(id, class, r), None, &outs, 4242);
        let mut covered = info(3, "Hidden", rect(0, 0, 3000, 1000));
        covered.minimized = true;
        // Bottom to top: the later window covers the earlier one.
        let stacked = [info(1, "Firefox", rect(0, 0, 1000, 1000)), info(2, "Files", rect(500, 500, 1000, 500)), covered];
        assert_eq!(topmost_at(&stacked, &outs, at(600.0, 600.0)).map(|w| w.id), Some(2));
        assert_eq!(topmost_at(&stacked, &outs, at(100.0, 100.0)).map(|w| w.id), Some(1));
        // Nothing there: fail closed.
        assert_eq!(topmost_at(&stacked, &outs, at(2000.0, 100.0)), None);
        assert_eq!(topmost_at(&[], &outs, at(1.0, 1.0)), None);
    }

    #[test]
    fn a_popup_above_a_window_is_an_owner_nobody_can_name() {
        let outs = outputs();
        let root = rect(0, 0, 3840, 1080);
        let child = |id, r| RootChild { id, rect: r, override_redirect: true, viewable: true, input_only: false, own: false };
        let managed = |id, class: &str, r, frame| (window_info(&raw(id, class, r), None, &outs, 4242), frame);
        // Bottom to top, as `QueryTree` lists the root's children.
        let children = [
            child(100, root),                           // 0: desktop or compositor overlay, covers the root
            RootChild { override_redirect: false, ..child(101, rect(0, 0, 1000, 1000)) }, // 1: the frame of window 1 (managed)
            child(102, rect(450, 450, 200, 200)),       // 2: a menu above window 1, below window 2
            RootChild { override_redirect: false, ..child(103, rect(500, 500, 1000, 500)) }, // 3: the frame of window 2 (managed)
            child(104, rect(600, 600, 50, 50)),         // 4: a tooltip above both
            RootChild { viewable: false, ..child(105, rect(0, 0, 3000, 1000)) },
            RootChild { input_only: true, ..child(106, rect(0, 0, 3000, 1000)) },
            RootChild { own: true, ..child(107, rect(0, 0, 3000, 1000)) },
            RootChild { override_redirect: false, ..child(108, rect(0, 0, 3000, 1000)) },
        ];
        let found = popups(&children, root, &outs);
        assert_eq!(found.iter().map(|(index, w)| (*index, w.id)).collect::<Vec<_>>(), [(2, 102), (4, 104)]);
        assert!(found.iter().all(|(_, w)| w.app_id.is_empty() && !w.minimized));
        let owner = |managed: Vec<_>, x, y| {
            let stacked = stack_with_popups(managed, found.clone());
            topmost_at(&stacked, &outs, at(x, y)).map(|w| (w.id, w.app_id.clone()))
        };
        let both = || vec![managed(1, "Firefox", rect(0, 0, 1000, 1000), Some(1)), managed(2, "Files", rect(500, 500, 1000, 500), Some(3))];
        // A point only the menu (and window 1 below it) holds: the menu, not the window below.
        assert_eq!(owner(both(), 460.0, 460.0), Some((102, String::new())));
        // Window 2 is above the menu: it owns what both hold.
        assert_eq!(owner(both(), 640.0, 640.0).map(|o| o.0), Some(104));
        assert_eq!(owner(both(), 520.0, 520.0), Some((2, "Files".into())));
        assert_eq!(owner(both(), 100.0, 100.0), Some((1, "Firefox".into())));
        // A window whose frame is unknown cannot be shown to cover the popup: the popup wins.
        let unknown = vec![managed(2, "Files", rect(0, 0, 1000, 1000), None)];
        assert_eq!(owner(unknown, 460.0, 460.0), Some((102, String::new())));
    }

    /// A map with letters on the first level, a shifted symbol, and free keycodes at the top.
    fn map() -> Mapping {
        // keycodes 8..=15, two keysyms each.
        let mut keysyms = vec![0; 8 * 2];
        let mut set = |keycode: usize, base: u32, shifted: u32| {
            keysyms[(keycode - 8) * 2] = base;
            keysyms[(keycode - 8) * 2 + 1] = shifted;
        };
        set(10, 'a' as u32, 'A' as u32);
        set(11, '1' as u32, '!' as u32);
        set(12, 0xff0d, 0xff0d); // Return
        set(13, 0xff09, 0xff09); // Tab
        set(14, SHIFT_L, 0);
        Mapping { first: 8, per: 2, keysyms }
    }

    fn key(keycode: u8, down: bool) -> KeyAction {
        KeyAction::Key { keycode, down }
    }

    fn taps(keycodes: &[u8]) -> Vec<KeyAction> {
        keycodes.iter().flat_map(|&k| [key(k, true), key(k, false)]).collect()
    }

    #[test]
    fn the_map_finds_keycodes_and_spares() {
        let m = map();
        assert_eq!((m.find(0x61), m.find(0x41), m.find(0x21), m.find(0x7a)), (Some((10, false)), Some((10, true)), Some((11, true)), None));
        // Free keycodes come from the top; a keycode with any symbol is taken.
        assert_eq!(m.spare(3), [15, 9, 8]);
        assert_eq!(m.spare(0), Vec::<u8>::new());
    }

    #[test]
    fn combos_hold_their_modifiers_around_the_key() {
        let m = map();
        // Ctrl is evdev 29 + 8; the key is found in the map, else its evdev position.
        let combo = parse_combo("cmd+shift+a").unwrap();
        assert_eq!(
            combo_actions(&combo, &m).unwrap(),
            [key(37, true), key(14, true), key(10, true), key(10, false), key(14, false), key(37, false)]
        );
        // `c` is not in this map: its evdev code (46) + 8.
        assert_eq!(combo_actions(&parse_combo("c").unwrap(), &m).unwrap(), taps(&[54]));
        // Named keys come through their keysym.
        assert_eq!(combo_actions(&parse_combo("return").unwrap(), &m).unwrap(), taps(&[12]));
        // `fn` has no key.
        assert_eq!(combo_actions(&parse_combo("fn+tab").unwrap(), &m).unwrap(), taps(&[13]));
    }

    #[test]
    fn text_uses_existing_keycodes_and_spares_only_for_the_rest() {
        let m = map();
        assert_eq!(text_actions("aA", &m).unwrap(), [key(10, true), key(10, false), key(14, true), key(10, true), key(10, false), key(14, false)]);
        assert_eq!(text_actions("a\n\t", &m).unwrap(), taps(&[10, 12, 13]));
        for crlf in ["\r\n", "\r"] {
            assert_eq!(text_actions(crlf, &m).unwrap(), taps(&[12]), "{crlf:?}");
        }
        // `é` is not in the map: spare keycode 15 is bound to its keysym for the chunk, then freed.
        let typed = text_actions("aé", &m).unwrap();
        assert_eq!(
            typed,
            [
                KeyAction::Remap(vec![(15, 0xe9)]),
                KeyAction::Pause(REMAP_SETTLE),
                key(10, true),
                key(10, false),
                key(15, true),
                key(15, false),
                KeyAction::Pause(REMAP_SETTLE),
                KeyAction::Remap(vec![(15, 0)]),
            ]
        );
        // A character past Latin-1 takes the Unicode keysym; one character is bound once.
        let twice = text_actions("你你", &m).unwrap();
        assert_eq!(twice[0], KeyAction::Remap(vec![(15, 0x0100_0000 + 0x4f60)]));
        assert_eq!(twice.iter().filter(|a| matches!(a, KeyAction::Key { down: true, .. })).count(), 2);
    }

    #[test]
    fn text_is_cut_into_chunks_by_the_spare_keycodes_and_refused_when_unsafe() {
        // Keycodes 8..=11 with one keysym each, 8 and 9 taken: two spares.
        let tight = Mapping { first: 8, per: 1, keysyms: vec![0x61, 0x62, 0, 0] };
        let typed = text_actions("éèêë", &tight).unwrap();
        let remaps: Vec<_> = typed.iter().filter_map(|a| if let KeyAction::Remap(r) = a { Some(r.len()) } else { None }).collect();
        // Bind two, free two, bind two, free two.
        assert_eq!(remaps, [2, 2, 2, 2]);
        let pressed = typed.iter().filter(|a| matches!(a, KeyAction::Key { down: true, .. })).count();
        assert_eq!(pressed, 4);
        // No free keycode at all: error, nothing sent.
        let full = Mapping { first: 8, per: 1, keysyms: vec![0x61, 0x62] };
        assert!(text_actions("é", &full).unwrap_err().contains("free keycode"));
        assert!(text_actions("a\u{7}", &map()).unwrap_err().contains("U+0007"));
        assert_eq!(text_actions("", &map()).unwrap(), []);
    }

    fn button(button: u8, down: bool) -> PointerAction {
        PointerAction::Button { button, down }
    }

    fn mv(x: i16, y: i16) -> PointerAction {
        PointerAction::Move { x, y }
    }

    #[test]
    fn pointer_events_become_xtest_motion_buttons_and_wheel_clicks() {
        let layout: Vec<Rect> = outputs().iter().map(|o| o.rect).collect();
        let target = at(2000.4, 100.6);
        let click = |button, count| InputEvent::Click { at: target, button, count };
        let actions = pointer_actions(&click(MouseButton::Right, 2), &layout).unwrap();
        let presses: Vec<_> = actions.iter().filter(|a| matches!(a, PointerAction::Button { .. })).cloned().collect();
        assert_eq!(actions[0], mv(2000, 101));
        assert_eq!(presses, [button(3, true), button(3, false), button(3, true), button(3, false)]);
        assert_eq!(pointer_actions(&InputEvent::Move(target), &layout).unwrap(), [mv(2000, 101)]);

        let drag = pointer_actions(&InputEvent::Drag { from: at(100.0, 100.0), to: at(300.0, 100.0) }, &layout).unwrap();
        let moves: Vec<_> = drag.iter().filter(|a| matches!(a, PointerAction::Move { .. })).cloned().collect();
        assert_eq!(moves, [mv(100, 100), mv(200, 100), mv(300, 100)]);
        let order: Vec<_> = drag.iter().filter(|a| !matches!(a, PointerAction::Pause(_))).collect();
        assert_eq!(order[1], &button(1, true));
        assert_eq!(order.last(), Some(&&button(1, false)));

        // Positive dy scrolls down (button 5), negative up (4); dx right (7), left (6).
        let scroll = |dx, dy| pointer_actions(&InputEvent::Scroll { at: target, dx, dy }, &layout).unwrap();
        let wheel = |actions: Vec<PointerAction>| -> Vec<PointerAction> {
            actions.into_iter().filter(|a| matches!(a, PointerAction::Button { down: true, .. })).collect()
        };
        assert_eq!(wheel(scroll(0, 80)), [button(5, true), button(5, true)]);
        assert_eq!(wheel(scroll(0, -5)), [button(4, true)]);
        assert_eq!(wheel(scroll(40, 0)), [button(7, true)]);
        assert_eq!(wheel(scroll(-40, 0)), [button(6, true)]);
        assert!(wheel(scroll(0, 0)).is_empty());
    }

    #[test]
    fn pointer_targets_outside_the_layout_and_other_events_are_refused() {
        let layout: Vec<Rect> = outputs().iter().map(|o| o.rect).collect();
        let outside = at(5000.0, 5.0);
        for event in [
            InputEvent::Move(outside),
            InputEvent::Click { at: outside, button: MouseButton::Left, count: 1 },
            InputEvent::Drag { from: at(1.0, 1.0), to: outside },
            InputEvent::Scroll { at: outside, dx: 0, dy: 5 },
            InputEvent::Click { at: at(1.0, 1.0), button: MouseButton::Left, count: 4 },
            InputEvent::Text("a".into()),
        ] {
            assert!(pointer_actions(&event, &layout).is_err(), "{event:?}");
        }
    }

    #[test]
    fn the_pixels_of_a_get_image_reply_become_a_jpeg_of_the_output() {
        let output = &outputs()[0];
        let format = PixelFormat { bits_per_pixel: 32, red_mask: 0xff0000, green_mask: 0xff00, blue_mask: 0xff, msb_first: false };
        // 1920 × 1080 of blue (B, G, R, unused).
        let blue: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 0], 1920 * 1080).flatten().collect();
        let capture = capture_of_pixels(&blue, format, output).unwrap();
        assert_eq!((capture.geometry.image_width, capture.geometry.image_height), (1280, 720));
        assert_eq!(capture.geometry.origin, Point { x: 0.0, y: 0.0 });
        let decoded = image::load_from_memory_with_format(&capture.data, image::ImageFormat::Jpeg).unwrap().to_rgb8();
        let [r, g, b] = decoded.get_pixel(10, 10).0;
        assert!(b > 200 && r < 60 && g < 60, "blue stays blue: {r} {g} {b}");

        // A layout the conversion does not know, or a reply of the wrong size, is refused.
        for (data, bad) in [
            (&blue[..], PixelFormat { bits_per_pixel: 16, ..format }),
            (&blue[..], PixelFormat { red_mask: 0xff, blue_mask: 0xff0000, ..format }),
            (&blue[..], PixelFormat { msb_first: true, ..format }),
            (&blue[..100], format),
        ] {
            assert!(capture_of_pixels(data, bad, output).is_err(), "{bad:?} {}", data.len());
        }
    }
}
