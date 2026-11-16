#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use softbuffer::Surface;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowAttributes, WindowId};

mod bar;
mod cache;
mod config;
mod decode;
mod files;
mod key_handler;
mod loader;
mod prefetch;
mod sharpness;
mod tiff;
mod view;

use decode::ImageSize;
use files::Navigation;
use view::{FrameParams, ScaleMode, ZoomStep};

const WINDOW_SIZE: PhysicalSize<u32> = PhysicalSize::new(800, 600);

// On Windows, drawing stalls while a new window takes focus. Create it without focus and wait
// for the first load result or FOCUS_DEADLINE, then draw before asking for focus
const FOCUS_AFTER_FIRST_IMAGE: bool = cfg!(windows);
const FOCUS_DEADLINE: Duration = Duration::from_millis(50);

const NAVIGATION_REPEAT_DELAY: Duration = Duration::from_millis(500);

// If this needs tuning, make it configurable later.
const IDLE_ZOOM_DELAY: Duration = Duration::from_millis(200);

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(
        required_unless_present = "stdin",
        help = "Images or directories to show"
    )]
    paths: Vec<PathBuf>,

    #[arg(short = 'i', long, help = "Read paths from stdin, one per line")]
    stdin: bool,

    #[arg(
        short = 'o',
        long,
        help = "Print the marked files on exit, one per line"
    )]
    stdout: bool,

    #[arg(short, long, help = "Start in fullscreen")]
    fullscreen: bool,
}

#[cfg(windows)]
fn fullscreen_attrs(attrs: WindowAttributes, monitor: Option<MonitorHandle>) -> WindowAttributes {
    let Some(monitor) = monitor else {
        return attrs;
    };
    attrs
        .with_decorations(false)
        .with_position(monitor.position())
        .with_inner_size(monitor.size())
}

#[cfg(not(windows))]
fn fullscreen_attrs(attrs: WindowAttributes, _: Option<MonitorHandle>) -> WindowAttributes {
    attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)))
}

#[cfg(windows)]
fn set_fullscreen(window: &Window, is_fullscreen: bool) {
    window.set_decorations(!is_fullscreen);
    match window.current_monitor().filter(|_| is_fullscreen) {
        Some(monitor) => {
            window.set_outer_position(monitor.position());
            let _ = window.request_inner_size(monitor.size());
        }
        None => {
            let _ = window.request_inner_size(WINDOW_SIZE);
        }
    }
}

#[cfg(not(windows))]
fn set_fullscreen(window: &Window, is_fullscreen: bool) {
    window.set_fullscreen(is_fullscreen.then_some(winit::window::Fullscreen::Borderless(None)));
}

enum ViewerEvent {
    Listed(Vec<PathBuf>),
    Loaded(loader::LoadResult),
    HandlerDone(key_handler::HandlerOutcome),
}

enum HandlerState {
    Idle,
    /// The prefix key was pressed, and the next key goes to the handler.
    Armed,
    Running,
    Failed(anyhow::Error),
}

enum IdleZoom {
    WaitingForFrame,
    WaitingUntil(Instant),
    Ready,
}

struct HeldNavigation {
    key: PhysicalKey,
    step: isize,
    next: Instant,
}

impl HeldNavigation {
    fn new(key: PhysicalKey, step: isize, now: Instant) -> Self {
        Self {
            key,
            step,
            next: now + NAVIGATION_REPEAT_DELAY,
        }
    }

    fn take_step(&mut self, now: Instant, interval: Duration) -> Option<isize> {
        if now < self.next {
            return None;
        }
        self.next = now + interval;
        Some(self.step)
    }
}

fn navigation_step(key: &Key) -> Option<isize> {
    match key.as_ref() {
        Key::Character("n") | Key::Named(NamedKey::Space) => Some(1),
        Key::Character("p") | Key::Named(NamedKey::Backspace) => Some(-1),
        _ => None,
    }
}

