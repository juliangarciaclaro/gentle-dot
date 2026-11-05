//! Accessibility on Linux (S2): what AT-SPI reports about an element, in the Accessibility terms
//! the risk check reads (S24.4), and the walks over the accessible tree. Plain mapping and
//! generic walks, tested on every OS; the D-Bus side reads the elements.
//!
//! A role name becomes the macOS role with the same meaning (a push button is `AXButton`, a link
//! `AXLink`, an entry `AXTextField`). The accessible's name is its title, its description its
//! description, and an editable text's contents its value. A password field is an `AXTextField`
//! whose value is never read, as on macOS and Windows. The window's default button is the one
//! with the `is-default` state.

use crate::computer::risk::{AxElement, Focus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// AT-SPI state numbers (`AtspiStateType`).
pub const STATE_ACTIVE: u32 = 1;
pub const STATE_DEFUNCT: u32 = 6;
pub const STATE_EDITABLE: u32 = 7;
pub const STATE_FOCUSED: u32 = 12;
pub const STATE_IS_DEFAULT: u32 = 39;

/// How many ancestors above the element are read, as on macOS.
const AX_ANCESTORS: usize = 4;
/// How deep a point is followed into the tree.
const MAX_DEPTH: usize = 24;
/// How many accessibles a search for a state may look at, and how deep.
const SEARCH_NODES: usize = 400;
const SEARCH_DEPTH: usize = 16;

/// What AT-SPI reports about one accessible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Raw {
    pub role_name: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// The state set as AT-SPI sends it: bit `n` of the 64-bit set is state `n`.
    pub states: Vec<u32>,
    pub value: Option<String>,
}

/// A top-level accessible of an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub name: String,
    pub active: bool,
}

/// The accessible tree, apart from D-Bus so the walks are tested.
pub trait Tree {
    type Node: Clone + PartialEq;
    fn describe(&self, node: &Self::Node) -> Option<Raw>;
    fn states(&self, node: &Self::Node) -> Option<Vec<u32>>;
    fn parent(&self, node: &Self::Node) -> Option<Self::Node>;
    /// The child of `node` at a point in window coordinates, when it has one.
    fn child_at(&self, node: &Self::Node, point: (i32, i32)) -> Option<Self::Node>;
    fn children(&self, node: &Self::Node) -> Vec<Self::Node>;
    /// The top-level accessibles of every application.
    fn windows(&self) -> Vec<(Self::Node, Window)>;
    /// The time for this query is up.
    fn expired(&self) -> bool {
        false
    }
}

pub fn has_state(states: &[u32], state: u32) -> bool {
    states.get((state / 32) as usize).is_some_and(|word| word & (1 << (state % 32)) != 0)
}

/// The Accessibility role for an AT-SPI role name. A plain text is a text area only while editable.
pub fn role(role_name: &str, editable: bool) -> &'static str {
    match role_name {
        "push button" | "toggle button" => "AXButton",
        "menu item" | "check menu item" | "radio menu item" | "tearoff menu item" => "AXMenuItem",
        "menu" => "AXMenuBarItem",
        "link" => "AXLink",
        "entry" | "password text" | "spin button" => "AXTextField",
        "text" if editable => "AXTextArea",
        "text" | "label" => "AXStaticText",
        "combo box" => "AXComboBox",
        "check box" => "AXCheckBox",
        "radio button" => "AXRadioButton",
        "page tab" => "AXTab",
        "list item" => "AXRow",
        "panel" | "filler" | "section" | "scroll pane" | "viewport" | "split pane" | "tool bar" => "AXGroup",
        "frame" | "window" | "dialog" | "alert" => "AXWindow",
        _ => "AXUnknown",
    }
}

/// Whether the accessible's contents are read as its value: editable text only, never a password.
pub fn reads_value(role_name: &str, editable: bool) -> bool {
    editable && matches!(role_name, "entry" | "text")
}

