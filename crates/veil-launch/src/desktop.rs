//! Application discovery from `.desktop` files, following the XDG Desktop
//! Entry and Base Directory specifications closely enough that what the
//! launcher lists is what other launchers list.
//!
//! The output of this module is an `App` whose `exec` is a command line ready
//! for `sh -c` (the compositor's `launch` IPC command runs it that way): the
//! `Exec=` value is tokenised with the spec's quoting rules, field codes are
//! resolved, and every argument is re-quoted for the shell. Splitting on
//! whitespace instead breaks any entry with a quoted argument.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// How deep to look below an `applications` directory (Flatpak and some
/// vendors use subdirectories).
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Desktop file ID: the path below `applications/` with `/` as `-`.
    pub id: String,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub icon: String,
    /// Ready for `sh -c`; see the module docs.
    pub exec: String,
    pub path: PathBuf,
    /// First word of the command, for matching what people type (`firefox`).
    pub program: String,
    // Lowercased once at scan time so a keystroke allocates nothing.
    pub(crate) name_lc: String,
    pub(crate) generic_lc: String,
    pub(crate) comment_lc: String,
    pub(crate) keywords_lc: Vec<String>,
    pub(crate) program_lc: String,
}

impl App {
    /// Fill the lowercase caches. Call after building an `App` by hand.
    pub fn index(mut self) -> Self {
        self.name_lc = self.name.to_lowercase();
        self.generic_lc = self.generic_name.to_lowercase();
        self.comment_lc = self.comment.to_lowercase();
        self.keywords_lc = self.keywords.iter().map(|k| k.to_lowercase()).collect();
        self.program_lc = self.program.to_lowercase();
        self
    }
}

/// Everything the scan reads from the environment, so tests can supply it.
#[derive(Debug, Clone, Default)]
pub struct ScanEnv {
    /// `applications` directories, highest precedence first.
    pub dirs: Vec<PathBuf>,
    /// `XDG_CURRENT_DESKTOP`, split on `:`.
    pub desktops: Vec<String>,
    /// Argv prefix that runs the rest in a terminal (`["foot", "-e"]`).
    pub terminal: Option<Vec<String>>,
}

