use crate::decode::{self, DecodedImage, RenderedImage};
use crate::tiff;
use crate::ViewerEvent;
use anyhow::{ensure, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use winit::event_loop::EventLoopProxy;

/// We check whether a read is still wanted between chunks.
const CHUNK: usize = 1 << 20;

/// Enough of a RAW to find the JPEGs embedded in it. On the A1 II the last IFD ends 145KB in.
const HEAD_LEN: usize = 256 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub w: u32,
    pub h: u32,
    pub upscale: bool,
}

#[derive(Clone)]
pub struct LoadJob {
    pub idx: usize,
    pub path: PathBuf,
    pub spec: Spec,
    /// The decoded image if we still have it, in which case it only needs scaling.
    pub full: Option<Arc<DecodedImage>>,
}

pub struct PreparedImage {
    pub fit: Arc<RenderedImage>,
    pub full: Option<Arc<DecodedImage>>,
    pub is_draft: bool,
}

pub struct LoadResult {
    pub job: LoadJob,
    pub result: Result<PreparedImage, String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Reading,
    Decoding,
}

#[derive(Default)]
struct LoadQueue {
    /// The image the user is looking at, if it still needs loading.
    current: Option<LoadJob>,
    /// Images the user is likely to look at next, most likely first.
    prefetch: Vec<LoadJob>,
    running: Vec<(PathBuf, Phase)>,
}

impl LoadQueue {
    fn phase(&self, path: &Path) -> Option<Phase> {
        self.running
            .iter()
            .find(|(running, _)| running == path)
            .map(|(_, phase)| *phase)
    }

    fn is_wanted(&self, path: &Path) -> bool {
        self.current
            .iter()
            .chain(&self.prefetch)
            .any(|job| job.path == path)
    }

    /// Reads happen one at a time and in priority order, since on slow storage a speculative read
    /// would otherwise delay the one the user is waiting for. Decoding is not limited like this.
    fn may_read(&self, path: &Path) -> Option<bool> {
        if let Some(current) = &self.current {
            if current.path == path {
                return Some(true);
            }
            // The worker reserved for the current image will get to it soon even if it has not
            // been picked up yet, so wait for that as well.
            if current.full.is_none() && self.phase(&current.path) != Some(Phase::Decoding) {
                return self.is_wanted(path).then_some(false);
            }
        }
        let ahead = self.prefetch.iter().position(|job| job.path == path)?;
        let is_blocked = self.prefetch[..ahead]
            .iter()
            .any(|job| self.phase(&job.path) == Some(Phase::Reading));
        Some(!is_blocked)
    }
}

struct SharedLoader {
    state: Mutex<LoadQueue>,
    wake: Condvar,
    proxy: EventLoopProxy<ViewerEvent>,
    /// Dropped once the first image has been read, which lets the directory listing start.
    lister_gate: Mutex<Option<Sender<()>>>,
}