struct Viewer {
    args: Args,
    cfg: config::Config,
    files: files::FileList,
    cache: cache::ImageCache,
    loader: loader::Loader,
    size: PhysicalSize<u32>,
    is_bar_shown: bool,
    /// How far the last navigation moved, as a hint for where the next one will go.
    last_step: isize,
    shown_frame: Option<Arc<decode::RenderedImage>>,
    /// The images we were on since the current image was last on screen, oldest first.
    trail: Vec<usize>,
    view: view::View,
    key_prefix: Option<usize>,
    mods: ModifiersState,
    handler_state: HandlerState,
    notice: Option<String>,
    clipboard: Option<arboard::Clipboard>,
    proxy: EventLoopProxy<ViewerEvent>,
    needs_redraw: bool,
    /// Set while we still have to draw the first frame and take focus, which waits for the first
    /// image until this time. See `FOCUS_AFTER_FIRST_IMAGE`.
    focus_by: Option<Instant>,
    idle_zoom: IdleZoom,
    held_navigation: Option<HeldNavigation>,
    is_fullscreen: bool,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    err: Option<anyhow::Error>,
}

impl Viewer {
    fn init_window(&mut self, el: &ActiveEventLoop, monitor: Option<MonitorHandle>) -> Result<()> {
        let mut attrs = Window::default_attributes()
            .with_title("fela")
            .with_inner_size(WINDOW_SIZE)
            .with_active(!FOCUS_AFTER_FIRST_IMAGE);
        if self.is_fullscreen {
            attrs = fullscreen_attrs(attrs, monitor);
        }
        let window = Rc::new(el.create_window(attrs)?);
        let ctx = softbuffer::Context::new(window.clone()).map_err(|err| anyhow!("{err}"))?;
        self.surface = Some(Surface::new(&ctx, window.clone()).map_err(|err| anyhow!("{err}"))?);
        self.window = Some(window);
        if FOCUS_AFTER_FIRST_IMAGE {
            self.focus_by = Some(Instant::now() + FOCUS_DEADLINE);
        }
        Ok(())
    }

    /// We don't leave the timing to the windowing system, as Windows can take ~200ms to ask a new
    /// window to paint itself.
    fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    fn bar_height(&self) -> usize {
        if self.is_bar_shown || matches!(self.handler_state, HandlerState::Failed(_)) {
            bar::HEIGHT
        } else {
            0
        }
    }

    fn area(&self) -> ImageSize {
        let bar_h = u32::try_from(self.bar_height()).unwrap_or(u32::MAX);
        ImageSize {
            w: self.size.width,
            h: self.size.height.saturating_sub(bar_h),
        }
    }

    fn frame_params(&self, idx: usize) -> FrameParams {
        self.view
            .frame_params(self.area(), idx == self.files.index())
    }

    fn current_frame(&self) -> Option<&cache::CachedFrame> {
        self.cache
            .get(self.files.current()?)?
            .frame(self.frame_params(self.files.index()))
    }

    fn zoom(&self) -> Option<f64> {
        if let ScaleMode::Zoom(zoom) = self.view.scale {
            return Some(zoom);
        }
        let ready = self
            .current_frame()?
            .result
            .as_ref()
            .ok()
            .filter(|ready| !ready.is_draft)?;
        let size = self.cache.source_size(self.files.current()?)?;
        let size = self.frame_params(self.files.index()).upright_size(size);
        Some(f64::from(ready.frame.w) / f64::from(size.w))
    }

    fn step_zoom(&mut self, step: ZoomStep) {
        if let Some(zoom) = self.zoom() {
            self.view.step_zoom(zoom, step);
            self.refresh();
        }
    }

    fn set_scale(&mut self, scale: ScaleMode) {
        self.view.scale = scale;
        self.refresh();
    }

    fn pan(&mut self, dir: (f64, f64), nr_px: Option<usize>) {
        let Some(img) = self
            .files
            .current()
            .and_then(|path| self.cache.source_size(path))
        else {
            return;
        };
        let area = self.area();
        if matches!(self.view.scale, ScaleMode::Zoom(_)) && area.w > 0 && area.h > 0 {
            self.view
                .pan_by(dir, nr_px, img.rotated(self.view.turns), area);
            self.refresh();
        }
    }