/// An element from what AT-SPI reports; blank strings count as absent.
pub fn element(raw: &Raw) -> AxElement {
    let present = |text: &Option<String>| text.clone().filter(|t| !t.trim().is_empty());
    let editable = has_state(&raw.states, STATE_EDITABLE);
    AxElement {
        role: role(&raw.role_name, editable).into(),
        title: present(&raw.name),
        description: present(&raw.description),
        value: if reads_value(&raw.role_name, editable) { present(&raw.value) } else { None },
    }
}

/// The focus from the focused accessible and its window's default button; a focused default
/// button is its own.
pub fn focus(focused: &Raw, default: Option<&Raw>) -> Focus {
    let is_default_button = has_state(&focused.states, STATE_IS_DEFAULT);
    let default = if is_default_button { Some(focused) } else { default };
    Focus { element: element(focused), is_default_button, default_button: default.map(element) }
}

/// The window an element is looked up in: the only one with the owner's title, else (when the
/// owner is the active window) the only active one. Several candidates are not known.
pub fn pick_window(windows: &[Window], title: &str, owner_active: bool) -> Option<usize> {
    let only = |matching: Vec<usize>| match matching.as_slice() {
        [one] => Some(Some(*one)),
        [] => None,
        _ => Some(None),
    };
    let named = (0..windows.len()).filter(|&i| !title.is_empty() && windows[i].name == title).collect();
    if let Some(found) = only(named) {
        return found;
    }
    if !owner_active {
        return None;
    }
    only((0..windows.len()).filter(|&i| windows[i].active).collect()).flatten()
}

/// `at` in a window's coordinates, for the first of the window's frames that holds it.
pub fn window_point(at: (f64, f64), frames: &[super::pointer::Rect]) -> Option<(i32, i32)> {
    let holds = |f: &&super::pointer::Rect| {
        at.0 >= f64::from(f.x) && at.1 >= f64::from(f.y) && at.0 < f64::from(f.x + f.width) && at.1 < f64::from(f.y + f.height)
    };
    let frame = frames.iter().find(holds)?;
    Some(((at.0 - f64::from(frame.x)).round() as i32, (at.1 - f64::from(frame.y)).round() as i32))
}

/// The element at a point of `window`, then its ancestors (as many as macOS reads); empty when
/// unknown. The point is followed down to the deepest accessible that holds it.
pub fn element_at<T: Tree>(tree: &T, window: &T::Node, point: (i32, i32)) -> Vec<AxElement> {
    let mut current = window.clone();
    for _ in 0..MAX_DEPTH {
        match tree.child_at(&current, point) {
            Some(child) if child != current && !tree.expired() => current = child,
            _ => break,
        }
    }
    let Some(raw) = tree.describe(&current) else { return Vec::new() };
    let mut chain = vec![element(&raw)];
    while chain.len() <= AX_ANCESTORS && !tree.expired() {
        let Some(parent) = tree.parent(&current) else { break };
        let Some(raw) = tree.describe(&parent) else { break };
        chain.push(element(&raw));
        current = parent;
    }
    chain
}

/// The first accessible under `root` (breadth first, within the node and depth limits) with
/// `state`, and what it reports.
pub fn find_state<T: Tree>(tree: &T, root: &T::Node, state: u32) -> Option<(T::Node, Raw)> {
    let mut queue = std::collections::VecDeque::from([(root.clone(), 0)]);
    let mut seen = 0;
    while let Some((node, depth)) = queue.pop_front() {
        if seen == SEARCH_NODES || tree.expired() {
            return None;
        }
        seen += 1;
        if tree.states(&node).is_some_and(|states| has_state(&states, state)) {
            let raw = tree.describe(&node)?;
            return Some((node, raw));
        }
        if depth < SEARCH_DEPTH {
            queue.extend(tree.children(&node).into_iter().map(|child| (child, depth + 1)));
        }
    }
    None
}

