use std::collections::HashSet;
use std::path::{Path, PathBuf};

const IMAGE_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "arw"];

pub struct FileList {
    list: Vec<PathBuf>,
    cur: usize,
    marked: HashSet<PathBuf>,
    pub is_listing: bool,
    pending_go_to: Option<usize>,
    pending_step: isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Navigation {
    Moved,
    Unchanged,
    Deferred,
}

fn has_image_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMAGE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn is_file_arg(arg: &Path) -> bool {
    has_image_ext(arg) || !arg.is_dir()
}

fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let readable = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let entries = match std::fs::read_dir(readable) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("failed to list {}: {err}", dir.display());
            return Vec::new();
        }
    };

    // DirEntry::path() would be the obvious choice, but we want paths relative to what the user
    // gave us, not to the "." we substituted above.
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| dir.join(entry.file_name()))
        .filter(|path| has_image_ext(path))
        .collect();
    paths.sort_unstable();
    paths
}

pub fn expand_args(args: &[PathBuf]) -> Vec<PathBuf> {
    if let [file] = args {
        if is_file_arg(file) {
            let mut paths = list_dir(file.parent().unwrap_or(Path::new("")));
            if !paths.contains(file) {
                paths.push(file.clone());
                paths.sort_unstable();
            }
            return paths;
        }
    }
    args.iter()
        .flat_map(|arg| {
            if is_file_arg(arg) {
                vec![arg.clone()]
            } else {
                list_dir(arg)
            }
        })
        .collect()
}

impl FileList {
    pub fn new(args: &[PathBuf]) -> Self {
        let list = args
            .first()
            .filter(|arg| has_image_ext(arg))
            .map(|arg| vec![arg.clone()])
            .unwrap_or_default();
        Self {
            list,
            cur: 0,
            marked: HashSet::new(),
            is_listing: true,
            pending_go_to: None,
            pending_step: 0,
        }
    }

