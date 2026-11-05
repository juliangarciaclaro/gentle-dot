//! Which kind of graphical session the app runs in (S2): Wayland when `WAYLAND_DISPLAY` is set,
//! else X11 when `DISPLAY` is set.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Wayland,
    X11,
    None,
}

pub fn detect(env: impl Fn(&str) -> Option<String>) -> Session {
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    if set("WAYLAND_DISPLAY") {
        Session::Wayland
    } else if set("DISPLAY") {
        Session::X11
    } else {
        Session::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_wins_over_x11_and_empty_values_do_not_count() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
        };
        assert_eq!(detect(env(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":1")])), Session::Wayland);
        assert_eq!(detect(env(&[("DISPLAY", ":0")])), Session::X11);
        assert_eq!(detect(env(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", ":0")])), Session::X11);
        assert_eq!(detect(env(&[])), Session::None);
    }
}