    fn rotate(&mut self, turns: u8) {
        self.view.turns = (self.view.turns + turns) % 4;
        self.view.pan = (0.0, 0.0);
        self.refresh();
    }

    fn status(&self, detail: Option<f64>) -> (String, String) {
        let cur = self.files.index();
        let left = match (&self.notice, &self.handler_state) {
            (Some(notice), _) => notice.clone(),
            (None, HandlerState::Armed) => "key handler: press a key, or Escape to abort".into(),
            (None, HandlerState::Running) => "key handler: running".into(),
            (None, HandlerState::Failed(err)) => format!("key handler: {err:#}"),
            (None, HandlerState::Idle) => self
                .files
                .current()
                .map_or(String::new(), |path| path.display().to_string()),
        };
        let mark = if self.files.is_marked(cur) { "* " } else { "" };
        let state = if self.files.is_listing {
            "listing "
        } else if self.files.current().is_some()
            && self
                .current_frame()
                .is_none_or(|frame| frame.result.as_ref().is_ok_and(|ready| ready.is_draft))
        {
            "loading "
        } else {
            ""
        };
        let zoom = self
            .zoom()
            .map_or(String::new(), |zoom| format!("{:.0}% ", zoom * 100.0));
        let sharpness = match detail {
            Some(detail) if state.is_empty() => format!("detail {detail:.2} "),
            _ if self.view.is_sharpness_shown => "sharpness ".into(),
            _ => String::new(),
        };
        let nr = if self.files.len() == 0 { 0 } else { cur + 1 };
        (
            left,
            format!("{state}{mark}{sharpness}{zoom}{nr}/{}", self.files.len()),
        )
    }

    fn remove_files(&mut self, paths: &[PathBuf]) -> bool {
        if paths.is_empty() {
            return false;
        }
        for path in paths {
            self.cache.invalidate(path);
        }
        self.files.remove(paths);
        self.trail.clear();
        self.view.pan = (0.0, 0.0);
        self.view.turns = 0;
        let is_empty = self.files.len() == 0;
        if is_empty {
            self.shown_frame = None;
        }
        is_empty
    }

    fn on_handler_key(&mut self, key: &Key) {
        if *key == Key::Named(NamedKey::Escape) {
            self.handler_state = HandlerState::Idle;
            self.request_redraw();
            return;
        }
        let Some(name) = key_handler::key_name(key, self.mods) else {
            return;
        };
        let mut paths = self.files.marked();
        if paths.is_empty() {
            paths.extend(self.files.current().cloned());
        }
        self.handler_state = HandlerState::Running;
        key_handler::spawn(self.cfg.handler.clone(), name, paths, self.proxy.clone());
        self.request_redraw();
    }

