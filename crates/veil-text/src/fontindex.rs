//! Installed-font discovery: which files exist and what family each one is.
//!
//! A font family is a *name* ("Inter"), not a file, so to honour
//! `font.family = "Inter"` we need to know which file holds Inter. Reading the
//! name table out of every font on the system is the only reliable way, and
//! doing it on every start would be slow, so the result is cached on disk and
//! only files whose modification time changed are read again.
//!
//! Deliberately dependency-free: it reads just the sfnt table directory and the
//! `name` table of each file (a few KB), never the glyph data, so scanning
//! about a thousand fonts takes tens of milliseconds, not seconds.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// Modification time (seconds since the epoch); lets a rescan skip
    /// unchanged files.
    pub mtime: u64,
    pub family: String,
    /// Lower-cased subfamily: "regular", "bold", "bold italic", ...
    pub style: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontIndex {
    entries: Vec<Entry>,
}

/// Lower-case and drop spaces, hyphens and underscores, so "JetBrains Mono",
/// "JetBrainsMono" and "jetbrains-mono" are the same family.
pub(crate) fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

impl FontIndex {
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find the file for `family`. Matching ignores case, spaces and hyphens;
    /// an exact family match beats a prefix match ("JetBrainsMono Nerd Font"
    /// finds "JetBrainsMono Nerd Font Mono" only if nothing exact exists).
    /// Within a family, `bold` picks the Bold face and otherwise Regular, and
    /// either falls back to whatever the family has.
    pub fn find(&self, family: &str, bold: bool) -> Option<&Entry> {
        let want = norm(family);
        if want.is_empty() {
            return None;
        }
        let mut candidates: Vec<&Entry> =
            self.entries.iter().filter(|e| norm(&e.family) == want).collect();
        if candidates.is_empty() {
            candidates = self.entries.iter().filter(|e| norm(&e.family).starts_with(&want)).collect();
        }
        if candidates.is_empty() {
            return None;
        }
        let styles: &[&str] = if bold {
            &["bold"]
        } else {
            &["regular", "book", "roman", "normal", ""]
        };
        for s in styles {
            if let Some(e) = candidates.iter().find(|e| e.style == *s) {
                return Some(e);
            }
        }
        // Asked for bold but the family has none: regular beats italic-only.
        for s in ["regular", "book", "roman", "normal"] {
            if let Some(e) = candidates.iter().find(|e| e.style == s) {
                return Some(e);
            }
        }
        candidates.first().copied()
    }

    // ── scanning ──