impl ScanEnv {
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let data_home = non_empty_env("XDG_DATA_HOME").unwrap_or_else(|| format!("{home}/.local/share"));
        let data_dirs = non_empty_env("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut dirs: Vec<PathBuf> = Vec::new();
        for base in std::iter::once(data_home.as_str()).chain(data_dirs.split(':')) {
            if base.is_empty() {
                continue;
            }
            let dir = Path::new(base).join("applications");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let desktops = non_empty_env("XDG_CURRENT_DESKTOP")
            .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        ScanEnv { dirs, desktops, terminal: find_terminal() }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn find_terminal() -> Option<Vec<String>> {
    if let Some(t) = non_empty_env("TERMINAL") {
        let mut argv: Vec<String> = t.split_whitespace().map(str::to_string).collect();
        argv.push("-e".into());
        return Some(argv);
    }
    ["foot", "alacritty", "kitty", "ghostty", "konsole", "xterm"]
        .iter()
        .find(|t| in_path(t))
        .map(|t| vec![(*t).to_string(), "-e".to_string()])
}

/// Scan every directory in precedence order. The first file with a given ID
/// wins, even when it is hidden: a user's `Hidden=true` copy is how a system
/// entry gets removed.
pub fn scan(env: &ScanEnv) -> Vec<App> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut apps = Vec::new();
    for dir in &env.dirs {
        let mut files = Vec::new();
        collect(dir, dir, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            if let Some(app) = parse(&id, &path, &content, env) {
                apps.push(app);
            }
        }
    }
    apps.sort_by(|a, b| a.name_lc.cmp(&b.name_lc).then_with(|| a.id.cmp(&b.id)));
    apps
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < MAX_DEPTH {
                collect(root, &path, depth + 1, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("desktop") {
            if let Ok(rel) = path.strip_prefix(root) {
                let id = rel.to_string_lossy().replace('/', "-");
                out.push((id, path));
            }
        }
    }
}

/// Parse one file. `None` means "not something to show": hidden, not an
/// application, not for this desktop, its `TryExec` is missing, or it cannot
/// be launched.
pub fn parse(id: &str, path: &Path, content: &str, env: &ScanEnv) -> Option<App> {
    let mut in_entry = false;
    let mut f = Fields::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if in_entry {
                break; // only [Desktop Entry]; actions come after it
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else { continue };
        let (key, val) = (key.trim(), val.trim());
        match key {
            "Type" => f.kind = val.to_string(),
            "Name" => f.name = unescape(val),
            "GenericName" => f.generic_name = unescape(val),
            "Comment" => f.comment = unescape(val),
            "Icon" => f.icon = val.to_string(),
            "Exec" => f.exec = val.to_string(),
            "TryExec" => f.try_exec = val.to_string(),
            "Keywords" => f.keywords = list(val),
            "Categories" => f.categories = list(val),
            "OnlyShowIn" => f.only_show_in = Some(list(val)),
            "NotShowIn" => f.not_show_in = list(val),
            "Terminal" => f.terminal = val == "true",
            "NoDisplay" => f.no_display = val == "true",
            "Hidden" => f.hidden = val == "true",
            _ => {} // localised keys (Name[de]) and everything else
        }
    }

    if !f.kind.is_empty() && f.kind != "Application" {
        return None;
    }
    if f.name.is_empty() || f.exec.is_empty() || f.hidden || f.no_display {
        return None;
    }
    let listed = |names: &[String]| names.iter().any(|n| env.desktops.iter().any(|d| d == n));
    if let Some(only) = &f.only_show_in {
        if !listed(only) {
            return None;
        }
    }
    if listed(&f.not_show_in) {
        return None;
    }
    if !f.try_exec.is_empty() && !in_path(&f.try_exec) {
        return None;
    }

    let mut argv = exec_argv(&f.exec, &f.name, &f.icon, path)?;
    let program = argv.first().map(|a| basename(a).to_string()).unwrap_or_default();
    if f.terminal {
        let mut prefixed = env.terminal.clone()?;
        prefixed.extend(argv);
        argv = prefixed;
    }

    Some(
        App {
            id: id.to_string(),
            name: f.name,
            generic_name: f.generic_name,
            comment: f.comment,
            keywords: f.keywords,
            categories: f.categories,
            icon: f.icon,
            exec: shell_join(&argv),
            path: path.to_path_buf(),
            program,
            name_lc: String::new(),
            generic_lc: String::new(),
            comment_lc: String::new(),
            keywords_lc: Vec::new(),
            program_lc: String::new(),
        }
        .index(),
    )
}

#[derive(Default)]
struct Fields {
    kind: String,
    name: String,
    generic_name: String,
    comment: String,
    icon: String,
    exec: String,
    try_exec: String,
    keywords: Vec<String>,
    categories: Vec<String>,
    only_show_in: Option<Vec<String>>,
    not_show_in: Vec<String>,
    terminal: bool,
    no_display: bool,
    hidden: bool,
}

/// `;`-separated list with `\;` as an escaped semicolon.
fn list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => cur.push(';'),
                Some(o) => {
                    cur.push('\\');
                    cur.push(o);
                }
                None => cur.push('\\'),
            },
            ';' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The spec's string escapes: `\s \n \t \r \\`.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
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
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split an `Exec=` value into arguments with the spec's quoting rules: a
/// double-quoted argument may hold spaces, and inside quotes a backslash
/// escapes `"`, `` ` ``, `$` and `\`. `None` for an unbalanced quote.
fn split_exec(s: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut in_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            match c {
                '"' => in_quote = false,
                '\\' => match chars.peek() {
                    Some('"' | '`' | '$' | '\\') => cur.push(chars.next()?),
                    _ => cur.push('\\'),
                },
                c => cur.push(c),
            }
        } else {
            match c {
                '"' => {
                    in_quote = true;
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    if in_quote {
        return None;
    }
    if started {
        args.push(cur);
    }
    Some(args)
}

/// Resolve the field codes. With nothing to open, an argument that carried a
/// file or URL code is dropped whole (`--file=%f` must not become `--file=`).
/// `None` when the command is empty or malformed.
fn exec_argv(exec: &str, name: &str, icon: &str, path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for arg in split_exec(exec)? {
        if arg == "%i" {
            if !icon.is_empty() {
                out.push("--icon".to_string());
                out.push(icon.to_string());
            }
            continue;
        }
        let mut resolved = String::new();
        let mut drop_arg = false;
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                resolved.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => resolved.push('%'),
                Some('c') => resolved.push_str(name),
                Some('k') => resolved.push_str(&path.to_string_lossy()),
                Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' | 'i') => drop_arg = true,
                Some(_) | None => {} // unknown code: ignore it
            }
        }
        // An argument that was empty to begin with stays; one that only held an
        // ignored code does not.
        if !drop_arg && (!resolved.is_empty() || arg.is_empty()) {
            out.push(resolved);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Quote for POSIX `sh`: leave safe words alone, single-quote the rest.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

fn shell_quote(a: &str) -> String {
    let safe = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Is `prog` an executable file, either as given (absolute) or on `PATH`?
pub fn in_path(prog: &str) -> bool {
    let is_exe = |p: &Path| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if prog.contains('/') {
        return is_exe(Path::new(prog));
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| is_exe(&d.join(prog))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn env() -> ScanEnv {
        ScanEnv { dirs: vec![], desktops: vec!["Veil".into()], terminal: Some(vec!["foot".into(), "-e".into()]) }
    }

    fn parse_str(content: &str) -> Option<App> {
        parse("x.desktop", Path::new("/usr/share/applications/x.desktop"), content, &env())
    }

    fn entry(extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName=Thing\nExec=thing\n{extra}\n")
    }

    #[test]
    fn reads_the_fields_and_ignores_localised_ones() {
        let app = parse_str(
            "[Desktop Entry]\nType=Application\nName=Files\nName[de]=Dateien\nGenericName=File Manager\nComment=Browse\\sfiles\nIcon=folder\nExec=nautilus --new-window %U\nKeywords=folder;disk;\nCategories=System;FileManager;\n",
        )
        .unwrap();
        assert_eq!(app.name, "Files");
        assert_eq!(app.generic_name, "File Manager");
        assert_eq!(app.comment, "Browse files");
        assert_eq!(app.icon, "folder");
        assert_eq!(app.exec, "nautilus --new-window");
        assert_eq!(app.program, "nautilus");
        assert_eq!(app.keywords, ["folder", "disk"]);
        assert_eq!(app.categories, ["System", "FileManager"]);
        assert_eq!(app.name_lc, "files");
    }

    #[test]
    fn only_the_main_group_counts() {
        let app = parse_str("[Desktop Entry]\nName=A\nExec=a\n[Desktop Action new]\nName=New\nExec=a --new\n").unwrap();
        assert_eq!((app.name.as_str(), app.exec.as_str()), ("A", "a"));
        assert!(parse_str("[Other]\nName=A\nExec=a\n").is_none());
    }

    #[test]
    fn hidden_links_and_incomplete_entries_are_dropped() {
        assert!(parse_str(&entry("Hidden=true")).is_none());
        assert!(parse_str(&entry("NoDisplay=true")).is_none());
        assert!(parse_str("[Desktop Entry]\nType=Link\nName=A\nExec=a\n").is_none());
        assert!(parse_str("[Desktop Entry]\nType=Directory\nName=A\nExec=a\n").is_none());
        assert!(parse_str("[Desktop Entry]\nExec=a\n").is_none());
        assert!(parse_str("[Desktop Entry]\nName=A\n").is_none());
        // Type is required by the spec but real files omit it
        assert!(parse_str("[Desktop Entry]\nName=A\nExec=a\n").is_some());
    }

    #[test]
    fn only_show_in_and_not_show_in_follow_the_current_desktop() {
        assert!(parse_str(&entry("OnlyShowIn=GNOME;KDE;")).is_none());
        assert!(parse_str(&entry("OnlyShowIn=GNOME;Veil;")).is_some());
        assert!(parse_str(&entry("NotShowIn=Veil;")).is_none());
        assert!(parse_str(&entry("NotShowIn=GNOME;")).is_some());
        // an unset desktop shows nothing that names desktops
        let mut e = env();
        e.desktops.clear();
        let p = |c: &str| parse("x.desktop", Path::new("/x"), c, &e);
        assert!(p(&entry("OnlyShowIn=GNOME;")).is_none());
        assert!(p(&entry("NotShowIn=GNOME;")).is_some());
    }

    #[test]
    fn try_exec_hides_what_is_not_installed() {
        assert!(parse_str(&entry("TryExec=/definitely/not/installed")).is_none());
        assert!(parse_str(&entry("TryExec=definitely-not-installed-veil")).is_none());
        assert!(parse_str(&entry("TryExec=sh")).is_some());
        assert!(parse_str(&entry("TryExec=/bin/sh")).is_some());
    }

    #[test]
    fn exec_quoting_and_field_codes() {
        let argv = |e: &str| exec_argv(e, "My App", "myicon", Path::new("/a/b.desktop"));
        assert_eq!(argv(r#"env FOO="a b" app --file=%f %U"#).unwrap(), ["env", "FOO=a b", "app"]);
        assert_eq!(argv("app %i %c").unwrap(), ["app", "--icon", "myicon", "My App"]);
        assert_eq!(argv("app %i").unwrap(), ["app", "--icon", "myicon"]);
        assert_eq!(argv("app --name=%c --path=%k").unwrap(), ["app", "--name=My App", "--path=/a/b.desktop"]);
        assert_eq!(argv("app 100%%").unwrap(), ["app", "100%"]);
        assert_eq!(argv(r#"sh -c "echo \"hi\" \$HOME \\ done""#).unwrap(), ["sh", "-c", r#"echo "hi" $HOME \ done"#]);
        assert_eq!(argv(r#"app "" x"#).unwrap(), ["app", "", "x"]);
        assert_eq!(exec_argv("app %i", "A", "", Path::new("/x")).unwrap(), ["app"]);
        assert!(argv("%U").is_none());
        assert!(argv("").is_none());
        assert!(argv(r#"app "unterminated"#).is_none());
    }

    #[test]
    fn terminal_apps_are_wrapped_or_skipped() {
        let app = parse_str(&entry("Terminal=true")).unwrap();
        assert_eq!(app.exec, "foot -e thing");
        assert_eq!(app.program, "thing");
        let mut e = env();
        e.terminal = None;
        assert!(parse("x.desktop", Path::new("/x"), &entry("Terminal=true"), &e).is_none());
    }

    #[test]
    fn shell_quoting_survives_a_real_shell() {
        let argv: Vec<String> = ["it's", "a b", "$HOME", "`id`", "x;y", "", "naïve ✓", "--k=v w", "plain-1.2/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n' {}", shell_join(&argv)))
            .output()
            .unwrap();
        let got: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
        assert_eq!(got, argv.iter().map(String::as_str).collect::<Vec<_>>());
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn earlier_directories_win_and_hidden_shadows() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, sys) = (tmp.path().join("user"), tmp.path().join("sys"));
        write(&user, "browser.desktop", "[Desktop Entry]\nName=Mine\nExec=mine\n");
        write(&sys, "browser.desktop", "[Desktop Entry]\nName=Theirs\nExec=theirs\n");
        write(&user, "gone.desktop", "[Desktop Entry]\nName=Gone\nExec=g\nHidden=true\n");
        write(&sys, "gone.desktop", "[Desktop Entry]\nName=Gone\nExec=g\n");
        write(&sys, "only-sys.desktop", "[Desktop Entry]\nName=Sys\nExec=s\n");
        write(&sys, "kde/foo.desktop", "[Desktop Entry]\nName=Foo\nExec=f\n");
        write(&sys, "notes.txt", "not an entry");
        let apps = scan(&ScanEnv { dirs: vec![user, sys], ..env() });
        let names: Vec<(&str, &str)> = apps.iter().map(|a| (a.name.as_str(), a.id.as_str())).collect();
        assert_eq!(names, [("Foo", "kde-foo.desktop"), ("Mine", "browser.desktop"), ("Sys", "only-sys.desktop")]);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        assert!(scan(&ScanEnv { dirs: vec![PathBuf::from("/definitely/not/here")], ..env() }).is_empty());
    }

    #[test]
    fn list_handles_escaped_semicolons() {
        assert_eq!(list("a;b\\;c;;d"), ["a", "b;c", "d"]);
        assert!(list("").is_empty());
    }
}
