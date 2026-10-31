use std::path::{Path, PathBuf};

const IMAGE_EXTS: [&str; 3] = ["jpg", "jpeg", "png"];

pub struct FileList {
    list: Vec<PathBuf>,
    cur: usize,
    pub is_listing: bool,
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
            is_listing: true,
        }
    }

    pub fn set_list(&mut self, list: Vec<PathBuf>) {
        let cur = self
            .current()
            .and_then(|cur| list.iter().position(|p| p == cur));
        self.cur = cur.unwrap_or(0);
        self.list = list;
        self.is_listing = false;
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

    pub fn go_to(&mut self, idx: usize) -> bool {
        let idx = idx.min(self.list.len().saturating_sub(1));
        let changed = idx != self.cur;
        self.cur = idx;
        changed
    }

    pub fn step(&mut self, delta: isize) -> bool {
        self.go_to(self.cur.saturating_add_signed(delta))
    }
}
