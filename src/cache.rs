use crate::decode::{DecodedImage, ImageSize};
use crate::loader::{LoadResult, PreparedImage};
use crate::view::FrameParams;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CachedFrame {
    pub params: FrameParams,
    pub result: Result<PreparedImage, String>,
}

impl CachedFrame {
    fn size_bytes(&self) -> usize {
        self.result
            .as_ref()
            .map_or(0, |ready| ready.frame.px.len() * size_of::<u32>())
    }
}

#[derive(Default)]
pub struct ImageCacheEntry {
    fit: Option<CachedFrame>,
    zoom: Option<CachedFrame>,
    pub original: Option<Arc<DecodedImage>>,
    last_used: u64,
}

impl ImageCacheEntry {
    fn frame_slot(&self, params: FrameParams) -> &Option<CachedFrame> {
        if params.is_zoom() {
            &self.zoom
        } else {
            &self.fit
        }
    }

    pub fn frame(&self, params: FrameParams) -> Option<&CachedFrame> {
        self.frame_slot(params)
            .as_ref()
            .filter(|frame| frame.params == params)
    }

    fn is_empty(&self) -> bool {
        self.fit.is_none() && self.zoom.is_none() && self.original.is_none()
    }
}

#[derive(Default)]
struct ImageInfo {
    source_size: Option<ImageSize>,
    revision: u64,
}

pub struct ImageCache {
    entries: HashMap<PathBuf, ImageCacheEntry>,
    info: HashMap<PathBuf, ImageInfo>,
    budget_bytes: usize,
    serial: u64,
}

impl ImageCache {
    pub fn new(budget_mib: usize) -> Self {
        Self {
            entries: HashMap::new(),
            info: HashMap::new(),
            budget_bytes: budget_mib.saturating_mul(1 << 20),
            serial: 0,
        }
    }

    pub fn get(&self, path: &Path) -> Option<&ImageCacheEntry> {
        self.entries.get(path)
    }

    pub fn invalidate(&mut self, path: &Path) {
        self.serial += 1;
        self.entries.remove(path);
        self.info.insert(
            path.to_owned(),
            ImageInfo {
                revision: self.serial,
                ..ImageInfo::default()
            },
        );
    }

    pub fn revision(&self, path: &Path) -> u64 {
        self.info.get(path).map_or(0, |info| info.revision)
    }

    pub fn source_size(&self, path: &Path) -> Option<ImageSize> {
        self.info.get(path)?.source_size
    }

    pub fn touch(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            self.serial += 1;
            entry.last_used = self.serial;
        }
    }

    pub fn insert(&mut self, loaded: LoadResult, wanted: FrameParams) -> bool {
        let job = loaded.job;
        if self.revision(&job.path) != job.revision {
            return false;
        }
        let entry = self.entries.entry(job.path.clone()).or_default();
        self.serial += 1;
        entry.last_used = self.serial;
        if let Some(original) = loaded.original {
            self.info.entry(job.path).or_default().source_size = Some(original.upright_size(0));
            entry.original = Some(original);
        }
        let slot = if job.params.is_zoom() {
            &mut entry.zoom
        } else {
            &mut entry.fit
        };
        let is_draft = loaded.result.as_ref().is_ok_and(|ready| ready.is_draft);
        if slot.as_ref().is_some_and(|frame| {
            (frame.params == wanted && frame.params != job.params)
                || (is_draft
                    && frame.params == job.params
                    && frame.result.as_ref().is_ok_and(|ready| !ready.is_draft))
        }) {
            return false;
        }
        *slot = Some(CachedFrame {
            params: job.params,
            result: loaded.result,
        });
        true
    }

    pub fn frame_cost(&self, path: &Path, params: FrameParams) -> usize {
        let frame = self.get(path).and_then(|entry| entry.frame(params));
        if let Some(frame) = frame.filter(|frame| {
            frame.result.as_ref().is_err()
                || frame.result.as_ref().is_ok_and(|ready| !ready.is_draft)
        }) {
            return frame.size_bytes();
        }
        let estimated = self
            .info
            .get(path)
            .and_then(|info| info.source_size)
            .map_or_else(
                || params.area.frame_bytes(),
                |size| params.size(size).frame_bytes(),
            );
        estimated.max(frame.map_or(0, CachedFrame::size_bytes))
    }

    pub fn prefetch_bytes(&self, current: &Path, params: FrameParams) -> usize {
        let mut left = self
            .budget_bytes
            .saturating_sub(self.frame_cost(current, params));
        if params.is_zoom() {
            let original = self.get(current).and_then(|entry| entry.original.as_ref());
            if let Some(left_after) = original.and_then(|img| left.checked_sub(img.px.len())) {
                left = left_after;
            }
        }
        left
    }

    pub fn trim(
        &mut self,
        current: Option<(&Path, FrameParams)>,
        selected: &HashMap<&Path, FrameParams>,
    ) {
        let mut left = self.budget_bytes;
        if let Some((path, params)) = current.filter(|(_, params)| params.is_zoom()) {
            let frame_bytes = self.frame_cost(path, params);
            if let Some(entry) = self.entries.get_mut(path) {
                let mut after_frame = left.saturating_sub(frame_bytes);
                let before_original = after_frame;
                keep_original(&mut entry.original, &mut after_frame);
                left -= before_original - after_frame;
            }
        }
        for (path, params) in current
            .iter()
            .copied()
            .chain(selected.iter().map(|(&path, &params)| (path, params)))
        {
            if let Some(frame) = self.get(path).and_then(|entry| entry.frame(params)) {
                left = left.saturating_sub(frame.size_bytes());
            }
        }

        let mut newest_first: Vec<_> = self.entries.iter_mut().collect();
        newest_first.sort_unstable_by_key(|(_, entry)| Reverse(entry.last_used));
        for (path, entry) in &mut newest_first {
            let is_current = current.is_some_and(|(cur, _)| cur == path.as_path());
            let active = if is_current {
                current.map(|(_, params)| params)
            } else {
                selected.get(path.as_path()).copied()
            };
            for slot in [&mut entry.fit, &mut entry.zoom] {
                let Some(frame) = slot else { continue };
                if Some(frame.params) == active {
                    continue;
                }
                if let Some(left_after) = left.checked_sub(frame.size_bytes()) {
                    left = left_after;
                } else {
                    *slot = None;
                }
            }
        }
        for (path, entry) in newest_first {
            if !current.is_some_and(|(cur, params)| cur == path.as_path() && params.is_zoom()) {
                keep_original(&mut entry.original, &mut left);
            }
        }
        self.entries.retain(|_, entry| !entry.is_empty());
    }
}

