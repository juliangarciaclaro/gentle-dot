//! The opt-in pointer: a pair of `/dev/uinput` devices (S2). Everything that decides what is
//! sent is pure and tested here: `actions` turns a pointer `InputEvent` into evdev frames and
//! pauses, using the path planner. `UinputPointer` (Linux only) opens the devices with raw
//! ioctls and plays the actions. It is opened only after the user turned on full desktop control.

use super::pointer::{plan, plan_from, Rect, Step};
use crate::computer::coords::Point;
use crate::computer::{InputEvent, MouseButton};
use std::time::Duration;

/// One evdev event: type, code, value.
pub type Event = (u16, u16, i32);

/// The two devices: absolute motion maps onto the output under the pointer, relative motion
/// crosses outputs (L4). Buttons and the wheel go through the relative one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Absolute,
    Relative,
}

/// Events delivered together, followed by a `SYN_REPORT` when written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub device: Device,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Frame(Frame),
    Pause(Duration),
}

// linux/input-event-codes.h
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
/// The absolute axes run 0..=ABS_RANGE across the output under the pointer.
const ABS_RANGE: i32 = 65535;

/// `_IOC` from `linux/ioctl.h` for the uinput magic 'U'.
const fn request(direction: u32, number: u32, size: u32) -> u32 {
    direction << 30 | size << 16 | (b'U' as u32) << 8 | number
}
const IOCTL_SET_EVBIT: u32 = request(1, 100, 4);
const IOCTL_SET_KEYBIT: u32 = request(1, 101, 4);
const IOCTL_SET_RELBIT: u32 = request(1, 102, 4);
const IOCTL_SET_ABSBIT: u32 = request(1, 103, 4);
const IOCTL_DEV_SETUP: u32 = request(1, 3, 92);
const IOCTL_ABS_SETUP: u32 = request(1, 4, 28);
const IOCTL_DEV_CREATE: u32 = request(0, 1, 0);
const IOCTL_DEV_DESTROY: u32 = request(0, 2, 0);

/// Wait after a frame when the next goes to the other device: the compositor reads each device
/// on its own, so an absolute move must not overtake the pushes before it.
const SETTLE: Duration = Duration::from_millis(5);
/// Wait for the pointer to arrive before a button or the wheel acts.
const ARRIVE: Duration = Duration::from_millis(30);
/// Wait between the presses and releases of a click.
const CLICK_GAP: Duration = Duration::from_millis(30);
/// Wait between the motion events of a drag, which apps need to see the item move.
const DRAG_GAP: Duration = Duration::from_millis(20);
/// A double or triple click is the most an app understands.
const MAX_CLICKS: u8 = 3;
/// One wheel notch is this many pixels of scroll; the wheel is capped so one call cannot spin.
const PIXELS_PER_NOTCH: u32 = 40;
const MAX_NOTCHES: i32 = 50;
const HI_RES_PER_NOTCH: i32 = 120;

fn fraction(value: f64) -> i32 {
    (value.clamp(0.0, 1.0) * f64::from(ABS_RANGE)).round() as i32
}

fn step_frame(step: &Step) -> Frame {
    match *step {
        Step::Relative { dx, dy } => {
            Frame { device: Device::Relative, events: vec![(EV_REL, REL_X, dx), (EV_REL, REL_Y, dy)] }
        }
        Step::Absolute { x, y } => {
            Frame { device: Device::Absolute, events: vec![(EV_ABS, ABS_X, fraction(x)), (EV_ABS, ABS_Y, fraction(y))] }
        }
    }
}

/// The planner steps as frames; `hold` is the wait after each (a drag), else the devices settle
/// when the next frame goes to the other one.
fn steps_actions(steps: &[Step], hold: Option<Duration>) -> Vec<Action> {
    let frames: Vec<Frame> = steps.iter().map(step_frame).collect();
    let mut actions = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let switches = frames.get(index + 1).is_some_and(|next| next.device != frame.device);
        actions.push(Action::Frame(frame.clone()));
        if let Some(gap) = hold {
            actions.push(Action::Pause(gap));
        } else if switches {
            actions.push(Action::Pause(SETTLE));
        }
    }
    actions
}

