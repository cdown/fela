use crate::decode::{self, DecodedImage, RenderedImage};
use crate::tiff;
use crate::view::FrameParams;
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

#[derive(Clone)]
pub struct LoadJob {
    pub idx: usize,
    pub path: PathBuf,
    pub params: FrameParams,
    pub revision: u64,
    /// The decoded image if we still have it, in which case it only needs scaling.
    pub original: Option<Arc<DecodedImage>>,
}

struct EncodedImage {
    data: Vec<u8>,
    orientation: Option<u16>,
}

impl LoadJob {
    fn output(&self) -> Self {
        Self {
            path: self.path.clone(),
            original: None,
            ..*self
        }
    }
}

pub struct PreparedImage {
    pub frame: Arc<RenderedImage>,
    pub is_draft: bool,
}

pub struct LoadResult {
    pub job: LoadJob,
    pub result: Result<PreparedImage, String>,
    pub original: Option<Arc<DecodedImage>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Reading,
    Decoding,
    // Keep the job until the UI sees its result so a new request cannot start it again.
    AwaitingAck,
}

struct InFlight {
    path: PathBuf,
    phase: Phase,
    was_current: bool,
}

enum ReadTurn {
    Read,
    Wait,
    Cancelled,
}

#[derive(PartialEq, Eq)]
enum JobStatus {
    Continue,
    Cancelled,
}

#[derive(Default)]
struct LoadQueue {
    /// The image the user is looking at, if it still needs loading.
    current: Option<LoadJob>,
    /// Images the user is likely to look at next, most likely first.
    prefetch: Vec<LoadJob>,
    in_flight: Vec<InFlight>,
}

impl LoadQueue {
    fn wanted(&self, job: &LoadJob) -> Option<&LoadJob> {
        self.current
            .iter()
            .chain(&self.prefetch)
            .find(|wanted| wanted.path == job.path && wanted.revision == job.revision)
    }

    fn in_flight(&self, path: &Path) -> Option<&InFlight> {
        self.in_flight.iter().find(|job| job.path == path)
    }

    fn phase(&self, path: &Path) -> Option<Phase> {
        self.in_flight(path).map(|job| job.phase)
    }

    fn set_phase(&mut self, path: &Path, phase: Phase) {
        if let Some(job) = self.in_flight.iter_mut().find(|job| job.path == path) {
            job.phase = phase;
        }
    }

    fn is_wanted(&self, path: &Path) -> bool {
        self.current
            .iter()
            .chain(&self.prefetch)
            .any(|job| job.path == path)
    }