impl SharedLoader {
    fn lock(&self) -> MutexGuard<'_, LoadQueue> {
        self.state.lock().expect("BUG: loader state poisoned")
    }

    fn next_job(&self, is_reserved: bool) -> LoadJob {
        let mut state = self.lock();
        loop {
            let prefetch = if is_reserved {
                &[][..]
            } else {
                &state.prefetch[..]
            };
            let job = state
                .current
                .iter()
                .chain(prefetch)
                .find(|job| state.phase(&job.path).is_none())
                .cloned();
            if let Some(job) = job {
                let phase = if job.full.is_some() {
                    Phase::Decoding
                } else {
                    Phase::Reading
                };
                state.running.push((job.path.clone(), phase));
                return job;
            }
            state = self.wake.wait(state).expect("BUG: loader state poisoned");
        }
    }

    fn wait_for_disk(&self, path: &Path) -> bool {
        let mut state = self.lock();
        loop {
            match state.may_read(path) {
                Some(true) => return true,
                Some(false) => state = self.wake.wait(state).expect("BUG: loader state poisoned"),
                None => return false,
            }
        }
    }

    fn start_decoding(&self, path: &Path) -> bool {
        let mut state = self.lock();
        if let Some(running) = state
            .running
            .iter_mut()
            .find(|(running, _)| running == path)
        {
            running.1 = Phase::Decoding;
        }
        self.wake.notify_all();
        state.is_wanted(path)
    }

    fn finish(&self, path: &Path, is_done: bool) {
        let mut state = self.lock();
        state.running.retain(|(running, _)| running != path);
        if is_done {
            state.current.take_if(|job| job.path == path);
            state.prefetch.retain(|job| job.path != path);
        }
        self.wake.notify_all();
    }

    fn read_range(
        &self,
        path: &Path,
        file: &mut File,
        offset: u64,
        len: usize,
        data: &mut Vec<u8>,
    ) -> Result<bool> {
        file.seek(SeekFrom::Start(offset))?;
        data.reserve_exact(len);
        let mut left = len;
        while left > 0 {
            if !self.wait_for_disk(path) {
                return Ok(false);
            }
            let limit = u64::try_from(left.min(CHUNK))?;
            let nr_read = file.by_ref().take(limit).read_to_end(data)?;
            ensure!(nr_read > 0, "file is shorter than expected");
            left -= nr_read;
        }
        Ok(true)
    }

    fn read_span(
        &self,
        path: &Path,
        file: &mut File,
        head: &[u8],
        span: tiff::ByteRange,
    ) -> Result<Option<Vec<u8>>> {
        let nr_have = head.len().saturating_sub(span.offset).min(span.len);
        let mut data = head
            .get(span.offset..span.offset + nr_have)
            .unwrap_or_default()
            .to_vec();
        let offset = u64::try_from(span.offset + nr_have)?;
        let is_complete = self.read_range(path, file, offset, span.len - nr_have, &mut data)?;
        Ok(is_complete.then_some(data))
    }

    fn send(&self, job: &LoadJob, result: Result<PreparedImage, String>) {
        let loaded = LoadResult {
            job: job.clone(),
            result,
        };
        // Only fails if the event loop has gone away, in which case we are exiting
        let _ = self.proxy.send_event(ViewerEvent::Loaded(loaded));
    }

    /// Decoding the full size JPEG is slow, but the smaller one is usually fast, so draft it
    fn send_draft(&self, job: &LoadJob, data: &[u8], orientation: u16) {
        let fit = decode::decode(data, Some(orientation))
            .and_then(|img| decode::fit(&img, job.spec.w, job.spec.h, true));
        if let Ok(fit) = fit {
            let prepared = PreparedImage {
                fit: Arc::new(fit),
                full: None,
                is_draft: true,
            };
            self.send(job, Ok(prepared));
        }
    }

    fn read(&self, job: &LoadJob) -> Result<Option<(Vec<u8>, Option<u16>)>> {
        let path = &job.path;
        if !self.wait_for_disk(path) {
            return Ok(None);
        }
        let mut file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())?;
        let mut head = Vec::new();
        if !self.read_range(path, &mut file, 0, len.min(HEAD_LEN), &mut head)? {
            return Ok(None);
        }
        if !tiff::has_tiff_magic(&head) {
            let offset = u64::try_from(head.len())?;
            let nr_left = len - head.len();
            let is_complete = self.read_range(path, &mut file, offset, nr_left, &mut head)?;
            return Ok(is_complete.then_some((head, None)));
        }

        let embedded = tiff::find_embedded(&head)?;
        if let Some(draft) = embedded.draft {
            let Some(data) = self.read_span(path, &mut file, &head, draft)? else {
                return Ok(None);
            };
            self.send_draft(job, &data, embedded.orientation);
        }
        let data = self.read_span(path, &mut file, &head, embedded.full)?;
        Ok(data.map(|data| (data, Some(embedded.orientation))))
    }

    /// None means the job was abandoned because it is no longer wanted.
    fn run(&self, job: &LoadJob) -> Result<Option<PreparedImage>> {
        let full = match &job.full {
            Some(full) => Arc::clone(full),
            None => {
                let data = self.read(job);
                self.lister_gate
                    .lock()
                    .expect("BUG: lister gate poisoned")
                    .take();
                let Some((data, orientation)) = data? else {
                    return Ok(None);
                };
                if !self.start_decoding(&job.path) {
                    return Ok(None);
                }
                Arc::new(decode::decode(&data, orientation)?)
            }
        };
        let fit = Arc::new(decode::fit(
            &full,
            job.spec.w,
            job.spec.h,
            job.spec.upscale,
        )?);
        Ok(Some(PreparedImage {
            fit,
            full: Some(full),
            is_draft: false,
        }))
    }

    fn work(&self, is_reserved: bool) {
        loop {
            let job = self.next_job(is_reserved);
            let result = self.run(&job).map_err(|err| format!("{err:#}")).transpose();
            self.finish(&job.path, result.is_some());
            if let Some(result) = result {
                self.send(&job, result);
            }
        }
    }
}

pub struct Loader {
    shared: Arc<SharedLoader>,
}

impl Loader {
    pub fn new(
        nr_prefetch: usize,
        proxy: EventLoopProxy<ViewerEvent>,
        lister_gate: Option<Sender<()>>,
    ) -> Self {
        let shared = Arc::new(SharedLoader {
            state: Mutex::default(),
            wake: Condvar::new(),
            proxy,
            lister_gate: Mutex::new(lister_gate),
        });
        for nr in 0..=nr_prefetch {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.work(nr == 0));
        }
        Self { shared }
    }

    pub fn set_jobs(&self, current: Option<LoadJob>, prefetch: Vec<LoadJob>) {
        let mut state = self.shared.lock();
        state.current = current;
        state.prefetch = prefetch;
        self.shared.wake.notify_all();
    }
}
