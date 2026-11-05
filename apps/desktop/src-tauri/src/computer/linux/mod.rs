//! The Linux desktop (S2). The pure parts are tested on every OS: evdev key codes for the `keys`
//! names, the xkb keymap for the Wayland virtual keyboard, the uinput pointer path planner,
//! `.desktop` entries and the app catalog, the window and output decisions, screenshot cropping
//! and the portal's answers, what the virtual keyboard is sent, and the uinput events for each
//! pointer action. `LinuxDesktop` joins them
//! with a Wayland client and the screenshot portal on Linux.

pub mod apps;
pub mod atspi;
pub mod desktop_entry;
pub mod evdev;
pub mod keymap;
pub mod pointer;
pub mod session;
pub mod shot;
pub mod toplevels;
pub mod typing;
pub mod uinput;

#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "linux")]
pub use desktop::{request_permission, startup_pointer, CaptureGuard, FullControl, LinuxDesktop, Pointer, OVERLAY_GONE};
#[cfg(target_os = "linux")]
pub use uinput::{device_writable, UinputPointer};