    /// Reads happen one at a time and in priority order, since on slow storage a speculative read
    /// would otherwise delay the one the user is waiting for. Decoding is not limited like this.
    fn read_turn(&self, path: &Path) -> ReadTurn {
        if let Some(current) = &self.current {
            if current.path == path {
                return ReadTurn::Read;
            }
            // The worker reserved for the current image will get to it soon even if it has not
            // been picked up yet, so wait for that as well.
            let is_reading = matches!(self.phase(&current.path), None | Some(Phase::Reading));
            if current.original.is_none() && is_reading {
                return if self.is_wanted(path) {
                    ReadTurn::Wait
                } else {
                    ReadTurn::Cancelled
                };
            }
        }
        let Some(ahead) = self.prefetch.iter().position(|job| job.path == path) else {
            return ReadTurn::Cancelled;
        };
        let is_blocked = self.prefetch[..ahead]
            .iter()
            .any(|job| self.phase(&job.path) == Some(Phase::Reading));
        if is_blocked {
            ReadTurn::Wait
        } else {
            ReadTurn::Read
        }
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
                let phase = if job.original.is_some() {
                    Phase::Decoding
                } else {
                    Phase::Reading
                };
                let was_current = state.current.as_ref().is_some_and(|c| c.path == job.path);
                state.in_flight.push(InFlight {
                    path: job.path.clone(),
                    phase,
                    was_current,
                });
                return job;
            }
            state = self.wake.wait(state).expect("BUG: loader state poisoned");
        }
    }

    // On slow storage, cancelling every superseded read while moving quickly would leave nothing
    // to show. Finish reading the draft of any image that was current, even after the user moves
    // on.
    fn wait_for_disk(&self, path: &Path, is_for_draft: bool) -> JobStatus {
        let mut state = self.lock();
        loop {
            match state.read_turn(path) {
                ReadTurn::Read => return JobStatus::Continue,
                ReadTurn::Wait => {
                    state = self.wake.wait(state).expect("BUG: loader state poisoned")
                }
                ReadTurn::Cancelled => {
                    let was_current = state.in_flight(path).is_some_and(|job| job.was_current);
                    return if is_for_draft && was_current {
                        JobStatus::Continue
                    } else {
                        JobStatus::Cancelled
                    };
                }
            }
        }
    }

    fn start_decoding(&self, path: &Path) -> JobStatus {
        let mut state = self.lock();
        state.set_phase(path, Phase::Decoding);
        self.wake.notify_all();
        if state.is_wanted(path) {
            JobStatus::Continue
        } else {
            JobStatus::Cancelled
        }
    }

    fn finish(&self, job: &LoadJob) -> Option<LoadJob> {
        let mut state = self.lock();
        if let Some(wanted) = state
            .wanted(job)
            .filter(|wanted| wanted.params != job.params)
        {
            return Some(wanted.output());
        }
        let path = job.path.as_path();
        state.set_phase(path, Phase::AwaitingAck);
        state.current.take_if(|job| job.path == path);
        state.prefetch.retain(|job| job.path != path);
        self.wake.notify_all();
        None
    }

    fn forget(&self, path: &Path) {
        let mut state = self.lock();
        state.in_flight.retain(|job| job.path != path);
        self.wake.notify_all();
    }

    fn read_range(
        &self,
        path: &Path,
        file: &mut File,
        offset: u64,
        len: usize,
        data: &mut Vec<u8>,
        is_for_draft: bool,
    ) -> Result<JobStatus> {
        file.seek(SeekFrom::Start(offset))?;
        data.reserve_exact(len);
        let mut left = len;
        while left > 0 {
            if self.wait_for_disk(path, is_for_draft) == JobStatus::Cancelled {
                return Ok(JobStatus::Cancelled);
            }
            let limit = u64::try_from(left.min(CHUNK))?;
            let nr_read = file.by_ref().take(limit).read_to_end(data)?;
            ensure!(nr_read > 0, "file is shorter than expected");
            left -= nr_read;
        }
        Ok(JobStatus::Continue)
    }

    fn read_span(
        &self,
        path: &Path,
        file: &mut File,
        head: &[u8],
        span: tiff::ByteRange,
        is_for_draft: bool,
    ) -> Result<Option<Vec<u8>>> {
        let nr_have = head.len().saturating_sub(span.offset).min(span.len);
        let mut data = head
            .get(span.offset..span.offset + nr_have)
            .unwrap_or_default()
            .to_vec();
        let offset = u64::try_from(span.offset + nr_have)?;
        let nr_left = span.len - nr_have;
        let status = self.read_range(path, file, offset, nr_left, &mut data, is_for_draft)?;
        Ok((status == JobStatus::Continue).then_some(data))
    }

    fn send(&self, loaded: LoadResult) {
        // Only fails if the event loop has gone away, in which case we are exiting
        let _ = self.proxy.send_event(ViewerEvent::Loaded(loaded));
    }

    /// Decoding the full size JPEG is slow, but the smaller one is usually fast, so draft it
    fn send_draft(&self, job: &LoadJob, data: &[u8], orientation: u16) {
        let job = self.output_job(job);
        if job.params.is_zoom() {
            return;
        }
        let Ok(img) = decode::decode(data, Some(orientation)) else {
            return;
        };
        if let Ok(frame) = job.params.render_draft(&img) {
            let prepared = PreparedImage {
                frame: Arc::new(frame),
                is_draft: true,
            };
            self.send(LoadResult {
                job,
                result: Ok(prepared),
                original: None,
            });
        }
    }

    fn output_job(&self, job: &LoadJob) -> LoadJob {
        let state = self.lock();
        state.wanted(job).unwrap_or(job).output()
    }

    fn read(&self, job: &LoadJob) -> Result<Option<EncodedImage>> {
        let path = &job.path;
        if self.wait_for_disk(path, true) == JobStatus::Cancelled {
            return Ok(None);
        }
        let mut file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())?;
        let mut head = Vec::new();
        if self.read_range(path, &mut file, 0, len.min(HEAD_LEN), &mut head, true)?
            == JobStatus::Cancelled
        {
            return Ok(None);
        }
        if !tiff::has_tiff_magic(&head) {
            let offset = u64::try_from(head.len())?;
            let nr_left = len - head.len();
            let status = self.read_range(path, &mut file, offset, nr_left, &mut head, false)?;
            return Ok((status == JobStatus::Continue).then_some(EncodedImage {
                data: head,
                orientation: None,
            }));
        }

        let embedded = tiff::find_embedded(&head)?;
        if !self.output_job(job).params.is_zoom() {
            if let Some(draft) = embedded.draft {
                let draft = self.read_span(path, &mut file, &head, draft, true)?;
                let Some(data) = draft else {
                    return Ok(None);
                };
                self.send_draft(job, &data, embedded.orientation);
            }
        }
        let data = self.read_span(path, &mut file, &head, embedded.full, false)?;
        Ok(data.map(|data| EncodedImage {
            data,
            orientation: Some(embedded.orientation),
        }))
    }

    /// None means the job was abandoned because it is no longer wanted.
    fn load_original(&self, job: &mut LoadJob) -> Result<Option<Arc<DecodedImage>>> {
        if let Some(original) = job.original.take() {
            return Ok(Some(original));
        }
        let source = self.read(job);
        self.lister_gate
            .lock()
            .expect("BUG: lister gate poisoned")
            .take();
        let Some(source) = source? else {
            return Ok(None);
        };
        if self.start_decoding(&job.path) == JobStatus::Cancelled {
            return Ok(None);
        }
        Ok(Some(Arc::new(decode::decode(
            &source.data,
            source.orientation,
        )?)))
    }

    fn work(&self, is_reserved: bool) {
        loop {
            let mut job = self.next_job(is_reserved);
            let original = match self.load_original(&mut job) {
                Ok(Some(original)) => Ok(original),
                Err(err) => Err(format!("{err:#}")),
                Ok(None) => {
                    self.forget(&job.path);
                    continue;
                }
            };
            let mut job = self.output_job(&job);
            loop {
                let result = match &original {
                    Ok(img) => job
                        .params
                        .render(img)
                        .map(|frame| PreparedImage {
                            frame: Arc::new(frame),
                            is_draft: false,
                        })
                        .map_err(|err| format!("{err:#}")),
                    Err(err) => Err(err.clone()),
                };
                if let Some(wanted) = self.finish(&job) {
                    job = wanted;
                    continue;
                }
                self.send(LoadResult {
                    job,
                    result,
                    original: original.ok(),
                });
                break;
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

    pub fn set_jobs(
        &self,
        current: Option<LoadJob>,
        prefetch: Vec<LoadJob>,
        completed: Option<&Path>,
    ) {
        let mut state = self.shared.lock();
        let LoadQueue {
            current: current_slot,
            in_flight,
            ..
        } = &mut *state;
        if let Some(path) = completed {
            in_flight.retain(|job| job.path != path);
        }
        *current_slot = current;
        if let Some(current) = current_slot {
            for job in in_flight.iter_mut().filter(|job| job.path == current.path) {
                job.was_current = true;
            }
        }
        state.prefetch = prefetch;
        self.shared.wake.notify_all();
    }
}