    /// Directories fonts are installed in.
    pub fn font_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        if let Some(d) = data_home {
            dirs.push(d.join("fonts"));
        }
        if let Some(h) = &home {
            dirs.push(h.join(".fonts"));
        }
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
            dirs.push(Path::new(d).join("fonts"));
        }
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|d| seen.insert(d.clone()));
        dirs
    }

    /// Scan the system fonts, reusing `previous` entries whose files have not
    /// changed (pass the cached index; `None` reads everything).
    pub fn scan(previous: Option<&FontIndex>) -> Self {
        Self::scan_dirs(&Self::font_dirs(), previous)
    }

    pub fn scan_dirs(dirs: &[PathBuf], previous: Option<&FontIndex>) -> Self {
        let mut files = Vec::new();
        for d in dirs {
            collect_font_files(d, 0, &mut files);
        }
        files.sort();
        files.dedup();

        let old: std::collections::HashMap<&Path, &Entry> = previous
            .map(|p| p.entries.iter().map(|e| (e.path.as_path(), e)).collect())
            .unwrap_or_default();

        let mut entries = Vec::with_capacity(files.len());
        for path in files {
            let mtime = file_mtime(&path);
            if let Some(e) = old.get(path.as_path()) {
                if e.mtime == mtime {
                    entries.push((*e).clone());
                    continue;
                }
            }
            if let Some((family, style)) = read_names(&path) {
                entries.push(Entry { path, mtime, family, style: style.to_lowercase() });
            }
        }
        Self { entries }
    }

    // ── on-disk cache ──

    /// `$XDG_CACHE_HOME/veil/fonts.tsv` (or `~/.cache/veil/fonts.tsv`).
    pub fn cache_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("veil").join("fonts.tsv"))
    }

    pub fn load_cache() -> Option<Self> {
        Self::parse_cache(&std::fs::read_to_string(Self::cache_path()?).ok()?)
    }

    pub fn save_cache(&self) {
        let Some(path) = Self::cache_path() else { return };
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        let tmp = path.with_extension("tsv.tmp");
        if std::fs::write(&tmp, self.to_cache_text()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    pub fn to_cache_text(&self) -> String {
        let mut out = String::from("veil-fonts 1\n");
        for e in &self.entries {
            let p = e.path.to_string_lossy();
            // Tabs and newlines would break the format; no real font path or
            // family has them, so such entries are simply not cached.
            if [p.as_ref(), e.family.as_str(), e.style.as_str()]
                .iter()
                .any(|s| s.contains(['\t', '\n']))
            {
                continue;
            }
            out.push_str(&format!("{}\t{}\t{}\t{}\n", p, e.mtime, e.family, e.style));
        }
        out
    }

    pub fn parse_cache(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != "veil-fonts 1" {
            return None; // other version: rescan instead of guessing
        }
        let mut entries = Vec::new();
        for line in lines {
            let mut f = line.split('\t');
            let (path, mtime, family, style) = (f.next()?, f.next()?, f.next()?, f.next()?);
            entries.push(Entry {
                path: PathBuf::from(path),
                mtime: mtime.parse().ok()?,
                family: family.to_string(),
                style: style.to_string(),
            });
        }
        Some(Self { entries })
    }
}

fn file_mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn collect_font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_font_files(&path, depth + 1, out);
        } else if is_font_file(&path) {
            // Symlinked fonts are common (fontconfig, nix); follow them.
            out.push(path);
        } else if ft.is_symlink() && path.is_dir() {
            collect_font_files(&path, depth + 1, out);
        }
    }
}

fn is_font_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("ttf" | "otf" | "ttc" | "otc")
    )
}

// ── sfnt name-table reader ───────────────────────────────────────────────────

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Family and style of the first face in a font file, reading only the table
/// directory and the `name` table.
pub fn read_names(path: &Path) -> Option<(String, String)> {
    let mut f = File::open(path).ok()?;
    let mut head = [0u8; 16];
    f.read_exact(&mut head).ok()?;
    // A collection (.ttc/.otc) starts with 'ttcf'; its first face's table
    // directory lives at the offset stored at byte 12.
    let dir_off = if &head[0..4] == b"ttcf" { be32(&head, 12)? as u64 } else { 0 };
    f.seek(SeekFrom::Start(dir_off)).ok()?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr).ok()?;
    let num_tables = be16(&hdr, 4)? as usize;
    if num_tables == 0 || num_tables > 256 {
        return None;
    }
    let mut dir = vec![0u8; num_tables * 16];
    f.read_exact(&mut dir).ok()?;
    let (offset, length) = dir.as_chunks::<16>().0.iter().find_map(|rec| {
        if &rec[0..4] != b"name" {
            return None;
        }
        Some((be32(rec, 8)? as u64, be32(rec, 12)? as usize))
    })?;
    if length == 0 || length > 4 * 1024 * 1024 {
        return None;
    }
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut name = vec![0u8; length];
    f.read_exact(&mut name).ok()?;
    parse_name_table(&name)
}