fn button_frame(button: MouseButton, down: bool) -> Action {
    let code = match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
    };
    Action::Frame(Frame { device: Device::Relative, events: vec![(EV_KEY, code, i32::from(down))] })
}

/// Wheel notches for a pixel distance: at least one for any scroll, at most `MAX_NOTCHES`.
fn notches(pixels: i32) -> i32 {
    if pixels == 0 {
        return 0;
    }
    let count = (pixels.unsigned_abs().saturating_add(PIXELS_PER_NOTCH / 2) / PIXELS_PER_NOTCH).clamp(1, MAX_NOTCHES as u32);
    count as i32 * pixels.signum()
}

fn wheel_events(events: &mut Vec<Event>, hi_res: u16, legacy: u16, notches: i32) {
    if notches != 0 {
        events.push((EV_REL, hi_res, notches * HI_RES_PER_NOTCH));
        events.push((EV_REL, legacy, notches));
    }
}

/// What a pointer `InputEvent` becomes. Errors (and sends nothing) for an event that is not a
/// pointer action, a target outside the layout, or a click count the device refuses.
pub fn actions(event: &InputEvent, outputs: &[Rect]) -> Result<Vec<Action>, String> {
    match event {
        InputEvent::Move(at) => Ok(steps_actions(&plan(outputs, *at)?, None)),
        InputEvent::Click { at, button, count } => {
            if !(1..=MAX_CLICKS).contains(count) {
                return Err(format!("a click of {count} presses is not supported"));
            }
            let mut actions = steps_actions(&plan(outputs, *at)?, None);
            actions.push(Action::Pause(ARRIVE));
            for _ in 0..*count {
                actions.extend([button_frame(*button, true), Action::Pause(CLICK_GAP)]);
                actions.extend([button_frame(*button, false), Action::Pause(CLICK_GAP)]);
            }
            Ok(actions)
        }
        InputEvent::Drag { from, to } => {
            let mut actions = steps_actions(&plan(outputs, *from)?, None);
            // From the known start: no pushes to the corner while the button is held. Halfway
            // first, so the app sees the item travel, unless the halfway point is in no output.
            let half = Point { x: (from.x + to.x) / 2.0, y: (from.y + to.y) / 2.0 };
            let path = plan_from(outputs, *from, half)
                .and_then(|first| plan_from(outputs, half, *to).map(|second| [first, second].concat()))
                .or_else(|_| plan_from(outputs, *from, *to))?;
            actions.extend([Action::Pause(ARRIVE), button_frame(MouseButton::Left, true), Action::Pause(DRAG_GAP)]);
            actions.extend(steps_actions(&path, Some(DRAG_GAP)));
            actions.push(button_frame(MouseButton::Left, false));
            Ok(actions)
        }
        InputEvent::Scroll { at, dx, dy } => {
            let mut actions = steps_actions(&plan(outputs, *at)?, None);
            let mut events = Vec::new();
            // Positive `dy` scrolls down and positive `dx` right; the wheel counts up and right.
            wheel_events(&mut events, REL_HWHEEL_HI_RES, REL_HWHEEL, notches(*dx));
            wheel_events(&mut events, REL_WHEEL_HI_RES, REL_WHEEL, -notches(*dy));
            if !events.is_empty() {
                actions.push(Action::Pause(ARRIVE));
                actions.push(Action::Frame(Frame { device: Device::Relative, events }));
            }
            Ok(actions)
        }
        InputEvent::Key(_) | InputEvent::Text(_) => Err("the pointer device does not type".into()),
    }
}

