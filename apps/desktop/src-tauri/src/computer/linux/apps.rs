//! The installed apps (S2): the `.desktop` entries in the XDG data dirs. They name running
//! windows (a Wayland app_id has no display name) and say how `open_app` starts an app.

use super::desktop_entry::{matches_app_id, parse, DesktopEntry};
use std::path::{Path, PathBuf};

/// How deep below `applications/` entries are looked for (vendor subdirectories).
const MAX_DEPTH: usize = 3;

/// The `applications` directories, highest precedence first: `$XDG_DATA_HOME` (default
/// `~/.local/share`), then `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`).
pub fn data_dirs(env: &impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let set = |key: &str| env(key).filter(|value| !value.is_empty());
    let home = set("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(system.split(':').filter(|dir| !dir.is_empty()).map(PathBuf::from))
        .map(|dir| dir.join("applications"))
        .collect()
}

/// The POSIX locale for localized names: `LC_ALL`, `LC_MESSAGES`, then `LANG`.
pub fn locale(env: &impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(env)
        .find(|value| !value.is_empty() && value != "C" && value != "POSIX")
}

/// The desktop file ID: the path below the `applications` dir with `/` turned into `-`.
fn desktop_id(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?.to_str()?;
    Some(relative.replace('/', "-"))
}

fn visit(root: &Path, dir: &Path, depth: usize, locale: Option<&str>, out: &mut Vec<(String, DesktopEntry)>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = read.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            if depth < MAX_DEPTH {
                visit(root, &path, depth + 1, locale, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            let Some(id) = desktop_id(root, &path) else { continue };
            // The first directory that has an ID wins, even if a later one has it too.
            if out.iter().any(|(known, _)| *known == id) {
                continue;
            }
            if let Some(entry) = std::fs::read_to_string(&path).ok().and_then(|text| parse(&text, locale)) {
                out.push((id, entry));
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// (desktop file ID with `.desktop`, entry), highest-precedence directory first.
    entries: Vec<(String, DesktopEntry)>,
}

impl Catalog {
    pub fn from_entries(entries: Vec<(String, DesktopEntry)>) -> Self {
        Catalog { entries }
    }

    pub fn load(env: &impl Fn(&str) -> Option<String>) -> Self {
        let locale = locale(env);
        let mut entries = Vec::new();
        for dir in data_dirs(env) {
            visit(&dir, &dir, 0, locale.as_deref(), &mut entries);
        }
        Catalog { entries }
    }

    /// The display name of the app a window's `app_id` belongs to.
    pub fn name_for(&self, app_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.is_application && !entry.hidden)
            .find(|(id, entry)| matches_app_id(id, entry, app_id))
            .and_then(|(_, entry)| entry.name.as_deref())
    }

    /// The listed app that `query` names: its display name (any case), its desktop file ID
    /// with or without `.desktop`, or the app_id it opens windows with.
    pub fn find(&self, query: &str) -> Option<(&str, &DesktopEntry)> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .filter(|(_, entry)| entry.is_listed())
            .find(|(id, entry)| {
                entry.name.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(query))
                    || id.eq_ignore_ascii_case(query)
                    || matches_app_id(id, entry, query)
            })
            .map(|(id, entry)| (id.as_str(), entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    }

    fn entry(name: &str, wm_class: Option<&str>, listed: bool) -> DesktopEntry {
        DesktopEntry {
            name: Some(name.into()),
            exec: Some("app %U".into()),
            startup_wm_class: wm_class.map(str::to_string),
            no_display: !listed,
            is_application: true,
            ..DesktopEntry::default()
        }
    }

    fn catalog() -> Catalog {
        Catalog::from_entries(vec![
            ("org.gnome.Nautilus.desktop".into(), entry("Files", None, true)),
            ("firefox.desktop".into(), entry("Firefox Web Browser", Some("Navigator"), true)),
            ("helper.desktop".into(), entry("Helper", None, false)),
        ])
    }

    #[test]
    fn data_dirs_follow_the_xdg_variables_in_precedence_order() {
        let dirs = data_dirs(&env_of(&[("XDG_DATA_HOME", "/h/data"), ("XDG_DATA_DIRS", "/a:/b::")]));
        assert_eq!(dirs, [Path::new("/h/data/applications"), Path::new("/a/applications"), Path::new("/b/applications")]);
        let defaults = data_dirs(&env_of(&[("HOME", "/home/u")]));
        assert_eq!(
            defaults,
            [
                Path::new("/home/u/.local/share/applications"),
                Path::new("/usr/local/share/applications"),
                Path::new("/usr/share/applications")
            ]
        );
    }

    #[test]
    fn the_locale_skips_c_and_empty_values() {
        assert_eq!(locale(&env_of(&[("LC_ALL", "C"), ("LANG", "es_AR.UTF-8")])).as_deref(), Some("es_AR.UTF-8"));
        assert_eq!(locale(&env_of(&[("LC_ALL", ""), ("LC_MESSAGES", "de")])).as_deref(), Some("de"));
        assert_eq!(locale(&env_of(&[("LANG", "POSIX")])), None);
    }

    #[test]
    fn a_window_app_id_gets_the_entry_display_name() {
        let catalog = catalog();
        assert_eq!(catalog.name_for("org.gnome.Nautilus"), Some("Files"));
        assert_eq!(catalog.name_for("navigator"), Some("Firefox Web Browser"));
        assert_eq!(catalog.name_for("firefox"), Some("Firefox Web Browser"));
        assert_eq!(catalog.name_for("unknown-app"), None);
        assert_eq!(catalog.name_for(""), None);
    }

    #[test]
    fn open_app_finds_listed_apps_by_name_id_or_app_id_only() {
        let catalog = catalog();
        for query in ["files", "Firefox Web Browser", "firefox.desktop", "org.gnome.Nautilus", "Navigator"] {
            assert!(catalog.find(query).is_some(), "{query}");
        }
        assert_eq!(catalog.find("Files").map(|(id, _)| id), Some("org.gnome.Nautilus.desktop"));
        // Hidden entries and unknown or empty names are not found.
        assert!(catalog.find("Helper").is_none());
        assert!(catalog.find("nothing").is_none());
        assert!(catalog.find("  ").is_none());
    }

    #[test]
    fn load_reads_nested_dirs_and_the_first_directory_wins() {
        let root = std::env::temp_dir().join(format!("gentle-dot-apps-{}", std::process::id()));
        let (high, low) = (root.join("high/applications"), root.join("low/applications"));
        std::fs::create_dir_all(high.join("vendor")).unwrap();
        std::fs::create_dir_all(&low).unwrap();
        std::fs::write(high.join("vendor/tool.desktop"), "[Desktop Entry]\nType=Application\nName=Tool\nExec=tool\n").unwrap();
        std::fs::write(high.join("a.desktop"), "[Desktop Entry]\nType=Application\nName=A high\nExec=a\n").unwrap();
        std::fs::write(low.join("a.desktop"), "[Desktop Entry]\nType=Application\nName=A low\nExec=a\n").unwrap();
        std::fs::write(low.join("note.txt"), "not an entry").unwrap();
        let data_dirs = format!("{}:{}", root.join("high").display(), root.join("low").display());
        let env = move |key: &str| (key == "XDG_DATA_DIRS").then(|| data_dirs.clone());
        let catalog = Catalog::load(&env);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(catalog.find("a").and_then(|(_, e)| e.name.clone()).as_deref(), Some("A high"));
        assert_eq!(catalog.find("vendor-tool").map(|(id, _)| id), Some("vendor-tool.desktop"));
        assert_eq!(catalog.entries.len(), 2);
    }
}