    fn on_handler_done(&mut self, el: &ActiveEventLoop, outcome: key_handler::HandlerOutcome) {
        self.handler_state = HandlerState::Idle;
        for path in &outcome.changed {
            if self.files.position(self.files.index(), path).is_some() {
                self.cache.invalidate(path);
            }
        }
        let is_empty = self.remove_files(&outcome.gone);
        if let Some(err) = outcome.err {
            self.handler_state = HandlerState::Failed(err);
        } else if is_empty {
            el.exit();
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        self.idle_zoom = IdleZoom::WaitingForFrame;
        if let Some(path) = self.files.current() {
            self.cache.touch(path);
        }
        self.schedule(None);
        if let Some(window) = &self.window {
            if let Some(path) = self.files.current() {
                window.set_title(&format!("fela - {}", path.display()));
            } else {
                window.set_title("fela");
            }
        }
        self.request_redraw();
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(window) = &self.window {
            self.is_fullscreen = !self.is_fullscreen;
            set_fullscreen(window, self.is_fullscreen);
        }
    }

    fn job_for(&self, idx: usize, params: FrameParams) -> Option<loader::LoadJob> {
        let path = self.files.get(idx)?;
        let entry = self.cache.get(path);
        if params.area.w == 0
            || params.area.h == 0
            || entry
                .and_then(|entry| entry.frame(params))
                .is_some_and(|frame| frame.result.as_ref().map_or(true, |ready| !ready.is_draft))
        {
            return None;
        }
        Some(loader::LoadJob {
            idx,
            path: path.clone(),
            params,
            revision: self.cache.revision(path),
            original: entry.and_then(|entry| entry.original.clone()),
        })
    }

    fn idle_zoom_params(&self, path: &Path, budget: usize) -> Option<FrameParams> {
        if self.cfg.prefetch_inflight == 0
            || matches!(self.view.scale, ScaleMode::Zoom(_))
            || budget == 0
        {
            return None;
        }
        let params = self
            .view
            .frame_params_at_scale(self.area(), true, ScaleMode::Zoom(1.0));
        (self.cache.frame_cost(path, params) <= budget).then_some(params)
    }

    fn prefetch(&self, current: &Path, budget: usize) -> prefetch::Prefetch {
        let cur = self.files.index();
        let params = self.view.frame_params(self.area(), false);
        let jump = cur
            .checked_add_signed(self.last_step)
            .filter(|_| self.cfg.prefetch_jump && self.last_step.unsigned_abs() > 1);
        let candidates = |ahead| {
            prefetch::indices(
                cur,
                self.files.len(),
                self.last_step,
                self.cfg.prefetch_jump,
                ahead,
            )
            .filter_map(|idx| {
                let path = self.files.get(idx)?;
                Some(prefetch::Candidate {
                    idx,
                    path,
                    bytes: self.cache.frame_cost(path, params),
                    is_estimate: self.cache.source_size(path).is_none(),
                    is_jump: Some(idx) == jump,
                })
            })
        };
        prefetch::select(
            current,
            candidates(true),
            candidates(false),
            budget,
            self.cfg.prefetch_bias,
        )
    }

    fn schedule(&mut self, completed: Option<&Path>) {
        let area = self.area();
        if area.w == 0 || area.h == 0 {
            self.loader.set_jobs(None, Vec::new(), completed);
            return;
        }
        let cur = self.files.index();
        let params = self.frame_params(cur);
        let current = self.files.current().map(PathBuf::as_path);
        let mut budget = current.map_or(0, |path| self.cache.prefetch_bytes(path, params));
        let idle_params = current
            .filter(|_| matches!(self.idle_zoom, IdleZoom::Ready))
            .and_then(|path| self.idle_zoom_params(path, budget));
        if let (Some(path), Some(idle)) = (current, idle_params) {
            budget -= self.cache.frame_cost(path, idle);
        }
        let idle_job = idle_params.and_then(|params| self.job_for(cur, params));
        let prefetch = current.map_or_else(prefetch::Prefetch::default, |path| {
            self.prefetch(path, budget)
        });
        let view = &self.view;
        let selected: HashMap<_, _> = prefetch
            .indices
            .iter()
            .filter_map(|&idx| {
                Some((
                    self.files.get(idx)?.as_path(),
                    view.frame_params(area, false),
                ))
            })
            .chain(current.zip(idle_params))
            .collect();
        self.cache
            .trim(current.map(|path| (path, params)), &selected);
        let prefetch = idle_job
            .into_iter()
            .chain(
                prefetch
                    .indices
                    .into_iter()
                    .chain(prefetch.probes)
                    .filter_map(|idx| self.job_for(idx, self.frame_params(idx))),
            )
            .collect();
        self.loader
            .set_jobs(self.job_for(cur, params), prefetch, completed);
    }

    fn redraw(&mut self) -> Result<()> {
        let Some(window) = self.window.clone() else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        if size != self.size {
            self.size = size;
            self.idle_zoom = IdleZoom::WaitingForFrame;
            self.schedule(None);
        }

        let mut message = None;
        match self.current_frame().map(|frame| &frame.result) {
            Some(Ok(ready)) => {
                self.shown_frame = Some(Arc::clone(&ready.frame));
                self.trail.clear();
            }
            Some(Err(err)) => {
                message = Some(err.clone());
                self.shown_frame = None;
                self.trail.clear();
            }
            None if !matches!(self.view.scale, ScaleMode::Zoom(_)) => {
                if let Some((nr, frame)) =
                    self.trail.iter().enumerate().rev().find_map(|(nr, &idx)| {
                        let frame = self
                            .cache
                            .get(self.files.get(idx)?)?
                            .frame(self.frame_params(idx))?;
                        Some((nr, Arc::clone(&frame.result.as_ref().ok()?.frame)))
                    })
                {
                    self.shown_frame = Some(frame);
                    self.trail.drain(..=nr);
                }
            }
            None => {}
        }
        let sharpness = self
            .shown_frame
            .as_ref()
            .filter(|_| self.view.is_sharpness_shown)
            .map(|frame| sharpness::render(frame));
        let (left, right) = self.status(sharpness.as_ref().and_then(|(_, detail)| *detail));

        let (buf_w, buf_h) = (size.width as usize, size.height as usize);
        let bar_h = self.bar_height().min(buf_h);
        let Some(surface) = &mut self.surface else {
            return Ok(());
        };
        surface.resize(w, h).map_err(|err| anyhow!("{err}"))?;
        let mut buf = surface.buffer_mut().map_err(|err| anyhow!("{err}"))?;
        let (area, bar) = buf.split_at_mut((buf_h - bar_h) * buf_w);
        let frame = sharpness
            .as_ref()
            .map(|(frame, _)| frame)
            .or(self.shown_frame.as_deref());
        match frame {
            Some(frame) => view::draw(area, buf_w, buf_h - bar_h, frame),
            None => area.fill(0),
        }
        if let Some(message) = &message {
            bar::draw_message(area, buf_w, message);
        }
        if bar_h > 0 {
            bar::draw(bar, buf_w, &left, &right);
        }
        buf.present().map_err(|err| anyhow!("{err}"))?;

        if matches!(self.idle_zoom, IdleZoom::WaitingForFrame)
            && self
                .current_frame()
                .is_some_and(|frame| frame.result.as_ref().is_ok_and(|ready| !ready.is_draft))
            && self.files.current().is_some_and(|path| {
                let budget = self
                    .cache
                    .prefetch_bytes(path, self.frame_params(self.files.index()));
                self.idle_zoom_params(path, budget).is_some()
            })
        {
            self.idle_zoom = IdleZoom::WaitingUntil(Instant::now() + IDLE_ZOOM_DELAY);
        }
        Ok(())
    }

    fn copy_filenames(&mut self, marked: bool) -> Result<()> {
        let paths = if marked {
            self.files.marked()
        } else {
            self.files.current().cloned().into_iter().collect()
        };
        let names: Vec<_> = paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy())
            .collect();
        anyhow::ensure!(!names.is_empty(), "no filenames to copy");
        let clipboard = match &mut self.clipboard {
            Some(clipboard) => clipboard,
            slot => slot.insert(arboard::Clipboard::new()?),
        };
        clipboard.set_text(names.join("\n"))?;
        self.notice = Some(format!("copied {} filename(s)", names.len()));
        Ok(())
    }

