//! Notification history: a bounded ring, the unread count, the DND flag and the
//! id counter, persisted to a 0600 file in `$XDG_RUNTIME_DIR` so a notify
//! crash loses nothing and a logout clears it.

use std::collections::VecDeque;
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Limits, Notification};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// What the bell shows; published on the `notifications` IPC topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub count: u32,
    pub dnd: bool,
}

#[derive(Serialize)]
struct DiskRef<'a> {
    version: u32,
    next_id: u32,
    dnd: bool,
    items: &'a VecDeque<Notification>,
}

#[derive(Deserialize)]
struct Disk {
    version: u32,
    next_id: u32,
    dnd: bool,
    items: Vec<Notification>,
}

/// The result of [`Store::load`]: the history plus the id counter to resume from.
pub struct Loaded {
    pub store: Store,
    pub next_id: u32,
}

pub struct Store {
    /// Oldest first.
    items: VecDeque<Notification>,
    dnd: bool,
    cap: usize,
    dirty: bool,
}

impl Store {
    pub fn new(cap: usize) -> Store {
        Store { items: VecDeque::new(), dnd: false, cap: cap.max(1), dirty: false }
    }

    /// Add `n`, replacing any entry with the same id (the replacement counts as
    /// the newest). Returns the ids pushed out by the cap, oldest first.
    pub fn push(&mut self, n: Notification) -> Vec<u32> {
        self.items.retain(|x| x.id != n.id);
        self.items.push_back(n);
        let mut evicted = Vec::new();
        while self.items.len() > self.cap {
            if let Some(old) = self.items.pop_front() {
                evicted.push(old.id);
            }
        }
        self.dirty = true;
        evicted
    }

    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.items.iter().find(|n| n.id == id)
    }

    pub fn remove(&mut self, id: u32) -> Option<Notification> {
        let pos = self.items.iter().position(|n| n.id == id)?;
        self.dirty = true;
        self.items.remove(pos)
    }

    /// Drop everything; returns how many entries went.
    pub fn clear(&mut self) -> usize {
        let n = self.items.len();
        if n > 0 {
            self.items.clear();
            self.dirty = true;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn newest_first(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter().rev()
    }

    pub fn unread(&self) -> u32 {
        self.items.iter().filter(|n| !n.read).count() as u32
    }

    /// Returns whether anything changed.
    pub fn mark_all_read(&mut self) -> bool {
        let mut changed = false;
        for n in self.items.iter_mut().filter(|n| !n.read) {
            n.read = true;
            changed = true;
        }
        self.dirty |= changed;
        changed
    }

    pub fn dnd(&self) -> bool {
        self.dnd
    }

    /// Returns whether the flag changed.
    pub fn set_dnd(&mut self, on: bool) -> bool {
        let changed = self.dnd != on;
        self.dnd = on;
        self.dirty |= changed;
        changed
    }

    pub fn summary(&self) -> Summary {
        Summary { count: self.unread(), dnd: self.dnd }
    }

    /// True once after any change that should reach the file.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Write atomically: temp file (0600) in the same directory, then rename.
    /// `next_id` is the id counter to persist, from [`crate::ids::IdGen::peek`].
    pub fn save(&self, path: &Path, next_id: u32) -> io::Result<()> {
        let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let json = serde_json::to_vec(&DiskRef { version: VERSION, next_id, dnd: self.dnd, items: &self.items })
            .map_err(io::Error::other)?;
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("notify-history.json");
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.set_permissions(Permissions::from_mode(0o600))?;
            f.write_all(&json)?;
            f.flush()
        })();
        match written.and_then(|()| std::fs::rename(&tmp, path)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Read the history back. Anything wrong with the file (missing, foreign,
    /// too big, corrupt, another version) yields an empty store, never an error.
    pub fn load(path: &Path, limits: &Limits) -> Loaded {
        match Self::try_load(path, limits) {
            Ok(Some(loaded)) => loaded,
            Ok(None) => Loaded { store: Store::new(limits.history), next_id: 1 },
            Err(e) => {
                tracing::warn!("ignoring notification history at {}: {e}", path.display());
                Loaded { store: Store::new(limits.history), next_id: 1 }
            }
        }
    }

    fn try_load(path: &Path, limits: &Limits) -> io::Result<Option<Loaded>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(invalid("not a regular file owned by us"));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(invalid("file too large"));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut buf)?;
        let disk: Disk = serde_json::from_slice(&buf).map_err(invalid)?;
        if disk.version != VERSION {
            return Err(invalid("unsupported version"));
        }

        let mut store = Store::new(limits.history);
        for mut n in disk.items {
            if n.id == 0 {
                continue;
            }
            n.normalize(limits);
            store.items.retain(|x| x.id != n.id);
            store.items.push_back(n);
        }
        while store.items.len() > store.cap {
            store.items.pop_front();
        }
        let max_id = store.items.iter().map(|n| n.id).max().unwrap_or(0);
        let floor = if max_id == u32::MAX { 1 } else { max_id + 1 };
        let next_id = disk.next_id.max(floor);
        store.dnd = disk.dnd;
        Ok(Some(Loaded { store, next_id }))
    }
}