/// `(family, style)` from the bytes of a `name` table. Prefers the
/// typographic names (IDs 16/17), which hold the real family for fonts that
/// split into many legacy "families", then the legacy ones (1/2).
pub fn parse_name_table(t: &[u8]) -> Option<(String, String)> {
    let count = be16(t, 2)? as usize;
    let strings = be16(t, 4)? as usize;
    let get = |id: u16| -> Option<String> {
        let mut best: Option<(u8, String)> = None;
        for i in 0..count {
            let r = 6 + i * 12;
            let (plat, _enc, lang, nid) = (be16(t, r)?, be16(t, r + 2)?, be16(t, r + 4)?, be16(t, r + 6)?);
            if nid != id {
                continue;
            }
            let (len, off) = (be16(t, r + 8)? as usize, be16(t, r + 10)? as usize);
            let raw = t.get(strings + off..strings + off + len)?;
            let (rank, s) = match plat {
                3 => (if lang == 0x0409 { 0 } else { 1 }, utf16be(raw)),
                0 => (2, utf16be(raw)),
                1 => (3, raw.iter().map(|&b| b as char).collect()),
                _ => continue,
            };
            let s = s.trim().to_string();
            if !s.is_empty() && best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, s));
            }
        }
        best.map(|(_, s)| s)
    };
    let family = get(16).or_else(|| get(1))?;
    let style = get(17).or_else(|| get(2)).unwrap_or_else(|| "Regular".to_string());
    Some((family, style))
}

