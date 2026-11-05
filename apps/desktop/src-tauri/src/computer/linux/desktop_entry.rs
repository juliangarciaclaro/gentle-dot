//! `.desktop` entries (S2): what `list_apps` and `open_app` read from the XDG data dirs.
//! Only the `[Desktop Entry]` group counts. `exec_argv` splits an `Exec` line per the Desktop
//! Entry specification's quoting rules and drops its field codes.

/// The fields of a `[Desktop Entry]` group the Linux desktop uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    /// `Name`, or its localized form when a locale was given and the entry has one.
    pub name: Option<String>,
    pub exec: Option<String>,
    pub icon: Option<String>,
    pub startup_wm_class: Option<String>,
    pub no_display: bool,
    pub hidden: bool,
    /// `Type=Application`.
    pub is_application: bool,
}

impl DesktopEntry {
    /// Whether the entry belongs in an app list: an application that is not hidden.
    pub fn is_listed(&self) -> bool {
        self.is_application && !self.no_display && !self.hidden
    }
}

/// Parses the file's contents; `locale` is a POSIX locale such as `es_AR.UTF-8` for the
/// localized `Name`. `None` when there is no `[Desktop Entry]` group.
pub fn parse(contents: &str, locale: Option<&str>) -> Option<DesktopEntry> {
    let candidates = locale.map(name_keys).unwrap_or_default();
    let mut entry = DesktopEntry::default();
    let mut in_group = false;
    let mut found = false;
    let mut localized: Option<(usize, String)> = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            found |= in_group;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if !in_group {
            continue;
        }
        let (key, value) = (key.trim(), unescape(value.trim_start()));
        match key {
            "Name" => entry.name = entry.name.or(Some(value)),
            "Exec" => entry.exec = Some(value),
            "Icon" => entry.icon = Some(value),
            "StartupWMClass" => entry.startup_wm_class = Some(value),
            "NoDisplay" => entry.no_display = value == "true",
            "Hidden" => entry.hidden = value == "true",
            "Type" => entry.is_application = value == "Application",
            _ => {
                // The most specific `Name[...]` wins (lowest index in `candidates`).
                if let Some(rank) = candidates.iter().position(|c| format!("Name[{c}]") == key) {
                    if localized.as_ref().is_none_or(|(best, _)| rank < *best) {
                        localized = Some((rank, value));
                    }
                }
            }
        }
    }
    if let Some((_, name)) = localized {
        entry.name = Some(name);
    }
    found.then_some(entry)
}

/// The locale suffixes to try for `Name[...]`, most specific first: `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang`.
fn name_keys(locale: &str) -> Vec<String> {
    let without_encoding = |s: &str| s.split('.').next().unwrap_or("").to_owned();
    let (rest, modifier) = match locale.split_once('@') {
        Some((rest, modifier)) => (without_encoding(rest), Some(modifier)),
        None => (without_encoding(locale), None),
    };
    let (lang, country) = match rest.split_once('_') {
        Some((lang, country)) => (lang.to_owned(), Some(country.to_owned())),
        None => (rest, None),
    };
    if lang.is_empty() {
        return Vec::new();
    }
    let mut keys = Vec::new();
    if let (Some(country), Some(modifier)) = (&country, modifier) {
        keys.push(format!("{lang}_{country}@{modifier}"));
    }
    if let Some(country) = &country {
        keys.push(format!("{lang}_{country}"));
    }
    if let Some(modifier) = modifier {
        keys.push(format!("{lang}@{modifier}"));
    }
    keys.push(lang);
    keys
}

/// The string escapes of the specification: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Splits an `Exec` value into the program and its arguments: arguments are separated by
/// spaces, double quotes group (with `\"`, `` \` ``, `\$` and `\\` escaped inside), `%%` is a
/// literal `%`, and field codes (`%f %F %u %U %i %c %k` and the deprecated ones) are dropped,
/// an argument made only of a field code with it.
pub fn exec_argv(exec: &str) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut arg = String::new();
    let mut started = false; // a quoted empty argument still counts
    let mut dropped_code = false; // an argument left empty by a dropped code is dropped too
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match chars.next() {
                Some(escaped @ ('"' | '`' | '$' | '\\')) => arg.push(escaped),
                Some(other) => {
                    arg.push('\\');
                    arg.push(other);
                }
                None => return Err(format!("`{exec}` ends inside an escape")),
            },
            '%' => match chars.next() {
                Some('%') => {
                    arg.push('%');
                    started = true;
                }
                Some('f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {
                    dropped_code = true;
                }
                Some(other) => return Err(format!("unknown field code `%{other}` in `{exec}`")),
                None => return Err(format!("`{exec}` ends with a lone `%`")),
            },
            ' ' if !quoted => {
                if started && !(arg.is_empty() && dropped_code) {
                    argv.push(std::mem::take(&mut arg));
                }
                arg.clear();
                started = false;
                dropped_code = false;
            }
            other => {
                arg.push(other);
                started = true;
            }
        }
    }
    if quoted {
        return Err(format!("`{exec}` has an unclosed quote"));
    }
    if started && !(arg.is_empty() && dropped_code) {
        argv.push(arg);
    }
    if argv.is_empty() {
        return Err(format!("`{exec}` names no program"));
    }
    Ok(argv)
}

