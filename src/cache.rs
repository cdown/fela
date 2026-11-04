use crate::decode::{DecodedImage, RenderedImage};
use crate::loader::{LoadResult, Spec};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CachedImage {
    pub fit: Arc<RenderedImage>,
    pub full: Option<Arc<DecodedImage>>,
    pub size: (u32, u32),
    pub is_draft: bool,
}

pub struct ImageCacheEntry {
    pub path: PathBuf,
    pub spec: Spec,
    pub result: Result<CachedImage, String>,
}

impl ImageCacheEntry {
    pub fn is_draft(&self) -> bool {
        self.result.as_ref().is_ok_and(|ready| ready.is_draft)
    }

    fn size(&self) -> usize {
        self.result.as_ref().map_or(0, |ready| {
            ready.fit.px.len() * 4 + ready.full.as_ref().map_or(0, |full| full.px.len())
        })
    }
}

pub struct ImageCache {
    entries: HashMap<usize, ImageCacheEntry>, // Keyed by position in the file list
    budget: usize,
}

impl ImageCache {
    pub fn new(budget_mb: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget: budget_mb.saturating_mul(1 << 20),
        }
    }

    pub fn get(&self, idx: usize) -> Option<&ImageCacheEntry> {
        self.entries.get(&idx)
    }

    pub fn insert(&mut self, idx: usize, loaded: LoadResult) {
        let result = loaded.result.map(|prepared| CachedImage {
            fit: prepared.fit,
            full: prepared.full,
            size: prepared.size,
            is_draft: prepared.is_draft,
        });
        let is_draft = result.as_ref().is_ok_and(|ready| ready.is_draft);
        let has_real = self
            .get(idx)
            .is_some_and(|entry| entry.result.is_ok() && !entry.is_draft());
        if is_draft && has_real {
            return;
        }
        let entry = ImageCacheEntry {
            path: loaded.job.path,
            spec: loaded.job.spec,
            result,
        };
        self.entries.insert(idx, entry);
    }

    pub fn trim(&mut self, cur: usize) {
        let used: usize = self.entries.values().map(ImageCacheEntry::size).sum();
        let mut over = used.saturating_sub(self.budget);
        let mut furthest_first: Vec<_> = self.entries.keys().copied().collect();
        furthest_first.sort_unstable_by_key(|idx| Reverse(idx.abs_diff(cur)));

        // Decoded originals are ~10x the size of what goes on screen and can be decoded again, so
        // get rid of those first.
        for idx in &furthest_first {
            if over == 0 {
                return;
            }
            let full = self
                .entries
                .get_mut(idx)
                .and_then(|entry| entry.result.as_mut().ok())
                .and_then(|ready| ready.full.take());
            over = over.saturating_sub(full.map_or(0, |full| full.px.len()));
        }
        for idx in furthest_first.iter().filter(|idx| **idx != cur) {
            if over == 0 {
                return;
            }
            let entry = self.entries.remove(idx);
            over = over.saturating_sub(entry.map_or(0, |entry| entry.size()));
        }
    }

    pub fn reindex(&mut self, position: impl Fn(&Path) -> Option<usize>) {
        self.entries = std::mem::take(&mut self.entries)
            .into_values()
            .filter_map(|entry| Some((position(&entry.path)?, entry)))
            .collect();
    }
}