fn utf16be(raw: &[u8]) -> String {
    let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal but valid sfnt whose only table is a `name` table holding
    /// the given (nameID, string) pairs as Windows/English UTF-16BE records.
    pub(crate) fn fake_sfnt(names: &[(u16, &str)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut records = Vec::new();
        for (id, s) in names {
            let utf16: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
            records.extend_from_slice(&3u16.to_be_bytes()); // platform: Windows
            records.extend_from_slice(&1u16.to_be_bytes()); // encoding: Unicode BMP
            records.extend_from_slice(&0x0409u16.to_be_bytes()); // English (US)
            records.extend_from_slice(&id.to_be_bytes());
            records.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
            records.extend_from_slice(&(strings.len() as u16).to_be_bytes());
            strings.extend_from_slice(&utf16);
        }
        let mut table = Vec::new();
        table.extend_from_slice(&0u16.to_be_bytes()); // format
        table.extend_from_slice(&(names.len() as u16).to_be_bytes());
        table.extend_from_slice(&((6 + records.len()) as u16).to_be_bytes());
        table.extend_from_slice(&records);
        table.extend_from_slice(&strings);

        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // sfnt version
        font.extend_from_slice(&1u16.to_be_bytes()); // numTables
        font.extend_from_slice(&[0u8; 6]); // searchRange, entrySelector, rangeShift
        font.extend_from_slice(b"name");
        font.extend_from_slice(&0u32.to_be_bytes()); // checksum
        font.extend_from_slice(&28u32.to_be_bytes()); // offset: 12 header + 16 record
        font.extend_from_slice(&(table.len() as u32).to_be_bytes());
        font.extend_from_slice(&table);
        font
    }

    fn entry(path: &str, family: &str, style: &str) -> Entry {
        Entry { path: path.into(), mtime: 1, family: family.into(), style: style.into() }
    }

    #[test]
    fn reads_family_and_style_from_a_name_table() {
        let dir = std::env::temp_dir().join(format!("veil-fonts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.ttf");
        std::fs::write(&p, fake_sfnt(&[(1, "Legacy Name"), (2, "Regular"), (16, "Real Family"), (17, "Bold")])).unwrap();
        // Typographic names (16/17) win over the legacy ones (1/2).
        assert_eq!(read_names(&p), Some(("Real Family".into(), "Bold".into())));
        std::fs::write(&p, fake_sfnt(&[(1, "Only Legacy"), (2, "Italic")])).unwrap();
        assert_eq!(read_names(&p), Some(("Only Legacy".into(), "Italic".into())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn garbage_files_are_skipped_not_fatal() {
        let dir = std::env::temp_dir().join(format!("veil-fonts-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in [("empty.ttf", vec![]), ("short.ttf", vec![0, 1, 0, 0]), ("junk.otf", vec![0xff; 4096])] {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_names(&p), None, "{name}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_ignores_case_spaces_and_hyphens() {
        let idx = FontIndex::from_entries(vec![entry("/f/jb.ttf", "JetBrains Mono", "regular")]);
        for q in ["JetBrains Mono", "jetbrainsmono", "jetbrains-mono", "JETBRAINS_MONO"] {
            assert!(idx.find(q, false).is_some(), "{q}");
        }
        assert!(idx.find("Inter", false).is_none());
        assert!(idx.find("   ", false).is_none());
    }

    #[test]
    fn find_prefers_exact_family_then_prefix() {
        let idx = FontIndex::from_entries(vec![
            entry("/f/nfm.ttf", "JetBrainsMono Nerd Font Mono", "regular"),
            entry("/f/jb.ttf", "JetBrainsMono", "regular"),
        ]);
        assert_eq!(idx.find("JetBrainsMono", false).unwrap().path, PathBuf::from("/f/jb.ttf"));
        let only_nf = FontIndex::from_entries(vec![entry("/f/nfm.ttf", "JetBrainsMono Nerd Font Mono", "regular")]);
        assert_eq!(only_nf.find("JetBrainsMono Nerd Font", false).unwrap().path, PathBuf::from("/f/nfm.ttf"));
    }

    #[test]
    fn find_picks_the_requested_weight_with_sane_fallbacks() {
        let idx = FontIndex::from_entries(vec![
            entry("/f/i.ttf", "Inter", "italic"),
            entry("/f/b.ttf", "Inter", "bold"),
            entry("/f/r.ttf", "Inter", "regular"),
        ]);
        assert_eq!(idx.find("Inter", false).unwrap().path, PathBuf::from("/f/r.ttf"));
        assert_eq!(idx.find("Inter", true).unwrap().path, PathBuf::from("/f/b.ttf"));
        let no_bold = FontIndex::from_entries(vec![
            entry("/f/i.ttf", "Inter", "italic"),
            entry("/f/r.ttf", "Inter", "regular"),
        ]);
        assert_eq!(no_bold.find("Inter", true).unwrap().path, PathBuf::from("/f/r.ttf"), "bold missing -> regular, not italic");
        let only_italic = FontIndex::from_entries(vec![entry("/f/i.ttf", "Inter", "italic")]);
        assert!(only_italic.find("Inter", false).is_some(), "any face beats none");
    }

    #[test]
    fn cache_round_trips_and_rejects_other_versions() {
        let idx = FontIndex::from_entries(vec![
            entry("/usr/share/fonts/a b/Inter.ttf", "Inter", "regular"),
            entry("/usr/share/fonts/c.otf", "Noto Sans", "bold"),
        ]);
        assert_eq!(FontIndex::parse_cache(&idx.to_cache_text()), Some(idx));
        assert_eq!(FontIndex::parse_cache("veil-fonts 2\n"), None);
        assert_eq!(FontIndex::parse_cache("nonsense"), None);
        assert_eq!(FontIndex::parse_cache("veil-fonts 1\nonly\ttwo\n"), None);
    }

    #[test]
    fn scan_uses_the_cache_for_unchanged_files_and_rereads_changed_ones() {
        let dir = std::env::temp_dir().join(format!("veil-fonts-scan-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let a = dir.join("sub").join("a.ttf");
        std::fs::write(&a, fake_sfnt(&[(1, "Alpha"), (2, "Regular")])).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a font").unwrap();

        let first = FontIndex::scan_dirs(std::slice::from_ref(&dir), None);
        assert_eq!(first.len(), 1);
        assert_eq!(first.entries()[0].family, "Alpha");

        // Poison the cached family; an unchanged file must keep the cached value.
        let mut poisoned = first.clone();
        poisoned.entries[0].family = "FromCache".into();
        assert_eq!(FontIndex::scan_dirs(std::slice::from_ref(&dir), Some(&poisoned)).entries()[0].family, "FromCache");

        // A changed mtime forces a re-read.
        poisoned.entries[0].mtime = 0;
        assert_eq!(FontIndex::scan_dirs(std::slice::from_ref(&dir), Some(&poisoned)).entries()[0].family, "Alpha");
        std::fs::remove_dir_all(&dir).ok();
    }
}