fn keep_original(original: &mut Option<Arc<DecodedImage>>, left: &mut usize) {
    if let Some(img) = original {
        if let Some(left_after) = left.checked_sub(img.px.len()) {
            *left = left_after;
        } else {
            *original = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::RenderedImage;
    use crate::loader::LoadJob;
    use crate::view::{ScaleMode, View};

    fn loaded(path: &Path, params: FrameParams, original_bytes: usize) -> LoadResult {
        let source = DecodedImage {
            w: 512,
            h: 512,
            px: vec![0; original_bytes],
            turns: 0,
        };
        let size = params.size(source.upright_size(0));
        let frame = Arc::new(RenderedImage {
            w: size.w,
            h: size.h,
            px: vec![0; size.frame_bytes() / 4],
        });
        LoadResult {
            job: LoadJob {
                idx: 0,
                path: path.to_owned(),
                params,
                revision: 0,
                original: None,
            },
            result: Ok(PreparedImage {
                frame,
                is_draft: false,
            }),
            original: Some(Arc::new(source)),
        }
    }

    #[test]
    fn test_fit_and_zoom_survive_original_eviction() {
        let mut cache = ImageCache::new(1);
        let here = Path::new("here");
        let next = Path::new("next");
        let mut view = View::new();
        let area = ImageSize { w: 320, h: 240 };
        let fit = view.frame_params(area, true);
        view.scale = ScaleMode::Zoom(1.0);
        let zoom = view.frame_params(area, false);

        cache.insert(loaded(next, fit, 768 << 10), zoom);
        cache.insert(loaded(next, zoom, 768 << 10), zoom);
        cache.trim(Some((here, zoom)), &HashMap::from([(next, zoom)]));
        let entry = cache.get(next).unwrap();
        assert!(entry.original.is_none());
        assert!(entry.frame(zoom).is_some());
        assert!(entry.frame(fit).is_some());

        cache.trim(Some((next, zoom)), &HashMap::new());
        assert!(cache.get(next).unwrap().frame(zoom).is_some());
        assert_eq!(cache.source_size(next), Some(ImageSize { w: 512, h: 512 }));
    }

    #[test]
    fn test_oversized_frame_is_measured_after_eviction() {
        let mut cache = ImageCache::new(1);
        let path = Path::new("large");
        let params = View::new().frame_params(ImageSize { w: 512, h: 512 }, false);
        cache.insert(loaded(path, params, 768 << 10), params);
        let bytes = cache.frame_cost(path, params);
        let here = Path::new("current");
        cache.insert(loaded(here, params, 768 << 10), params);
        cache.trim(Some((here, params)), &HashMap::new());
        assert!(cache.get(path).is_none());
        assert_eq!(cache.frame_cost(path, params), bytes);
        assert_eq!(cache.prefetch_bytes(here, params), 0);
    }

    #[test]
    fn test_stale_resize_and_reload_results() {
        let mut cache = ImageCache::new(1);
        let path = Path::new("image");
        let view = View::new();
        let before = view.frame_params(ImageSize { w: 64, h: 64 }, true);
        let after = view.frame_params(ImageSize { w: 64, h: 32 }, true);

        cache.insert(loaded(path, after, 768 << 10), after);
        assert!(!cache.insert(loaded(path, before, 768 << 10), after));
        assert!(cache.get(path).unwrap().frame(after).is_some());
        cache.invalidate(path);
        assert!(!cache.insert(loaded(path, after, 768 << 10), after));
        assert!(cache.get(path).is_none());
    }
}
