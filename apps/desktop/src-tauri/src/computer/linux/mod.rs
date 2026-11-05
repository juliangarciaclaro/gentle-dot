//! The Linux desktop's pure parts (S2), tested on every OS: evdev key codes for the `keys`
//! names, the xkb keymap for the Wayland virtual keyboard, the uinput pointer path planner,
//! and `.desktop` entry parsing. The real backend lives beside them once it is wired in.

pub mod desktop_entry;
pub mod evdev;
pub mod keymap;
pub mod pointer;