/// The focused element of `window` and the window's default button.
pub fn focused<T: Tree>(tree: &T, window: &T::Node) -> Option<Focus> {
    let (_, focused) = find_state(tree, window, STATE_FOCUSED)?;
    let default = if has_state(&focused.states, STATE_IS_DEFAULT) {
        None
    } else {
        find_state(tree, window, STATE_IS_DEFAULT).map(|(_, raw)| raw)
    };
    Some(focus(&focused, default.as_ref()))
}

/// How long one query may take, and how long it walks before it stops by itself: a stuck app
/// must not stall the helper.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const WALK_BUDGET: Duration = Duration::from_millis(400);
/// How many queries may be stuck at once; more are refused instead of piling up threads.
const MAX_WORKERS: usize = 3;

/// Counts the threads of queries that have not finished.
#[derive(Default, Clone)]
pub struct Workers(Arc<AtomicUsize>);

struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs `work` on a thread of its own and waits at most `limit` for its answer. `None` when it
/// did not answer in time or `cap` earlier queries are still running; a late answer is dropped.
pub fn within<T: Send + 'static>(
    workers: &Workers,
    cap: usize,
    limit: Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    if workers.0.fetch_add(1, Ordering::AcqRel) >= cap {
        workers.0.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    let live = Live(workers.0.clone());
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("computer-atspi".into())
        .spawn(move || {
            let _live = live;
            let _ = send.send(work());
        })
        .ok()?;
    receive.recv_timeout(limit).ok().flatten()
}

