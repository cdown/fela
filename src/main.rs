#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowAttributes, WindowId};

mod bar;
mod cache;
mod config;
mod decode;
mod files;
mod key_handler;
mod loader;
mod sharpness;
mod tiff;
mod view;

use files::Navigation;
use view::{ScaleMode, ZoomStep};

fn render_frame(
    ready: &cache::CachedImage,
    view: &view::View,
    area: (u32, u32),
) -> Result<Option<Arc<decode::RenderedImage>>> {
    let ScaleMode::Zoom(zoom) = view.scale else {
        return Ok(Some(Arc::clone(&ready.fitted)));
    };
    let Some(original) = &ready.original else {
        return Ok(None);
    };
    if area.0 == 0 || area.1 == 0 {
        return Ok(None);
    }
    let img = original.upright_size(view.turns);
    let region = view.region(zoom, img, area);
    let frame = decode::render(original, view.turns, region.crop, region.w, region.h)?;
    Ok(Some(Arc::new(frame)))
}

const WINDOW_SIZE: PhysicalSize<u32> = PhysicalSize::new(800, 600);

// On Windows, drawing stalls while a new window takes focus. Create it without focus and wait
// for the first load result or FOCUS_DEADLINE, then draw before asking for focus
const FOCUS_AFTER_FIRST_IMAGE: bool = cfg!(windows);
const FOCUS_DEADLINE: Duration = Duration::from_millis(50);

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
            let deadline = Instant::now() + FOCUS_DEADLINE;
            self.focus_by = Some(deadline);
            el.set_control_flow(ControlFlow::WaitUntil(deadline));
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

    fn area(&self) -> (u32, u32) {
        let bar_h = u32::try_from(self.bar_height()).unwrap_or(u32::MAX);
        (self.size.width, self.size.height.saturating_sub(bar_h))
    }

    fn zoom(&self) -> Option<f64> {
        if let ScaleMode::Zoom(zoom) = self.view.scale {
            return Some(zoom);
        }
        let entry = self.cache.get(self.files.index())?;
        let ready = entry.result.as_ref().ok().filter(|ready| !ready.is_draft)?;
        Some(f64::from(ready.fitted.w) / f64::from(ready.upright_size.0))
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
        let Some(original) = self
            .cache
            .get(self.files.index())
            .and_then(|entry| entry.result.as_ref().ok())
            .and_then(|ready| ready.original.as_ref())
        else {
            return;
        };
        let area = self.area();
        if area.0 > 0 && area.1 > 0 {
            let img = original.upright_size(self.view.turns);
            self.view.pan_by(dir, nr_px, img, area);
            self.request_redraw();
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
            && self.cache.get(cur).is_none_or(|entry| {
                entry.result.as_ref().is_ok_and(|ready| {
                    ready.is_draft
                        || (matches!(self.view.scale, ScaleMode::Zoom(_))
                            && ready.original.is_none())
                })
            })
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
        self.files.remove(paths);
        self.cache.reindex(|path| self.files.position(0, path));
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
            if let Some(idx) = self.files.position(self.files.index(), path) {
                self.cache.remove(idx);
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
        self.schedule();
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

    fn job_for(&self, idx: usize) -> Option<loader::LoadJob> {
        let is_current = idx == self.files.index();
        let (w, h) = self.area();
        let fit_params = decode::FitParams {
            w,
            h,
            upscale: self.view.scale == ScaleMode::Fit,
            turns: if is_current { self.view.turns } else { 0 },
        };
        let needs_original = is_current && matches!(self.view.scale, ScaleMode::Zoom(_));

        let original = match self.cache.get(idx) {
            None => None,
            Some(entry) => match &entry.result {
                Err(_) => return None,
                Ok(ready) => {
                    let is_fitted = entry.fit_params == fit_params && !ready.is_draft;
                    if is_fitted && (ready.original.is_some() || !needs_original) {
                        return None;
                    }
                    ready.original.clone()
                }
            },
        };
        Some(loader::LoadJob {
            idx,
            path: self.files.get(idx)?.clone(),
            fit_params,
            original,
        })
    }

    fn schedule(&mut self) {
        let cur = self.files.index();
        let dir = if self.last_step < 0 { -1 } else { 1 };
        let nr_ahead = isize::try_from(self.cfg.prefetch_ahead).unwrap_or(isize::MAX);
        let nr_behind = isize::try_from(self.cfg.prefetch_behind).unwrap_or(isize::MAX);

        let mut steps = Vec::new();
        if self.cfg.prefetch_jump && self.last_step.abs() > 1 {
            steps.push(self.last_step);
        }
        steps.extend((1..=nr_ahead).map(|nr| nr * dir));
        steps.extend((1..=nr_behind).map(|nr| -nr * dir));

        let mut prefetch: Vec<loader::LoadJob> = Vec::new();
        for step in steps {
            let Some(idx) = cur.checked_add_signed(step) else {
                continue;
            };
            if prefetch.iter().all(|job| job.idx != idx) {
                prefetch.extend(self.job_for(idx));
            }
        }
        self.loader.set_jobs(self.job_for(cur), prefetch);
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
            self.schedule();
        }

        let cur = self.files.index();
        let mut message = None;
        match self.cache.get(cur).map(|entry| &entry.result) {
            Some(Ok(ready)) => {
                if let Some(frame) = render_frame(ready, &self.view, self.area())? {
                    self.shown_frame = Some(frame);
                    self.trail.clear();
                }
            }
            Some(Err(err)) => {
                self.shown_frame = None;
                self.trail.clear();
                message = Some(err.clone());
            }
            None if !matches!(self.view.scale, ScaleMode::Zoom(_)) => {
                let is_ready = |idx: &usize| self.cache.get(*idx).is_some_and(|e| e.result.is_ok());
                if let Some(nr) = self.trail.iter().rposition(is_ready) {
                    let idx = self.trail[nr];
                    if let Some(Ok(ready)) = self.cache.get(idx).map(|entry| &entry.result) {
                        self.shown_frame = Some(Arc::clone(&ready.fitted));
                    }
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
        let jump = isize::try_from(self.cfg.jump).unwrap_or(isize::MAX);
        let from = self.files.index();

        let navigation = match key.as_ref() {
            Key::Character("q") if self.files.has_marks() => {
                self.notice = Some("files are marked, press Q to quit".into());
                return self.request_redraw();
            }
            Key::Character("q" | "Q") => return el.exit(),
            Key::Character("n") | Key::Named(NamedKey::Space) => self.files.step(n),
            Key::Character("p") | Key::Named(NamedKey::Backspace) => self.files.step(-n),
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
                self.cache.remove(from);
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
        if loaded
            .result
            .as_ref()
            .map_or(true, |prepared| !prepared.is_draft)
        {
            self.loader.acknowledge(&loaded.job.path);
        }
        let Some(idx) = self.files.position(loaded.job.idx, &loaded.job.path) else {
            return;
        };
        let cur = self.files.index();
        self.cache.insert(idx, loaded);
        let needs_original = matches!(self.view.scale, ScaleMode::Zoom(_));
        self.cache.trim(cur, needs_original);
        self.schedule();
        if idx == cur || self.cache.get(cur).is_none() {
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
        self.schedule();

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
                self.cache.reindex(|path| self.files.position(0, path));
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
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                if !event.repeat || event.logical_key.as_ref() != Key::Character("s") {
                    self.on_key(el, &event.logical_key);
                }
            }
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            WindowEvent::Resized(_) | WindowEvent::RedrawRequested => self.request_redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let has_image = self.cache.get(self.files.index()).is_some();
        let is_holding = !has_image && self.focus_by.is_some_and(|by| Instant::now() < by);
        if !is_holding && std::mem::take(&mut self.needs_redraw) {
            let res = self.redraw();
            self.exit_on_err(el, res);
        }
        if let (false, Some(_), Some(window)) = (is_holding, self.focus_by, &self.window) {
            window.focus_window();
            self.focus_by = None;
            el.set_control_flow(ControlFlow::Wait);
        }
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
    fn test_zoom_waits_for_original() {
        let fitted = Arc::new(decode::RenderedImage {
            w: 1,
            h: 1,
            px: vec![0],
        });
        let mut ready = cache::CachedImage {
            fitted: Arc::clone(&fitted),
            original: None,
            upright_size: (4, 4),
            is_draft: true,
        };
        let mut view = view::View::new();
        let area = (2, 2);
        assert!(Arc::ptr_eq(
            &render_frame(&ready, &view, area).unwrap().unwrap(),
            &fitted,
        ));

        view.scale = ScaleMode::Zoom(1.0);
        assert!(render_frame(&ready, &view, area).unwrap().is_none());
        ready.is_draft = false;
        assert!(render_frame(&ready, &view, area).unwrap().is_none());

        ready.original = Some(Arc::new(decode::DecodedImage {
            w: 4,
            h: 4,
            px: vec![0; 4 * 4 * 3],
            turns: 0,
        }));
        let frame = render_frame(&ready, &view, area).unwrap().unwrap();
        assert_eq!((frame.w, frame.h), area);
    }
}
