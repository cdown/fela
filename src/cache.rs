use crate::decode::{DecodedImage, FitParams, RenderedImage};
use crate::loader::LoadResult;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CachedImage {
    pub fitted: Arc<RenderedImage>,
    pub original: Option<Arc<DecodedImage>>,
    pub upright_size: (u32, u32),
    pub is_draft: bool,
}

pub struct ImageCacheEntry {
    pub path: PathBuf,
    pub fit_params: FitParams,
    pub result: Result<CachedImage, String>,
}

impl ImageCacheEntry {
    pub fn is_draft(&self) -> bool {
        self.result.as_ref().is_ok_and(|ready| ready.is_draft)
    }

    fn size_bytes(&self) -> usize {
        self.result.as_ref().map_or(0, |ready| {
            ready.fitted.px.len() * 4 + ready.original.as_ref().map_or(0, |img| img.px.len())
        })
    }
}

pub struct ImageCache {
    entries: HashMap<usize, ImageCacheEntry>, // Keyed by position in the file list
    budget_bytes: usize,
}

impl ImageCache {
    pub fn new(budget_mib: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes: budget_mib.saturating_mul(1 << 20),
        }
    }

    pub fn get(&self, idx: usize) -> Option<&ImageCacheEntry> {
        self.entries.get(&idx)
    }

    pub fn remove(&mut self, idx: usize) {
        self.entries.remove(&idx);
    }

    pub fn insert(&mut self, idx: usize, loaded: LoadResult) {
        let result = loaded.result.map(|prepared| CachedImage {
            fitted: prepared.fitted,
            original: prepared.original,
            upright_size: prepared.upright_size,
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
            fit_params: loaded.job.fit_params,
            result,
        };
        self.entries.insert(idx, entry);
    }

    pub fn trim(&mut self, cur: usize) {
        let used: usize = self.entries.values().map(ImageCacheEntry::size_bytes).sum();
        let mut over = used.saturating_sub(self.budget_bytes);
        let mut furthest_first: Vec<_> = self.entries.keys().copied().collect();
        furthest_first.sort_unstable_by_key(|idx| Reverse(idx.abs_diff(cur)));

        // Decoded originals are ~10x the size of what goes on screen and can be decoded again, so
        // get rid of those first.
        for idx in &furthest_first {
            if over == 0 {
                return;
            }
            let original = self
                .entries
                .get_mut(idx)
                .and_then(|entry| entry.result.as_mut().ok())
                .and_then(|ready| ready.original.take());
            over = over.saturating_sub(original.map_or(0, |img| img.px.len()));
        }
        for idx in furthest_first.iter().filter(|idx| **idx != cur) {
            if over == 0 {
                return;
            }
            let entry = self.entries.remove(idx);
            over = over.saturating_sub(entry.map_or(0, |entry| entry.size_bytes()));
        }
    }

    pub fn reindex(&mut self, position: impl Fn(&Path) -> Option<usize>) {
        self.entries = std::mem::take(&mut self.entries)
            .into_values()
            .filter_map(|entry| Some((position(&entry.path)?, entry)))
            .collect();
    }
}