    fn navigation_interval(&self) -> Duration {
        Duration::from_millis(u64::from(self.cfg.navigation_repeat_ms.get()))
    }

    fn on_keyboard(&mut self, el: &ActiveEventLoop, event: KeyEvent, is_synthetic: bool) {
        let is_held = self
            .held_navigation
            .as_ref()
            .is_some_and(|held| held.key == event.physical_key);
        if !event.state.is_pressed() {
            if is_held {
                self.held_navigation = None;
            }
            return;
        }
        let step = navigation_step(&event.logical_key);
        if event.repeat {
            if !is_held && step.is_none() && event.logical_key.as_ref() != Key::Character("s") {
                self.on_key(el, &event.logical_key);
            }
            return;
        }
        if is_synthetic && step.is_some() {
            return;
        }
        self.held_navigation = step
            .filter(|_| {
                matches!(
                    self.handler_state,
                    HandlerState::Idle | HandlerState::Failed(_)
                ) && !self.mods.control_key()
            })
            .map(|step| HeldNavigation::new(event.physical_key, step, Instant::now()));
        self.on_key(el, &event.logical_key);
    }

    fn step(&mut self, step: isize) -> Navigation {
        let from = self.files.index();
        let navigation = self.files.step(step);
        self.after_navigation(from, navigation);
        navigation
    }