#[cfg(target_os = "linux")]
mod dbus {
    use super::*;
    use zbus::blocking::{connection, Connection};
    use zbus::export::serde::{de::DeserializeOwned, Serialize};
    use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type, Value};

    const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
    const COMPONENT: &str = "org.a11y.atspi.Component";
    const TEXT: &str = "org.a11y.atspi.Text";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    const REGISTRY: &str = "org.a11y.atspi.Registry";
    const ROOT: &str = "/org/a11y/atspi/accessible/root";
    const NULL: &str = "/org/a11y/atspi/null";
    /// `ATSPI_COORD_TYPE_WINDOW`.
    const WINDOW_COORDS: u32 = 1;
    /// How much of a text is read as its value.
    const VALUE_CHARS: i32 = 512;

    /// An accessible: the bus name of its application and its object path.
    pub type Reference = (String, OwnedObjectPath);

    fn real(reference: Reference) -> Option<Reference> {
        (!reference.0.is_empty() && reference.1.as_str() != NULL).then_some(reference)
    }

    /// The accessibility bus, whose address the session bus gives.
    pub fn connect() -> Option<Connection> {
        let session = Connection::session().ok()?;
        let reply = session.call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some("org.a11y.Bus"), "GetAddress", &()).ok()?;
        let address: String = reply.body().deserialize().ok()?;
        connection::Builder::address(address.as_str()).ok()?.build().ok()
    }

    /// Tells the toolkits to expose their accessibles (`org.a11y.Status.IsEnabled`).
    pub fn turn_on() -> Result<(), String> {
        let session = Connection::session().map_err(|e| e.to_string())?;
        let body = ("org.a11y.Status", "IsEnabled", Value::from(true));
        session
            .call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some(PROPERTIES), "Set", &body)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The accessible tree over D-Bus; every call stops answering once the deadline passed.
    pub struct Dbus {
        pub connection: Connection,
        pub deadline: Instant,
    }

    impl Dbus {
        fn call<B: Serialize + DynamicType, R: DeserializeOwned + Type>(
            &self,
            node: &Reference,
            interface: &str,
            method: &str,
            body: &B,
        ) -> Option<R> {
            if self.expired() {
                return None;
            }
            let reply = self.connection.call_method(Some(node.0.as_str()), node.1.as_str(), Some(interface), method, body).ok()?;
            reply.body().deserialize().ok()
        }

        fn property<R: TryFrom<OwnedValue>>(&self, node: &Reference, interface: &str, name: &str) -> Option<R> {
            let value: OwnedValue = self.call(node, PROPERTIES, "Get", &(interface, name))?;
            R::try_from(value).ok()
        }

        fn text(&self, node: &Reference) -> Option<String> {
            let count: i32 = self.property(node, TEXT, "CharacterCount")?;
            self.call(node, TEXT, "GetText", &(0i32, count.min(VALUE_CHARS)))
        }
    }

    impl Tree for Dbus {
        type Node = Reference;

        fn describe(&self, node: &Reference) -> Option<Raw> {
            let role_name: String = self.call(node, ACCESSIBLE, "GetRoleName", &())?;
            let states: Vec<u32> = self.call(node, ACCESSIBLE, "GetState", &())?;
            let editable = has_state(&states, STATE_EDITABLE);
            let value = if reads_value(&role_name, editable) { self.text(node) } else { None };
            Some(Raw {
                name: self.property(node, ACCESSIBLE, "Name"),
                description: self.property(node, ACCESSIBLE, "Description"),
                role_name,
                states,
                value,
            })
        }

        fn states(&self, node: &Reference) -> Option<Vec<u32>> {
            self.call(node, ACCESSIBLE, "GetState", &())
        }

        fn parent(&self, node: &Reference) -> Option<Reference> {
            real(self.property(node, ACCESSIBLE, "Parent")?)
        }

        fn child_at(&self, node: &Reference, (x, y): (i32, i32)) -> Option<Reference> {
            real(self.call(node, COMPONENT, "GetAccessibleAtPoint", &(x, y, WINDOW_COORDS))?)
        }

        fn children(&self, node: &Reference) -> Vec<Reference> {
            let children: Vec<Reference> = self.call(node, ACCESSIBLE, "GetChildren", &()).unwrap_or_default();
            children.into_iter().filter_map(real).collect()
        }

        fn windows(&self) -> Vec<(Reference, Window)> {
            let root = (REGISTRY.to_string(), OwnedObjectPath::try_from(ROOT).expect("a valid path"));
            let mut windows = Vec::new();
            for app in self.children(&root) {
                for node in self.children(&app) {
                    let Some(states) = self.states(&node) else { continue };
                    if has_state(&states, STATE_DEFUNCT) {
                        continue;
                    }
                    let name: String = self.property(&node, ACCESSIBLE, "Name").unwrap_or_default();
                    windows.push((node, Window { name, active: has_state(&states, STATE_ACTIVE) }));
                }
            }
            windows
        }

        fn expired(&self) -> bool {
            Instant::now() >= self.deadline
        }
    }
}

#[cfg(target_os = "linux")]
struct Shared {
    connection: Mutex<Option<zbus::blocking::Connection>>,
    enabled: AtomicBool,
}

#[cfg(target_os = "linux")]
/// Reads elements through AT-SPI. Best effort: a bus that is missing, an app that does not
/// answer, or a toolkit that exposes nothing gives an empty answer, which the trait allows.
pub struct Atspi {
    shared: Arc<Shared>,
    workers: Workers,
}

