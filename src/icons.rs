//! Find an application's icon file from what its audio stream tells us.
//!
//! Most apps don't set `application.icon_name` on their stream, so the icon
//! name is usually looked up through the app's `.desktop` file (matched by
//! desktop id, binary, `StartupWMClass`, `Exec` or `Name`), then resolved
//! through the current icon theme, its inherited themes, hicolor and pixmaps,
//! following the freedesktop layouts (`<theme>/<size>/<context>/` and
//! `<theme>/<context>/<size>/`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

/// What we know about a stream that can lead to its icon.
pub struct IconHints<'a> {
    /// `application.icon_name`
    pub icon_name: Option<&'a str>,
    /// `application.id` or `pipewire.access.portal.app_id` (desktop id)
    pub app_id: Option<&'a str>,
    /// `application.process.binary`
    pub binary: Option<&'a str>,
    /// `application.name`
    pub app_name: &'a str,
}

/// Icon formats we can show on a key (the image crate can't read XPM).
const EXTENSIONS: &[&str] = &["png", "svg"];

/// A PNG at least this large beats a scalable SVG.
const PREFERRED_PNG_SIZE: u32 = 128;

pub fn find_icon(hints: &IconHints) -> Option<PathBuf> {
    if let Some(path) = hints.icon_name.and_then(lookup_icon) {
        return Some(path);
    }

    if let Some(path) = find_desktop_icon(hints).and_then(|icon| lookup_icon(&icon)) {
        return Some(path);
    }

    [Some(hints.app_name), hints.binary]
        .into_iter()
        .flatten()
        .find_map(lookup_icon)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$XDG_DATA_HOME` followed by `$XDG_DATA_DIRS`, with the spec's defaults.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => dirs.extend(home().map(|h| h.join(".local/share"))),
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs
}

// ---------------------------------------------------------------- desktop files

struct DesktopEntry {
    id: String,
    name: Option<String>,
    exec: Option<String>,
    wm_class: Option<String>,
    icon: String,
}

/// Find the best-matching `.desktop` file for the stream and return its `Icon=`.
fn find_desktop_icon(hints: &IconHints) -> Option<String> {
    let app_id = hints.app_id.map(str::to_lowercase);
    let binary = hints.binary.map(str::to_lowercase);
    let app_name = hints.app_name.to_lowercase();

    let score = |e: &DesktopEntry| -> Option<u8> {
        let id = e.id.to_lowercase();
        let is_name = |s: &str| s == app_name || binary.as_deref() == Some(s);

        if app_id.as_deref() == Some(id.as_str()) {
            Some(0)
        } else if is_name(&id) || id.rsplit('.').next().is_some_and(is_name) {
            Some(1)
        } else if e.wm_class.as_deref().map(str::to_lowercase).is_some_and(|c| is_name(&c)) {
            Some(2)
        } else if binary.is_some()
            && e.exec.as_deref().and_then(exec_binary).map(|b| b.to_lowercase()) == binary
        {
            Some(3)
        } else if e.name.as_deref().map(str::to_lowercase).as_deref() == Some(app_name.as_str()) {
            Some(4)
        } else {
            None
        }
    };

    desktop_entries()
        .into_iter()
        .filter_map(|e| score(&e).map(|s| (s, e)))
        .min_by_key(|(s, _)| *s)
        .map(|(_, e)| e.icon)
}

/// All `.desktop` files with an `Icon=`, earlier data dirs shadowing later
/// ones for the same desktop id.
fn desktop_entries() -> Vec<DesktopEntry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for dir in data_dirs() {
        let mut files = Vec::new();
        collect_desktop_files(&dir.join("applications"), &mut files, 0);
        for path in files {
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !seen.insert(id.to_string()) {
                continue;
            }
            if let Some(entry) = parse_desktop_file(&path, id) {
                entries.push(entry);
            }
        }
    }
    entries
}

fn collect_desktop_files(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < 3 {
                collect_desktop_files(&path, out, depth + 1);
            }
        } else if path.extension().is_some_and(|e| e == "desktop") {
            out.push(path);
        }
    }
}

fn parse_desktop_file(path: &Path, id: &str) -> Option<DesktopEntry> {
    let text = fs::read_to_string(path).ok()?;
    let mut in_main = false;
    let (mut name, mut exec, mut wm_class, mut icon) = (None, None, None, None);

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = Some(value.trim().to_string());
        match key.trim() {
            "Name" => name = value,
            "Exec" => exec = value,
            "StartupWMClass" => wm_class = value,
            "Icon" => icon = value,
            _ => {}
        }
    }

    Some(DesktopEntry {
        id: id.to_string(),
        name,
        exec,
        wm_class,
        icon: icon.filter(|i| !i.is_empty())?,
    })
}

/// Basename of the program an `Exec=` line runs, skipping `env` and
/// `VAR=value` prefixes.
fn exec_binary(exec: &str) -> Option<&str> {
    exec.split_whitespace()
        .map(|t| t.trim_matches('"'))
        .find(|t| !t.contains('=') && Path::new(t).file_name().is_some_and(|f| f != "env"))
        .and_then(|t| Path::new(t).file_name()?.to_str())
}

// ---------------------------------------------------------------- icon themes

/// Base dirs that hold icon themes: `~/.icons`, then `<data dir>/icons`.
static ICON_BASE_DIRS: LazyLock<Vec<PathBuf>> = LazyLock::new(|| {
    let mut dirs: Vec<PathBuf> = home().map(|h| h.join(".icons")).into_iter().collect();
    dirs.extend(data_dirs().into_iter().map(|d| d.join("icons")));
    dirs
});

/// The current icon theme followed by everything it inherits, then hicolor.
static THEME_CHAIN: LazyLock<Vec<String>> = LazyLock::new(|| {
    let current = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_matches('\'').to_string())
        .filter(|s| !s.is_empty());

    let mut chain: Vec<String> = Vec::new();
    let mut queue: Vec<String> = current.into_iter().collect();
    while !queue.is_empty() {
        let theme = queue.remove(0);
        if chain.contains(&theme) || theme == "hicolor" {
            continue;
        }
        let inherits = theme_inherits(&theme);
        chain.push(theme);
        queue.extend(inherits);
    }
    chain.push("hicolor".to_string());
    chain
});

fn theme_inherits(theme: &str) -> Vec<String> {
    ICON_BASE_DIRS
        .iter()
        .find_map(|base| fs::read_to_string(base.join(theme).join("index.theme")).ok())
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix("Inherits=").map(str::to_string))
        })
        .map(|v| v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default()
}

/// Resolve an icon name (or absolute path) to the best file for it.
pub fn lookup_icon(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let as_path = Path::new(name);
    if as_path.is_absolute() {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }

    // Some apps put the extension in Icon=
    let name = EXTENSIONS
        .iter()
        .find_map(|ext| name.strip_suffix(&format!(".{ext}")))
        .unwrap_or(name);

    for theme in THEME_CHAIN.iter() {
        let mut candidates = Vec::new();
        for base in ICON_BASE_DIRS.iter() {
            find_in_theme(&base.join(theme), name, &mut candidates);
        }
        if let Some(best) = pick_best(candidates) {
            return Some(best);
        }
    }

    data_dirs()
        .into_iter()
        .map(|d| d.join("pixmaps"))
        .chain(std::iter::once(PathBuf::from("/usr/share/pixmaps")))
        .flat_map(|dir| EXTENSIONS.iter().map(move |ext| dir.join(format!("{name}.{ext}"))))
        .find(|p| p.is_file())
}

/// Check `<theme>/<a>/<b>/<name>.<ext>` for both size/context layouts,
/// skipping symbolic (monochrome) icons.
fn find_in_theme(theme_dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(level1) = fs::read_dir(theme_dir) else {
        return;
    };
    for a in level1.flatten() {
        let a = a.path();
        if !a.is_dir() || is_symbolic_dir(&a) {
            continue;
        }
        let Ok(level2) = fs::read_dir(&a) else {
            continue;
        };
        for b in level2.flatten() {
            let b = b.path();
            if !b.is_dir() || is_symbolic_dir(&b) {
                continue;
            }
            for ext in EXTENSIONS {
                let file = b.join(format!("{name}.{ext}"));
                if file.is_file() {
                    out.push(file);
                }
            }
        }
    }
}

fn is_symbolic_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("symbolic"))
}

/// Pixel size from a path component like `48x48`, `48x48@2` or `48`.
fn icon_size(path: &Path) -> Option<u32> {
    path.components().rev().skip(1).find_map(|c| {
        let s = c.as_os_str().to_str()?;
        let (base, scale) = match s.split_once('@') {
            Some((b, sc)) => (b, sc.trim_end_matches('x').parse::<u32>().ok()?),
            None => (s, 1),
        };
        let size = base.split('x').next()?.parse::<u32>().ok()?;
        Some(size * scale)
    })
}

/// Largest PNG if it's big enough, else a scalable SVG, else the largest PNG.
fn pick_best(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    let (svgs, pngs): (Vec<PathBuf>, Vec<PathBuf>) = candidates
        .into_iter()
        .partition(|p| p.extension().is_some_and(|e| e == "svg"));

    let largest_png = pngs
        .into_iter()
        .max_by_key(|p| icon_size(p).unwrap_or(0));

    match largest_png {
        Some(png) if icon_size(&png).unwrap_or(0) >= PREFERRED_PNG_SIZE => Some(png),
        png => svgs.into_iter().next().or(png),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_binary_skips_env() {
        assert_eq!(exec_binary("firefox-bin --name=firefox-bin %u"), Some("firefox-bin"));
        assert_eq!(exec_binary("env FOO=1 /usr/bin/foo %F"), Some("foo"));
    }

    #[test]
    fn icon_sizes() {
        assert_eq!(icon_size(Path::new("/i/hicolor/48x48/apps/x.png")), Some(48));
        assert_eq!(icon_size(Path::new("/i/Papirus/apps/64/x.png")), Some(64));
        assert_eq!(icon_size(Path::new("/i/t/32x32@2/apps/x.png")), Some(64));
        assert_eq!(icon_size(Path::new("/i/t/scalable/apps/x.svg")), None);
    }
}