/// Whether `app_id` (a Wayland app_id or X11 class) names this entry: case-insensitively, the
/// desktop file id without `.desktop`, that id's last dotted component, or `StartupWMClass`.
pub fn matches_app_id(entry_id: &str, entry: &DesktopEntry, app_id: &str) -> bool {
    if app_id.is_empty() {
        return false;
    }
    let id = entry_id.strip_suffix(".desktop").unwrap_or(entry_id);
    let last = id.rsplit('.').next().unwrap_or(id);
    [Some(id), Some(last), entry.startup_wm_class.as_deref()]
        .into_iter()
        .flatten()
        .any(|candidate| candidate.eq_ignore_ascii_case(app_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
# a comment
[Desktop Entry]
Type=Application
Name=Files
Name[es]=Archivos
Name[es_AR]=Archivos AR
Name[de]=Dateien
Exec=nautilus --new-window %U
Icon=org.gnome.Nautilus
StartupWMClass=Org.gnome.Nautilus
NoDisplay=false

[Desktop Action new-window]
Name=New Window
Exec=nautilus --other
NoDisplay=true
";

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        let entry = parse(FILE, None).unwrap();
        assert_eq!(entry.name.as_deref(), Some("Files"));
        assert_eq!(entry.exec.as_deref(), Some("nautilus --new-window %U"));
        assert_eq!(entry.icon.as_deref(), Some("org.gnome.Nautilus"));
        assert_eq!(entry.startup_wm_class.as_deref(), Some("Org.gnome.Nautilus"));
        assert!(entry.is_application && !entry.no_display && !entry.hidden && entry.is_listed());
        assert!(parse("[Other]\nName=x\n", None).is_none());
        assert!(parse("", None).is_none());
    }

    #[test]
    fn name_is_localized_most_specific_first() {
        let name = |locale| parse(FILE, locale).unwrap().name.unwrap();
        assert_eq!(name(Some("es_AR.UTF-8")), "Archivos AR");
        assert_eq!(name(Some("es_ES.UTF-8")), "Archivos");
        assert_eq!(name(Some("de_DE@euro")), "Dateien");
        assert_eq!(name(Some("fr_FR")), "Files");
        assert_eq!(name(Some("C")), "Files");
        let modified = "[Desktop Entry]\nName=N\nName[sr]=lat\nName[sr@latin]=latin\n";
        assert_eq!(parse(modified, Some("sr@latin")).unwrap().name.unwrap(), "latin");
    }

    #[test]
    fn hidden_and_non_application_entries_are_not_listed() {
        for body in ["NoDisplay=true", "Hidden=true", "Type=Link"] {
            let file = format!("[Desktop Entry]\nType=Application\nName=X\n{body}\n");
            let entry = parse(&file, None).unwrap();
            assert!(!entry.is_listed(), "{body}");
        }
    }

    #[test]
    fn string_escapes_are_decoded() {
        let entry = parse("[Desktop Entry]\nName=a\\sb\\\\c\nExec=x\n", None).unwrap();
        assert_eq!(entry.name.as_deref(), Some("a b\\c"));
    }

    fn argv(exec: &str) -> Vec<String> {
        exec_argv(exec).unwrap()
    }

    #[test]
    fn exec_splits_on_spaces_and_honours_quotes() {
        assert_eq!(argv("firefox --new-window"), ["firefox", "--new-window"]);
        assert_eq!(argv("  a   b  "), ["a", "b"]);
        assert_eq!(argv(r#"sh -c "echo \"hi\" \$HOME \\ \`x\`""#), ["sh", "-c", r#"echo "hi" $HOME \ `x`"#]);
        assert_eq!(argv(r#""/opt/My App/run" --flag"#), ["/opt/My App/run", "--flag"]);
        assert_eq!(argv(r#"cmd "" x"#), ["cmd", "", "x"]);
    }

    #[test]
    fn exec_drops_field_codes_and_unescapes_percent() {
        assert_eq!(argv("gedit %U"), ["gedit"]);
        assert_eq!(argv("app %f --x %F %u %i %c %k"), ["app", "--x"]);
        assert_eq!(argv("app --file=%f"), ["app", "--file="]);
        assert_eq!(argv("app 100%% %f"), ["app", "100%"]);
        assert_eq!(argv(r#"app "%u" y"#), ["app", "y"]);
    }

    #[test]
    fn exec_errors() {
        for exec in ["", "   ", "%U", "app %z", "app %", r#"app "unclosed"#] {
            assert!(exec_argv(exec).is_err(), "{exec:?}");
        }
    }

    #[test]
    fn app_ids_match_the_file_id_its_last_component_or_the_wm_class() {
        let entry = parse(FILE, None).unwrap();
        let id = "org.gnome.Nautilus.desktop";
        assert!(matches_app_id(id, &entry, "org.gnome.Nautilus"));
        assert!(matches_app_id(id, &entry, "ORG.GNOME.NAUTILUS"));
        assert!(matches_app_id(id, &entry, "nautilus"));
        assert!(matches_app_id("files.desktop", &entry, "org.gnome.nautilus")); // StartupWMClass
        assert!(!matches_app_id(id, &entry, "gnome"));
        assert!(!matches_app_id(id, &entry, ""));
        assert!(matches_app_id("firefox.desktop", &DesktopEntry::default(), "Firefox"));
    }
}