    fn on_key(&mut self, el: &ActiveEventLoop, key: &Key) {
        self.notice = None;
        match self.handler_state {
            HandlerState::Idle => {}
            HandlerState::Armed => return self.on_handler_key(key),
            HandlerState::Running => return,
            HandlerState::Failed(_) => {
                self.handler_state = HandlerState::Idle;
                self.refresh();
            }
        }
        if self.mods.control_key() {
            match key.as_ref() {
                Key::Character("x") => self.handler_state = HandlerState::Armed,
                Key::Character("u") => self.files.clear_marks(),
                _ => {}
            }
            return self.request_redraw();
        }

        let key_prefix = self.key_prefix.take();
        let n = isize::try_from(key_prefix.unwrap_or(1)).unwrap_or(isize::MAX);
        if let Some(step) = navigation_step(key) {
            self.step(step * n);
            return;
        }
        let jump = isize::try_from(self.cfg.jump).unwrap_or(isize::MAX);
        let from = self.files.index();

        let navigation = match key.as_ref() {
            Key::Character("q") if self.files.has_marks() => {
                self.notice = Some("files are marked, press Q to quit".into());
                return self.request_redraw();
            }
            Key::Character("q" | "Q") => return el.exit(),
            Key::Character("]") => self.files.step(n.saturating_mul(jump)),
            Key::Character("[") => self.files.step(n.saturating_mul(jump).saturating_neg()),
            Key::Character("g") => self.files.go_to(0),
            Key::Character("N") => self.files.step_marked(n),
            Key::Character("P") => self.files.step_marked(n.saturating_neg()),
            Key::Character("c" | "C") => {
                if let Err(err) = self.copy_filenames(key.as_ref() == Key::Character("C")) {
                    self.notice = Some(format!("clipboard: {err}"));
                }
                return self.request_redraw();
            }
            Key::Character("m") => {
                self.files.mark();
                return self.request_redraw();
            }
            Key::Character("M") => {
                self.files.unmark();
                return self.request_redraw();
            }
            Key::Character("D") => {
                let Some(path) = self.files.current().cloned() else {
                    return;
                };
                if self.remove_files(&[path]) {
                    el.exit();
                }
                return self.refresh();
            }
            Key::Character("r") => {
                if let Some(path) = self.files.current() {
                    self.cache.invalidate(path);
                }
                return self.refresh();
            }
            Key::Character("s") => {
                self.view.is_sharpness_shown = !self.view.is_sharpness_shown;
                return self.request_redraw();
            }
            Key::Character("+") => return self.step_zoom(ZoomStep::In),
            Key::Character("-") => return self.step_zoom(ZoomStep::Out),
            Key::Character("=") => {
                return self.set_scale(ScaleMode::Zoom(
                    key_prefix.map_or(1.0, |c| c as f64 / 100.0),
                ));
            }
            Key::Character("w") => return self.set_scale(ScaleMode::ShrinkToFit),
            Key::Character("W") => return self.set_scale(ScaleMode::Fit),
            Key::Character("h") | Key::Named(NamedKey::ArrowLeft) => {
                return self.pan((-1.0, 0.0), key_prefix);
            }
            Key::Character("l") | Key::Named(NamedKey::ArrowRight) => {
                return self.pan((1.0, 0.0), key_prefix);
            }
            Key::Character("k") | Key::Named(NamedKey::ArrowUp) => {
                return self.pan((0.0, -1.0), key_prefix);
            }
            Key::Character("j") | Key::Named(NamedKey::ArrowDown) => {
                return self.pan((0.0, 1.0), key_prefix);
            }
            Key::Character("<") => return self.rotate(3),
            Key::Character(">") => return self.rotate(1),
            Key::Character("b") => {
                self.is_bar_shown = !self.is_bar_shown;
                return self.refresh();
            }
            Key::Character("f") => return self.toggle_fullscreen(),
            Key::Character("G") => self
                .files
                .go_to(key_prefix.map_or(usize::MAX, |c| c.saturating_sub(1))),
            Key::Character(c) => {
                if let Some(digit) = c.parse::<usize>().ok().filter(|_| c.len() == 1) {
                    self.key_prefix = Some(
                        key_prefix
                            .unwrap_or(0)
                            .saturating_mul(10)
                            .saturating_add(digit),
                    );
                }
                return;
            }
            _ => return,
        };
        self.after_navigation(from, navigation);
    }