/// The bytes `write(2)` takes for one frame: its events and a closing `SYN_REPORT`.
pub fn frame_bytes(frame: &Frame) -> Vec<u8> {
    let mut bytes = Vec::new();
    for &(kind, code, value) in frame.events.iter().chain([&(EV_SYN, 0, 0)]) {
        // `struct input_event`: a timeval the kernel fills in, then type, code, value.
        bytes.extend_from_slice(&[0; 2 * std::mem::size_of::<std::ffi::c_long>()]);
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&code.to_ne_bytes());
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Where the pointer ends after a successful event, when it is known.
fn ends_at(event: &InputEvent) -> Option<Point> {
    match event {
        InputEvent::Move(at) | InputEvent::Click { at, .. } | InputEvent::Scroll { at, .. } => Some(*at),
        InputEvent::Drag { to, .. } => Some(*to),
        InputEvent::Key(_) | InputEvent::Text(_) => None,
    }
}

/// Where the pointer last went, and the turn that keeps two actions from interleaving.
#[derive(Default)]
struct Played {
    /// Held for a whole action (its pauses included): the only thing it blocks is another action.
    turn: std::sync::Mutex<()>,
    last: std::sync::Mutex<Option<Point>>,
}

impl Played {
    /// Plays one action; the point it ends at is remembered only when it played through.
    fn run(&self, play: impl FnOnce() -> Result<(), String>, ends: Option<Point>) -> Result<(), String> {
        let _turn = self.turn.lock().unwrap_or_else(|p| p.into_inner());
        play()?;
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = ends;
        Ok(())
    }

    fn position(&self) -> Option<Point> {
        *self.last.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(target_os = "linux")]
pub use device::{device_writable, UinputPointer};

#[cfg(target_os = "linux")]
mod device {
    use super::*;
    use crate::computer::linux::Pointer;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    const PATH: &str = "/dev/uinput";
    const BUS_VIRTUAL: u16 = 6;
    /// The compositor needs a moment to notice a new device before it hears from it.
    const ENUMERATE: Duration = Duration::from_millis(400);

    #[repr(C)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }

    #[repr(C)]
    struct Setup {
        id: InputId,
        name: [u8; 80],
        ff_effects_max: u32,
    }

    #[repr(C)]
    struct AbsInfo {
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    }

    #[repr(C)]
    struct AbsSetup {
        code: u16,
        info: AbsInfo,
    }

    const _: () = assert!(std::mem::size_of::<Setup>() == 92 && std::mem::size_of::<AbsSetup>() == 28);

    /// Whether this process may write `/dev/uinput`, without opening it.
    pub fn device_writable() -> bool {
        let path = std::ffi::CString::new(PATH).expect("no NUL in the path");
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }

    fn ioctl_int(file: &File, request: u32, value: u16) -> Result<(), String> {
        // SAFETY: the fd is open for the call and these requests take an int by value.
        let done = unsafe { libc::ioctl(file.as_raw_fd(), request as _, libc::c_int::from(value)) };
        (done >= 0).then_some(()).ok_or_else(|| format!("uinput ioctl {request:#x}: {}", std::io::Error::last_os_error()))
    }

    fn ioctl_ref<T>(file: &File, request: u32, arg: &T) -> Result<(), String> {
        // SAFETY: the fd is open, and `arg` is a live `repr(C)` struct of the size `request` names.
        let done = unsafe { libc::ioctl(file.as_raw_fd(), request as _, std::ptr::from_ref(arg)) };
        (done >= 0).then_some(()).ok_or_else(|| format!("uinput ioctl {request:#x}: {}", std::io::Error::last_os_error()))
    }

    /// One virtual input device, destroyed when dropped.
    struct Dev(File);

    impl Dev {
        fn create(kind: Device) -> Result<Dev, String> {
            let file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(PATH)
                .map_err(|e| format!("cannot open {PATH}: {e}"))?;
            // From here the device is destroyed on any failure.
            let dev = Dev(file);
            let file = &dev.0;
            for button in [BTN_LEFT, BTN_RIGHT] {
                ioctl_int(file, IOCTL_SET_EVBIT, EV_KEY)?;
                ioctl_int(file, IOCTL_SET_KEYBIT, button)?;
            }
            match kind {
                Device::Relative => {
                    ioctl_int(file, IOCTL_SET_EVBIT, EV_REL)?;
                    for axis in [REL_X, REL_Y, REL_HWHEEL, REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                        ioctl_int(file, IOCTL_SET_RELBIT, axis)?;
                    }
                }
                Device::Absolute => {
                    ioctl_int(file, IOCTL_SET_EVBIT, EV_ABS)?;
                    for axis in [ABS_X, ABS_Y] {
                        ioctl_int(file, IOCTL_SET_ABSBIT, axis)?;
                        let info = AbsInfo { value: 0, minimum: 0, maximum: ABS_RANGE, fuzz: 0, flat: 0, resolution: 0 };
                        ioctl_ref(file, IOCTL_ABS_SETUP, &AbsSetup { code: axis, info })?;
                    }
                }
            }
            let label = match kind {
                Device::Relative => &b"Gentle Dot pointer (relative)"[..],
                Device::Absolute => &b"Gentle Dot pointer (absolute)"[..],
            };
            let mut name = [0u8; 80];
            name[..label.len()].copy_from_slice(label);
            let id = InputId { bustype: BUS_VIRTUAL, vendor: 0x6764, product: 0x0001, version: 1 };
            ioctl_ref(file, IOCTL_DEV_SETUP, &Setup { id, name, ff_effects_max: 0 })?;
            // SAFETY: the fd is open and this request takes no argument.
            if unsafe { libc::ioctl(file.as_raw_fd(), IOCTL_DEV_CREATE as _) } < 0 {
                return Err(format!("cannot create the uinput device: {}", std::io::Error::last_os_error()));
            }
            Ok(dev)
        }

        fn send(&self, frame: &Frame) -> Result<(), String> {
            (&self.0).write_all(&frame_bytes(frame)).map_err(|e| format!("cannot write to the pointer device: {e}"))
        }
    }

    impl Drop for Dev {
        fn drop(&mut self) {
            // SAFETY: the fd is open and this request takes no argument; failing is harmless
            // (closing the fd destroys the device as well).
            unsafe { libc::ioctl(self.0.as_raw_fd(), IOCTL_DEV_DESTROY as _) };
        }
    }

    /// The opt-in pointer. Open it only after the user turned on full desktop control.
    pub struct UinputPointer {
        absolute: Dev,
        relative: Dev,
        /// Serializes events (two never interleave) and holds the last point reached.
        played: Played,
    }

    impl UinputPointer {
        pub fn open() -> Result<Self, String> {
            let absolute = Dev::create(Device::Absolute)?;
            let relative = Dev::create(Device::Relative)?;
            std::thread::sleep(ENUMERATE);
            Ok(UinputPointer { absolute, relative, played: Played::default() })
        }

        fn play(&self, actions: &[Action]) -> Result<(), String> {
            for action in actions {
                match action {
                    Action::Frame(frame) => match frame.device {
                        Device::Absolute => self.absolute.send(frame)?,
                        Device::Relative => self.relative.send(frame)?,
                    },
                    Action::Pause(wait) => std::thread::sleep(*wait),
                }
            }
            Ok(())
        }
    }

    impl Pointer for UinputPointer {
        fn post(&self, event: &InputEvent, outputs: &[Rect]) -> Result<(), String> {
            // Everything is checked before the first event, so a refusal sends nothing.
            let actions = actions(event, outputs)?;
            self.played.run(
                || {
                    let played = self.play(&actions);
                    if played.is_err() {
                        // Never leave a button held down.
                        for button in [MouseButton::Left, MouseButton::Right] {
                            if let Action::Frame(frame) = button_frame(button, false) {
                                let _ = self.relative.send(&frame);
                            }
                        }
                    }
                    played
                },
                ends_at(event),
            )
        }

        fn position(&self) -> Option<Point> {
            self.played.position()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDMI: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    const EDP: Rect = Rect { x: 1920, y: 0, width: 1920, height: 1080 };

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn frames(actions: &[Action]) -> Vec<&Frame> {
        actions.iter().filter_map(|a| if let Action::Frame(f) = a { Some(f) } else { None }).collect()
    }

    fn buttons(actions: &[Action]) -> Vec<(u16, i32)> {
        frames(actions)
            .iter()
            .flat_map(|f| f.events.iter().filter(|e| e.0 == 1).map(|e| (e.1, e.2)))
            .collect()
    }

    #[test]
    fn a_playing_action_does_not_block_readers_but_actions_stay_serialized() {
        use std::sync::mpsc::{channel, sync_channel};
        use std::sync::Arc;
        let played = Arc::new(Played::default());
        let (started, running) = sync_channel::<()>(0);
        let (release, gate) = channel::<()>();
        let first = {
            let played = played.clone();
            std::thread::spawn(move || {
                played.run(move || {
                    started.send(()).unwrap();
                    gate.recv().ok();
                    Ok(())
                }, Some(at(1.0, 1.0)))
            })
        };
        running.recv().unwrap();
        // The position is readable while the action plays.
        let (seen, position) = channel();
        let reader = played.clone();
        std::thread::spawn(move || seen.send(reader.position()).ok());
        assert_eq!(position.recv_timeout(Duration::from_secs(2)), Ok(None), "position blocked during play");
        // A second action waits for the first, so they never interleave.
        let (entered, second_in) = channel();
        let second = {
            let played = played.clone();
            std::thread::spawn(move || played.run(move || { entered.send(()).unwrap(); Ok(()) }, Some(at(2.0, 2.0))))
        };
        assert!(second_in.recv_timeout(Duration::from_millis(100)).is_err(), "two actions interleaved");
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(second.join().unwrap(), Ok(()));
        assert_eq!(played.position(), Some(at(2.0, 2.0)));
        // A failed action leaves the last point as it was.
        assert!(played.run(|| Err("write failed".into()), Some(at(9.0, 9.0))).is_err());
        assert_eq!(played.position(), Some(at(2.0, 2.0)));
    }

    #[test]
    fn a_move_plays_the_planner_steps_on_the_matching_devices() {
        let actions = actions(&InputEvent::Move(at(2070.0, 300.0)), &[HDMI, EDP]).unwrap();
        let frames = frames(&actions);
        assert_eq!(frames.len(), 83);
        assert_eq!(frames[0], &Frame { device: Device::Relative, events: vec![(2, 0, -400), (2, 1, 0)] });
        // Absolute x/y are 0..=65535 fractions of the output under the pointer.
        let last = frames[82];
        assert_eq!(last.device, Device::Absolute);
        assert_eq!(last.events[0], (3, 0, (150.0_f64 / 1919.0 * 65535.0).round() as i32));
        assert_eq!(last.events[1], (3, 1, (300.0_f64 / 1079.0 * 65535.0).round() as i32));
        // The compositor reads each device on its own, so switching device waits a moment.
        assert!(actions.contains(&Action::Pause(SETTLE)));
        for pair in actions.windows(2) {
            if let [Action::Frame(a), Action::Frame(b)] = pair {
                assert_eq!(a.device, b.device, "a device switch without a pause");
            }
        }
    }

    #[test]
    fn a_click_moves_then_presses_and_releases_the_button_count_times() {
        let click = |button, count| InputEvent::Click { at: at(100.0, 100.0), button, count };
        let single = actions(&click(MouseButton::Left, 1), &[HDMI]).unwrap();
        assert_eq!(buttons(&single), [(BTN_LEFT, 1), (BTN_LEFT, 0)]);
        let double = actions(&click(MouseButton::Right, 2), &[HDMI]).unwrap();
        assert_eq!(buttons(&double), [(BTN_RIGHT, 1), (BTN_RIGHT, 0), (BTN_RIGHT, 1), (BTN_RIGHT, 0)]);
        // Never press before the pointer is there.
        let first_press = frames(&single).iter().position(|f| f.events.iter().any(|e| e.0 == 1)).unwrap();
        assert!(first_press >= 81);
        for count in [0, 4] {
            assert!(actions(&click(MouseButton::Left, count), &[HDMI]).is_err(), "count {count}");
        }
    }

    #[test]
    fn a_drag_holds_the_button_without_going_back_to_the_corner() {
        let drag = InputEvent::Drag { from: at(500.0, 500.0), to: at(2500.0, 700.0) };
        let actions = actions(&drag, &[HDMI, EDP]).unwrap();
        assert_eq!(buttons(&actions), [(BTN_LEFT, 1), (BTN_LEFT, 0)]);
        let frames = frames(&actions);
        let down = frames.iter().position(|f| f.events.contains(&(1, BTN_LEFT, 1))).unwrap();
        let up = frames.iter().position(|f| f.events.contains(&(1, BTN_LEFT, 0))).unwrap();
        assert!(up > down + 1);
        // While held, no relative push (a push would drag the item to the layout corner).
        assert!(frames[down..up].iter().all(|f| f.events.iter().all(|e| e.0 != 2 || e.2.abs() < 400)));
    }

    #[test]
    fn a_scroll_moves_then_turns_the_wheel_down_and_right_for_positive_values() {
        let scroll = |dx, dy| InputEvent::Scroll { at: at(100.0, 100.0), dx, dy };
        let events = |actions: &[Action]| frames(actions).last().unwrap().events.clone();
        let down = actions(&scroll(0, 80), &[HDMI]).unwrap();
        assert_eq!(events(&down), [(2, 0x0b, -240), (2, 8, -2)]);
        let right_up = actions(&scroll(40, -1), &[HDMI]).unwrap();
        assert_eq!(events(&right_up), [(2, 0x0c, 120), (2, 6, 1), (2, 0x0b, 120), (2, 8, 1)]);
        // A huge value is capped.
        let big = actions(&scroll(0, 1_000_000), &[HDMI]).unwrap();
        assert_eq!(events(&big), [(2, 0x0b, -120 * 50), (2, 8, -50)]);
    }

    #[test]
    fn anything_that_is_not_a_reachable_pointer_action_sends_nothing() {
        assert!(actions(&InputEvent::Text("hi".into()), &[HDMI]).is_err());
        assert!(actions(&InputEvent::Move(at(5000.0, 5.0)), &[HDMI, EDP]).is_err());
        assert!(actions(&InputEvent::Move(at(f64::NAN, 5.0)), &[HDMI]).is_err());
        assert!(actions(&InputEvent::Move(at(5.0, 5.0)), &[]).is_err());
    }

    #[test]
    fn a_frame_is_its_events_then_a_sync_report() {
        let bytes = frame_bytes(&Frame { device: Device::Relative, events: vec![(1, 0x110, 1), (2, 0, -5)] });
        let size = 2 * std::mem::size_of::<std::ffi::c_long>() + 8;
        assert_eq!(bytes.len(), 3 * size);
        let tail = |n: usize| &bytes[n * size + size - 8..(n + 1) * size];
        assert_eq!(tail(0), [1, 0, 0x10, 0x01, 1, 0, 0, 0]);
        assert_eq!(tail(1), [2, 0, 0, 0, 0xfb, 0xff, 0xff, 0xff]);
        assert_eq!(tail(2), [0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(bytes[..size - 8].iter().all(|b| *b == 0));
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(IOCTL_SET_EVBIT, 0x4004_5564);
        assert_eq!(IOCTL_DEV_SETUP, 0x405c_5503);
        assert_eq!(IOCTL_ABS_SETUP, 0x401c_5504);
        assert_eq!(IOCTL_DEV_CREATE, 0x5501);
        assert_eq!(IOCTL_DEV_DESTROY, 0x5502);
    }
}