fn invalid<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_notification as n;

    #[test]
    fn push_replaces_and_evicts_oldest() {
        let mut s = Store::new(3);
        for id in 1..=3 {
            assert!(s.push(n(id, "x")).is_empty());
        }
        assert_eq!(s.push(n(4, "x")), vec![1]);
        assert_eq!(s.newest_first().map(|n| n.id).collect::<Vec<_>>(), vec![4, 3, 2]);
        // replacing id 2 moves it to the newest slot without evicting
        assert!(s.push(n(2, "new")).is_empty());
        assert_eq!(s.newest_first().map(|n| n.id).collect::<Vec<_>>(), vec![2, 4, 3]);
        assert_eq!(s.get(2).unwrap().summary, "new");
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn unread_read_remove_clear() {
        let mut s = Store::new(10);
        s.push(n(1, "a"));
        s.push(n(2, "b"));
        assert_eq!(s.summary(), Summary { count: 2, dnd: false });
        assert!(s.mark_all_read());
        assert!(!s.mark_all_read(), "second call changes nothing");
        assert_eq!(s.unread(), 0);
        assert_eq!(s.remove(1).unwrap().id, 1);
        assert!(s.remove(1).is_none());
        assert_eq!(s.clear(), 1);
        assert!(s.is_empty());
    }

    #[test]
    fn dnd_and_dirty_tracking() {
        let mut s = Store::new(10);
        assert!(!s.take_dirty());
        assert!(s.set_dnd(true));
        assert!(!s.set_dnd(true), "no change, no dirt");
        assert!(s.take_dirty());
        assert!(!s.take_dirty(), "reading the flag clears it");
        s.push(n(1, "a"));
        assert!(s.take_dirty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("veil").join("notify-history.json");
        let mut s = Store::new(100);
        for id in 1..=3 {
            let mut note = n(id, &format!("note {id}"));
            note.image = Some(crate::model::Image { width: 1, height: 1, rgba: vec![1, 2, 3, 4] });
            s.push(note);
        }
        s.mark_all_read();
        s.set_dnd(true);
        s.save(&path, 4).unwrap();

        let back = Store::load(&path, &Limits::default());
        assert_eq!(back.next_id, 4, "ids continue after a restart");
        let back = back.store;
        assert_eq!(back.len(), 3);
        assert!(back.dnd());
        assert_eq!(back.unread(), 0);
        assert_eq!(back.get(2).unwrap().summary, "note 2");
        assert!(back.get(2).unwrap().image.is_none(), "images are not persisted");
    }

    #[test]
    fn file_and_directory_are_private_and_no_temp_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("veil").join("notify-history.json");
        Store::new(10).save(&path, 1).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "only the history file remains");
    }

    #[test]
    fn bad_files_load_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let lim = Limits::default();
        assert!(Store::load(&path, &lim).store.is_empty(), "missing");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(Store::load(&path, &lim).store.is_empty(), "corrupt");
        std::fs::write(&path, br#"{"version":99,"next_id":5,"dnd":true,"items":[]}"#).unwrap();
        let loaded = Store::load(&path, &lim);
        assert!(!loaded.store.dnd(), "wrong version is ignored whole");
        assert_eq!(loaded.next_id, 1);
        std::fs::write(&path, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert!(Store::load(&path, &lim).store.is_empty(), "oversize");
    }

    #[test]
    fn load_repairs_ids_dupes_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let mut s = Store::new(100);
        for id in [1, 2, 3, 9] {
            s.push(n(id, "x"));
        }
        // a file claiming a counter below its own entries
        s.save(&path, 2).unwrap();
        assert_eq!(Store::load(&path, &Limits::default()).next_id, 10, "counter jumps past the highest id");

        let lim = Limits { history: 2, ..Limits::default() };
        let capped = Store::load(&path, &lim).store;
        assert_eq!(capped.newest_first().map(|n| n.id).collect::<Vec<_>>(), vec![9, 3]);
    }
}