    fn after_navigation(&mut self, from: usize, navigation: Navigation) {
        if navigation == Navigation::Moved {
            let to = self.files.index();
            self.last_step = if to > from {
                isize::try_from(to - from).unwrap_or(isize::MAX)
            } else {
                isize::try_from(from - to).map_or(isize::MIN, |step| -step)
            };
            if matches!(self.view.scale, ScaleMode::Zoom(_)) {
                self.trail.clear();
            } else {
                self.trail.push(from);
                self.view.pan = (0.0, 0.0);
            }
            self.view.turns = 0;
            self.refresh();
        }
    }

    fn on_loaded(&mut self, loaded: loader::LoadResult) {
        let completed = loaded
            .result
            .as_ref()
            .map_or(true, |ready| !ready.is_draft)
            .then(|| loaded.job.path.clone());
        let idx = self.files.position(loaded.job.idx, &loaded.job.path);
        let mut changed = false;
        let mut is_current = false;
        if let Some(idx) = idx {
            let destination = self.frame_params(idx);
            is_current = idx == self.files.index() && loaded.job.params == destination;
            changed = self.cache.insert(loaded, destination);
        }
        self.schedule(completed.as_deref());
        if changed && (is_current || self.current_frame().is_none()) {
            self.request_redraw();
        }
    }

    fn exit_on_err(&mut self, el: &ActiveEventLoop, res: Result<()>) {
        if let Err(err) = res {
            self.err = Some(err);
            el.exit();
        }
    }
}

impl ApplicationHandler<ViewerEvent> for Viewer {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Creating a window takes a while, so get the first image going
        let monitor = el
            .primary_monitor()
            .or_else(|| el.available_monitors().next());
        self.size = match &monitor {
            Some(monitor) if self.is_fullscreen => monitor.size(),
            _ => WINDOW_SIZE,
        };
        self.schedule(None);