#[cfg(target_os = "linux")]
impl Default for Atspi {
    fn default() -> Self {
        Atspi {
            shared: Arc::new(Shared { connection: Mutex::new(None), enabled: AtomicBool::new(false) }),
            workers: Workers::default(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Atspi {
    /// Asks the toolkits to expose their accessibles, once, from this process only and without
    /// any prompt. Never waits: it runs on a thread of its own.
    pub fn enable(&self) {
        if self.shared.enabled.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = std::thread::Builder::new().name("computer-atspi-enable".into()).spawn(|| {
            if let Err(error) = dbus::turn_on() {
                eprintln!("gentle-dot: cannot turn accessibility on: {error}");
            }
        });
        if spawned.is_err() {
            self.shared.enabled.store(false, Ordering::Release);
        }
    }

    fn run<T: Send + 'static>(&self, work: impl FnOnce(&dbus::Dbus) -> Option<T> + Send + 'static) -> Option<T> {
        let shared = self.shared.clone();
        within(&self.workers, MAX_WORKERS, QUERY_TIMEOUT, move || {
            let connection = {
                let mut slot = shared.connection.lock().unwrap_or_else(|p| p.into_inner());
                if slot.is_none() {
                    *slot = dbus::connect();
                }
                slot.clone()?
            };
            work(&dbus::Dbus { connection, deadline: Instant::now() + WALK_BUDGET })
        })
    }

    /// The element at `point` (in the window's coordinates) of the window the compositor calls
    /// `title`, then its ancestors; empty when unknown.
    pub fn element_at(&self, title: &str, owner_active: bool, point: (i32, i32)) -> Vec<AxElement> {
        let title = title.to_string();
        self.run(move |tree| {
            let windows = tree.windows();
            let node = window_of(&windows, &title, owner_active)?;
            Some(element_at(tree, node, point))
        })
        .unwrap_or_default()
    }

    /// The focused element of the front window, which the compositor calls `title`.
    pub fn focused(&self, title: &str) -> Option<Focus> {
        let title = title.to_string();
        self.run(move |tree| {
            let windows = tree.windows();
            focused(tree, window_of(&windows, &title, true)?)
        })
    }
}

#[cfg(target_os = "linux")]
fn window_of<'a, N>(windows: &'a [(N, Window)], title: &str, owner_active: bool) -> Option<&'a N> {
    let listed: Vec<Window> = windows.iter().map(|(_, window)| window.clone()).collect();
    pick_window(&listed, title, owner_active).map(|i| &windows[i].0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::linux::pointer::Rect;
    use crate::computer::risk::{assess, Act};

    fn bits(states: &[u32]) -> Vec<u32> {
        let mut set = vec![0u32; 2];
        for state in states {
            set[(state / 32) as usize] |= 1 << (state % 32);
        }
        set
    }

    fn raw(role_name: &str, name: Option<&str>, states: &[u32]) -> Raw {
        Raw { role_name: role_name.into(), name: name.map(Into::into), states: bits(states), ..Raw::default() }
    }

    #[test]
    fn roles_follow_the_macos_ones() {
        for (role_name, editable, expected) in [
            ("push button", false, "AXButton"),
            ("toggle button", false, "AXButton"),
            ("menu item", false, "AXMenuItem"),
            ("check menu item", false, "AXMenuItem"),
            ("menu", false, "AXMenuBarItem"),
            ("link", false, "AXLink"),
            ("entry", true, "AXTextField"),
            ("password text", true, "AXTextField"),
            ("spin button", true, "AXTextField"),
            ("text", true, "AXTextArea"),
            ("text", false, "AXStaticText"),
            ("label", false, "AXStaticText"),
            ("combo box", false, "AXComboBox"),
            ("check box", false, "AXCheckBox"),
            ("radio button", false, "AXRadioButton"),
            ("page tab", false, "AXTab"),
            ("list item", false, "AXRow"),
            ("panel", false, "AXGroup"),
            ("frame", false, "AXWindow"),
            ("dialog", false, "AXWindow"),
            ("something new", false, "AXUnknown"),
            ("", false, "AXUnknown"),
        ] {
            assert_eq!(role(role_name, editable), expected, "{role_name} editable {editable}");
        }
    }

    #[test]
    fn a_send_button_is_a_risky_click_as_on_macos() {
        let send = element(&raw("push button", Some("Send"), &[]));
        assert_eq!(send, AxElement { role: "AXButton".into(), title: Some("Send".into()), description: None, value: None });
        assert!(assess(Act::Click(&[send]), None).unwrap().contains("Send"));
        let cancel = element(&raw("push button", Some("Cancel"), &[]));
        assert_eq!(assess(Act::Click(&[cancel]), None), None);
    }

    #[test]
    fn names_are_trimmed_and_missing_ones_are_absent() {
        let mut blank = raw("push button", Some("  "), &[]);
        blank.description = Some("Pay now".into());
        let found = element(&blank);
        assert_eq!((found.title, found.description), (None, Some("Pay now".into())));
        assert!(assess(Act::Click(&[element(&blank)]), None).is_some());
        assert_eq!(element(&raw("push button", None, &[])).title, None);
        // An unknown role is not pressable, whatever it says.
        assert_eq!(assess(Act::Click(&[element(&raw("something new", Some("Delete"), &[]))]), None), None);
    }

    #[test]
    fn only_editable_text_gives_its_value_and_a_password_never_does() {
        let with_value = |role_name: &str, states: &[u32]| {
            let mut found = raw(role_name, Some("Name"), states);
            found.value = Some("hunter2".into());
            element(&found).value
        };
        assert_eq!(with_value("entry", &[STATE_EDITABLE]), Some("hunter2".into()));
        assert_eq!(with_value("password text", &[STATE_EDITABLE]), None);
        assert_eq!(with_value("label", &[]), None);
        assert_eq!(with_value("text", &[]), None);
        assert!(reads_value("entry", true) && reads_value("text", true));
        assert!(!reads_value("password text", true) && !reads_value("push button", false) && !reads_value("entry", false));
    }

    #[test]
    fn a_default_button_asks_before_return() {
        let default = raw("push button", Some("OK"), &[STATE_IS_DEFAULT]);
        let found = focus(&default, None);
        assert!(found.is_default_button);
        assert_eq!(found.default_button, Some(element(&default)));
        assert!(assess(Act::Return(Some(&found)), None).unwrap().contains("default button"));
        // A chat field whose window has a risky default button.
        let field = raw("entry", Some("Message #general"), &[STATE_EDITABLE]);
        let send = raw("push button", Some("Send"), &[STATE_IS_DEFAULT]);
        let found = focus(&field, Some(&send));
        assert!(!found.is_default_button);
        assert_eq!(found.default_button, Some(element(&send)));
        assert!(assess(Act::Return(Some(&found)), None).is_some());
        // Nothing risky anywhere.
        let note = focus(&raw("text", Some("Notes"), &[STATE_EDITABLE]), None);
        assert_eq!(assess(Act::Return(Some(&note)), None), None);
    }

    #[test]
    fn states_are_read_from_the_two_words() {
        let set = bits(&[STATE_FOCUSED, STATE_IS_DEFAULT]);
        assert!(has_state(&set, STATE_FOCUSED) && has_state(&set, STATE_IS_DEFAULT));
        assert!(!has_state(&set, STATE_ACTIVE));
        assert!(!has_state(&[], STATE_FOCUSED) && !has_state(&[1], STATE_IS_DEFAULT));
    }

    #[test]
    fn the_window_is_picked_by_title_or_by_being_the_only_active_one() {
        let win = |name: &str, active: bool| Window { name: name.into(), active };
        let one = [win("Mail", false), win("Notes", true)];
        for (windows, title, owner_active, expected) in [
            (&one[..], "Mail", false, Some(0)),
            (&one[..], "Notes", true, Some(1)),
            // No title match: only the active window of an active owner.
            (&one[..], "Renamed", true, Some(1)),
            (&one[..], "Renamed", false, None),
            (&one[..], "", true, Some(1)),
            // Two windows with the title: not known.
            (&[win("Doc", false), win("Doc", false)][..], "Doc", false, None),
            (&[win("Doc", true), win("Doc", false)][..], "Doc", true, None),
            // Two active windows and no title match.
            (&[win("A", true), win("B", true)][..], "C", true, None),
            (&[][..], "Mail", true, None),
        ] {
            assert_eq!(pick_window(windows, title, owner_active), expected, "{title} {owner_active}");
        }
    }

    #[test]
    fn a_point_is_taken_in_the_frame_that_holds_it() {
        let frames = [Rect { x: 100, y: 50, width: 400, height: 300 }, Rect { x: 2000, y: 0, width: 100, height: 100 }];
        for (at, expected) in [
            ((100.0, 50.0), Some((0, 0))),
            ((250.4, 120.6), Some((150, 71))),
            ((2050.0, 10.0), Some((50, 10))),
            ((500.0, 50.0), None),
            ((99.0, 60.0), None),
        ] {
            assert_eq!(window_point(at, &frames), expected, "{at:?}");
        }
        assert_eq!(window_point((1.0, 1.0), &[]), None);
    }

    /// A tree of rectangles (window coordinates); a child holds a point when its rectangle does.
    struct Fake {
        nodes: Vec<FakeNode>,
        windows: Vec<(usize, Window)>,
        expired_after: std::cell::Cell<usize>,
    }

    struct FakeNode {
        parent: Option<usize>,
        children: Vec<usize>,
        rect: (i32, i32, i32, i32),
        raw: Option<Raw>,
        states: Vec<u32>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake { nodes: Vec::new(), windows: Vec::new(), expired_after: std::cell::Cell::new(usize::MAX) }
        }

        fn add(&mut self, parent: Option<usize>, rect: (i32, i32, i32, i32), role_name: &str, name: &str, states: &[u32]) -> usize {
            let id = self.nodes.len();
            if let Some(parent) = parent {
                self.nodes[parent].children.push(id);
            }
            let raw = Some(raw(role_name, Some(name), states));
            self.nodes.push(FakeNode { parent, children: Vec::new(), rect, raw, states: bits(states) });
            id
        }
    }

    impl Tree for Fake {
        type Node = usize;

        fn describe(&self, node: &usize) -> Option<Raw> {
            self.nodes[*node].raw.clone()
        }

        fn states(&self, node: &usize) -> Option<Vec<u32>> {
            self.nodes[*node].raw.as_ref().map(|_| self.nodes[*node].states.clone())
        }

        fn parent(&self, node: &usize) -> Option<usize> {
            self.nodes[*node].parent
        }

        fn child_at(&self, node: &usize, (x, y): (i32, i32)) -> Option<usize> {
            self.nodes[*node].children.iter().copied().find(|child| {
                let (left, top, width, height) = self.nodes[*child].rect;
                x >= left && y >= top && x < left + width && y < top + height
            })
        }

        fn children(&self, node: &usize) -> Vec<usize> {
            self.nodes[*node].children.clone()
        }

        fn windows(&self) -> Vec<(usize, Window)> {
            self.windows.clone()
        }

        fn expired(&self) -> bool {
            let left = self.expired_after.get();
            self.expired_after.set(left.saturating_sub(1));
            left == 0
        }
    }

    /// root > app > window > pane > form > {Send button, message entry}.
    fn app() -> (Fake, usize) {
        let mut tree = Fake::new();
        let root = tree.add(None, (0, 0, 0, 0), "application", "app", &[]);
        let window = tree.add(Some(root), (0, 0, 800, 600), "frame", "Chat", &[STATE_ACTIVE]);
        let pane = tree.add(Some(window), (0, 0, 800, 600), "panel", "", &[]);
        let form = tree.add(Some(pane), (0, 500, 800, 100), "panel", "", &[]);
        tree.add(Some(form), (700, 500, 100, 100), "push button", "Send", &[STATE_IS_DEFAULT]);
        tree.add(Some(form), (0, 500, 700, 100), "entry", "Message", &[STATE_EDITABLE, STATE_FOCUSED]);
        tree.windows = vec![(window, Window { name: "Chat".into(), active: true })];
        (tree, window)
    }

    #[test]
    fn the_deepest_element_comes_first_then_up_to_four_ancestors() {
        let (tree, window) = app();
        let chain = element_at(&tree, &window, (750, 550));
        let roles: Vec<_> = chain.iter().map(|e| e.role.as_str()).collect();
        assert_eq!(roles, ["AXButton", "AXGroup", "AXGroup", "AXWindow", "AXUnknown"]);
        assert_eq!(chain[0].title.as_deref(), Some("Send"));
        assert!(assess(Act::Click(&chain), None).is_some());
        // Over the form but on no control, and over the pane only.
        assert_eq!(element_at(&tree, &window, (10, 10))[0].role, "AXGroup");
        // Outside the window: the window itself.
        assert_eq!(element_at(&tree, &window, (900, 900))[0].role, "AXWindow");
    }

    #[test]
    fn an_element_that_cannot_be_read_gives_nothing() {
        let (mut tree, window) = app();
        tree.nodes[4].raw = None;
        assert!(element_at(&tree, &window, (750, 550)).is_empty());
    }

    #[test]
    fn a_tree_that_loops_back_does_not_hang_the_walk() {
        let (mut tree, window) = app();
        // A child that holds its own parent: the walk stops at its depth limit.
        tree.nodes[4].children.push(3);
        tree.nodes[3].children.push(4);
        assert!(!element_at(&tree, &window, (750, 550)).is_empty());
    }

    #[test]
    fn the_focused_element_comes_with_its_windows_default_button() {
        let (tree, window) = app();
        let found = focused(&tree, &window).unwrap();
        assert_eq!(found.element.title.as_deref(), Some("Message"));
        assert!(!found.is_default_button);
        assert_eq!(found.default_button.unwrap().title.as_deref(), Some("Send"));
        assert!(assess(Act::Return(focused(&tree, &window).as_ref()), None).is_some());
    }

    #[test]
    fn a_search_gives_up_when_nothing_has_the_state_or_time_is_up() {
        let (tree, window) = app();
        assert!(find_state(&tree, &window, STATE_FOCUSED).is_some());
        assert!(find_state(&tree, &window, 30).is_none());
        tree.expired_after.set(0);
        assert!(find_state(&tree, &window, STATE_FOCUSED).is_none());
        // Past the node limit.
        let mut wide = Fake::new();
        let root = wide.add(None, (0, 0, 10, 10), "frame", "w", &[]);
        for _ in 0..SEARCH_NODES + 5 {
            wide.add(Some(root), (0, 0, 1, 1), "label", "x", &[]);
        }
        wide.add(Some(root), (0, 0, 1, 1), "entry", "late", &[STATE_FOCUSED]);
        assert!(find_state(&wide, &root, STATE_FOCUSED).is_none());
    }

    #[test]
    fn a_query_that_does_not_answer_is_given_up_and_cannot_pile_up() {
        let workers = Workers::default();
        let ms = Duration::from_millis;
        assert_eq!(within(&workers, 1, ms(500), || Some(7)), Some(7));
        assert_eq!(within::<i32>(&workers, 1, ms(500), || None), None);
        // A stuck query: the caller gives up at the limit, and a second one is refused at once.
        let (release, stuck) = mpsc::channel::<()>();
        let started = Instant::now();
        assert_eq!(within(&workers, 1, ms(30), move || stuck.recv().ok().map(|_| 1)), None);
        assert!(started.elapsed() < ms(400));
        let started = Instant::now();
        assert_eq!(within(&workers, 1, ms(5000), || Some(2)), None);
        assert!(started.elapsed() < ms(400));
        // Once it ends, queries run again.
        drop(release);
        let again = (0..100).find_map(|_| within(&workers, 1, ms(500), || Some(3)).or_else(|| {
            std::thread::sleep(ms(10));
            None
        }));
        assert_eq!(again, Some(3));
    }
}