    pub fn set_list(&mut self, list: Vec<PathBuf>) {
        let cur = self
            .current()
            .and_then(|cur| list.iter().position(|p| p == cur));
        self.cur = cur.unwrap_or(0);
        self.list = list;
        self.is_listing = false;
        if let Some(idx) = self.pending_go_to.take() {
            self.go_to(idx);
        }
        let pending_step = std::mem::take(&mut self.pending_step);
        self.step(pending_step);
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn index(&self) -> usize {
        self.cur
    }

    pub fn get(&self, idx: usize) -> Option<&PathBuf> {
        self.list.get(idx)
    }

    pub fn position(&self, hint: usize, path: &Path) -> Option<usize> {
        if self.get(hint).is_some_and(|p| p == path) {
            return Some(hint);
        }
        self.list.iter().position(|p| p == path)
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.list.get(self.cur)
    }

    pub fn go_to(&mut self, idx: usize) -> Navigation {
        if self.is_listing {
            self.pending_go_to = Some(idx);
            self.pending_step = 0;
            return Navigation::Deferred;
        }
        let idx = idx.min(self.list.len().saturating_sub(1));
        if idx == self.cur {
            return Navigation::Unchanged;
        }
        self.cur = idx;
        Navigation::Moved
    }

    pub fn step(&mut self, delta: isize) -> Navigation {
        if self.is_listing {
            self.pending_step = self.pending_step.saturating_add(delta);
            return Navigation::Deferred;
        }
        self.go_to(self.cur.saturating_add_signed(delta))
    }

    pub fn is_marked(&self, idx: usize) -> bool {
        self.get(idx).is_some_and(|path| self.marked.contains(path))
    }

    pub fn mark(&mut self) {
        if let Some(path) = self.current().cloned() {
            self.marked.insert(path);
        }
    }

    pub fn unmark(&mut self) {
        if let Some(path) = self.list.get(self.cur) {
            self.marked.remove(path);
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    pub fn marked(&self) -> Vec<PathBuf> {
        self.list
            .iter()
            .filter(|path| self.marked.contains(*path))
            .cloned()
            .collect()
    }

    pub fn step_marked(&mut self, n: isize) -> Navigation {
        let marked = (0..self.list.len()).filter(|idx| self.is_marked(*idx));
        let nr_skipped = n.unsigned_abs().saturating_sub(1);
        let target = match n {
            0 => None,
            1.. => marked.filter(|idx| *idx > self.cur).nth(nr_skipped),
            _ => marked.filter(|idx| *idx < self.cur).rev().nth(nr_skipped),
        };
        match target {
            Some(idx) => self.go_to(idx),
            None => Navigation::Unchanged,
        }
    }

    pub fn remove(&mut self, paths: &[PathBuf]) {
        let nr_kept_before = self.list[..self.cur.min(self.list.len())]
            .iter()
            .filter(|path| !paths.contains(path))
            .count();
        self.list.retain(|path| !paths.contains(path));
        self.marked.retain(|path| !paths.contains(path));
        self.cur = nr_kept_before.min(self.list.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str], cur: usize) -> FileList {
        let mut files = FileList::new(&[]);
        files.set_list(names.iter().map(PathBuf::from).collect());
        files.go_to(cur);
        files
    }

    fn mark(files: &mut FileList, idxs: &[usize]) {
        let cur = files.index();
        for idx in idxs {
            files.go_to(*idx);
            files.mark();
        }
        files.go_to(cur);
    }

    #[test]
    fn test_expand_args_lone_file_stands_for_its_directory() {
        let dir = std::env::temp_dir().join(format!("fela-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["b.jpg", "a.png", "c.ARW", "notes.txt", "clip.mp4"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let images = [dir.join("a.png"), dir.join("b.jpg"), dir.join("c.ARW")];

        assert_eq!(expand_args(&[dir.join("b.jpg")]), images);
        assert_eq!(expand_args(std::slice::from_ref(&dir)), images);
        let several = [dir.join("c.ARW"), dir.join("a.png")];
        assert_eq!(expand_args(&several), several);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_mark_and_unmark_are_idempotent() {
        let mut files = files(&["a"], 0);
        files.mark();
        files.mark();
        assert!(files.is_marked(0));
        files.unmark();
        files.unmark();
        assert!(!files.is_marked(0));
    }

    #[test]
    fn test_step_marked() {
        let mut files = files(&["a", "b", "c", "d", "e"], 2);
        mark(&mut files, &[0, 1, 3, 4]);
        assert_eq!(files.step_marked(2), Navigation::Moved);
        assert_eq!(files.index(), 4);
        assert_eq!(files.step_marked(1), Navigation::Unchanged);
        assert_eq!(files.step_marked(-3), Navigation::Moved);
        assert_eq!(files.index(), 0);
        assert_eq!(files.step_marked(0), Navigation::Unchanged);
    }

    #[test]
    fn test_marked_is_in_list_order() {
        let mut files = files(&["a", "b", "c"], 0);
        mark(&mut files, &[2, 0]);
        assert_eq!(files.marked(), [PathBuf::from("a"), PathBuf::from("c")]);
    }

    #[test]
    fn test_remove_keeps_current() {
        let mut files = files(&["a", "b", "c", "d"], 2);
        files.remove(&[PathBuf::from("a"), PathBuf::from("d")]);
        assert_eq!(files.current(), Some(&PathBuf::from("c")));
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_remove_current_moves_to_next() {
        let mut files = files(&["a", "b", "c", "d"], 1);
        mark(&mut files, &[1]);
        files.remove(&[PathBuf::from("b")]);
        assert_eq!(files.current(), Some(&PathBuf::from("c")));
        assert!(files.marked().is_empty());

        files.go_to(2);
        files.remove(&[PathBuf::from("d")]);
        assert_eq!(files.current(), Some(&PathBuf::from("c")));
    }

    #[test]
    fn test_steps_while_listing_apply_afterwards() {
        let mut files = FileList::new(&[PathBuf::from("b.jpg")]);
        assert_eq!(files.step(3), Navigation::Deferred);
        assert_eq!(files.step(-1), Navigation::Deferred);
        assert_eq!(files.current(), Some(&PathBuf::from("b.jpg")));
        let list = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"];
        files.set_list(list.iter().map(PathBuf::from).collect());
        assert_eq!(files.current(), Some(&PathBuf::from("d.jpg")));
    }

    #[test]
    fn test_go_to_while_listing_applies_afterwards() {
        let mut files = FileList::new(&[PathBuf::from("b.jpg")]);
        assert_eq!(files.step(1), Navigation::Deferred);
        assert_eq!(files.go_to(usize::MAX), Navigation::Deferred);
        assert_eq!(files.step(-1), Navigation::Deferred);
        let list = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"];
        files.set_list(list.iter().map(PathBuf::from).collect());
        assert_eq!(files.current(), Some(&PathBuf::from("d.jpg")));
    }

    #[test]
    fn test_set_list_keeps_current() {
        let mut files = FileList::new(&[PathBuf::from("dir/c.jpg")]);
        assert_eq!(files.index(), 0);
        let list = ["dir/a.jpg", "dir/b.png", "dir/c.jpg", "dir/d.ARW"];
        files.set_list(list.iter().map(PathBuf::from).collect());
        assert_eq!(files.index(), 2);
        assert_eq!(files.len(), 4);
    }
}