        let res = self.init_window(el, monitor);
        self.exit_on_err(el, res);
        self.refresh();
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Listed(list) if list.is_empty() => {
                self.exit_on_err(el, Err(anyhow!("no images to show")));
            }
            ViewerEvent::Listed(list) => {
                self.files.set_list(list);
                self.trail.clear();
                self.refresh();
            }
            ViewerEvent::Loaded(loaded) => self.on_loaded(loaded),
            ViewerEvent::HandlerDone(outcome) => self.on_handler_done(el, outcome),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                self.on_keyboard(el, event, is_synthetic);
            }
            WindowEvent::Focused(false) => self.held_navigation = None,
            WindowEvent::ModifiersChanged(mods) => {
                if self.mods != mods.state() {
                    self.held_navigation = None;
                }
                self.mods = mods.state();
            }
            WindowEvent::Resized(_) | WindowEvent::RedrawRequested => self.request_redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let interval = self.navigation_interval();
        if let Some(step) = self
            .held_navigation
            .as_mut()
            .and_then(|held| held.take_step(Instant::now(), interval))
        {
            if self.step(step) == Navigation::Unchanged {
                self.held_navigation = None;
            }
        }
        let has_image = self.current_frame().is_some();
        let is_holding = !has_image && self.focus_by.is_some_and(|by| Instant::now() < by);
        if !is_holding && std::mem::take(&mut self.needs_redraw) {
            let res = self.redraw();
            self.exit_on_err(el, res);
        }
        if let (false, Some(_), Some(window)) = (is_holding, self.focus_by, &self.window) {
            window.focus_window();
            self.focus_by = None;
        }
        let mut next_wake = self
            .focus_by
            .into_iter()
            .chain(self.held_navigation.as_ref().map(|held| held.next))
            .min();
        if let IdleZoom::WaitingUntil(deadline) = self.idle_zoom {
            if Instant::now() >= deadline {
                self.idle_zoom = IdleZoom::Ready;
                self.schedule(None);
            } else {
                next_wake = next_wake.into_iter().chain(Some(deadline)).min();
            }
        }
        el.set_control_flow(next_wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

/// On slow storage listing a large directory would compete with reading the first image, so it
/// waits until `gate` says that is done.
fn spawn_lister(args: Vec<PathBuf>, gate: Receiver<()>, proxy: EventLoopProxy<ViewerEvent>) {
    std::thread::spawn(move || {
        let _ = gate.recv();
        // Only fails if the event loop has gone away, in which case nobody wants the list anyway
        let _ = proxy.send_event(ViewerEvent::Listed(files::expand_args(&args)));
    });
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: Simple FFI. It fails if we were not started from a console, which is fine.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() -> Result<()> {
    attach_console();
    let mut args = Args::parse();
    let cfg = config::load()?;
    let mut paths = std::mem::take(&mut args.paths);
    if args.stdin {
        for line in std::io::stdin().lines() {
            paths.push(line.context("failed to read paths from stdin")?.into());
        }
    }

    let el = EventLoop::<ViewerEvent>::with_user_event().build()?;
    let files = files::FileList::new(&paths);
    let (gate_tx, gate_rx) = mpsc::channel();
    let gate_tx = files.current().is_some().then_some(gate_tx);
    spawn_lister(paths, gate_rx, el.create_proxy());

    let mut app = Viewer {
        cache: cache::ImageCache::new(cfg.cache_mb),
        loader: loader::Loader::new(cfg.prefetch_inflight, el.create_proxy(), gate_tx),
        is_fullscreen: args.fullscreen || cfg.fullscreen,
        args,
        cfg,
        files,
        size: PhysicalSize::default(),
        is_bar_shown: true,
        last_step: 1,
        shown_frame: None,
        trail: Vec::new(),
        view: view::View::new(),
        key_prefix: None,
        mods: ModifiersState::empty(),
        handler_state: HandlerState::Idle,
        notice: None,
        clipboard: None,
        proxy: el.create_proxy(),
        needs_redraw: false,
        focus_by: None,
        idle_zoom: IdleZoom::WaitingForFrame,
        held_navigation: None,
        window: None,
        surface: None,
        err: None,
    };
    el.run_app(&mut app)?;
    if app.args.stdout {
        let mut out = std::io::stdout().lock();
        for path in app.files.marked() {
            key_handler::write_path(&mut out, &path)?;
        }
    }
    app.err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_held_navigation_repeat_timing() {
        let now = Instant::now();
        let interval = Duration::from_millis(50);
        let key = PhysicalKey::Code(winit::keyboard::KeyCode::KeyP);
        let mut held = HeldNavigation::new(key, -1, now);
        let first_repeat = now + Duration::from_millis(500);
        assert_eq!(
            held.take_step(first_repeat - Duration::from_millis(1), interval),
            None
        );
        assert_eq!(held.take_step(first_repeat, interval), Some(-1));
        assert_eq!(held.take_step(first_repeat + interval, interval), Some(-1));
        let after_stall = first_repeat + Duration::from_millis(220);
        assert_eq!(held.take_step(after_stall, interval), Some(-1));
        assert_eq!(held.take_step(after_stall, interval), None);
        assert_eq!(held.take_step(after_stall + interval, interval), Some(-1));
    }

    #[test]
    fn test_zoomed_prefetch_matches_navigation() {
        let mut view = view::View::new();
        view.scale = ScaleMode::Zoom(1.0);
        view.pan = (2.0, 3.0);
        view.turns = 1;
        let area = ImageSize { w: 8, h: 6 };
        let prefetched = view.frame_params(area, false);
        assert_ne!(prefetched, view.frame_params(area, true));
        view.turns = 0;
        assert_eq!(prefetched, view.frame_params(area, true));
        view.scale = ScaleMode::ShrinkToFit;
        assert_ne!(prefetched, view.frame_params(area, true));
    }
}
