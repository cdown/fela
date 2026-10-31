use anyhow::{anyhow, Context, Result};
use clap::Parser;
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

mod cache;
mod config;
mod decode;
mod files;
mod loader;
mod view;

const WINDOW_SIZE: PhysicalSize<u32> = PhysicalSize::new(800, 600);

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

    #[arg(short, long, help = "Start in fullscreen")]
    fullscreen: bool,
}

enum ViewerEvent {
    Listed(Vec<PathBuf>),
    Loaded(loader::LoadResult),
}

struct Viewer {
    args: Args,
    cfg: config::Config,
    files: files::FileList,
    cache: cache::ImageCache,
    loader: loader::Loader,
    size: PhysicalSize<u32>,
    /// How far the last navigation moved, as a hint for where the next one will go.
    last_step: isize,
    shown: Option<Arc<decode::RenderedImage>>,
    count: Option<usize>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    err: Option<anyhow::Error>,
}

impl Viewer {
    fn init_window(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("fela")
            .with_inner_size(WINDOW_SIZE)
            .with_fullscreen(self.args.fullscreen.then_some(Fullscreen::Borderless(None)));
        let window = Rc::new(el.create_window(attrs)?);
        let ctx = softbuffer::Context::new(window.clone()).map_err(|err| anyhow!("{err}"))?;
        self.surface = Some(Surface::new(&ctx, window.clone()).map_err(|err| anyhow!("{err}"))?);
        self.window = Some(window);
        Ok(())
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = &self.window {
            let is_fullscreen = window.fullscreen().is_some();
            window.set_fullscreen((!is_fullscreen).then_some(Fullscreen::Borderless(None)));
        }
    }

    fn job_for(&self, idx: usize, spec: loader::Spec) -> Option<loader::LoadJob> {
        let full = match self.cache.get(idx) {
            Some(entry) if entry.spec == spec || entry.result.is_err() => return None,
            Some(entry) => entry
                .result
                .as_ref()
                .ok()
                .and_then(|ready| ready.full.clone()),
            None => None,
        };
        Some(loader::LoadJob {
            idx,
            path: self.files.get(idx)?.clone(),
            spec,
            full,
        })
    }

    fn schedule(&mut self) {
        let spec = loader::Spec {
            w: self.size.width,
            h: self.size.height,
            upscale: false,
        };
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
                prefetch.extend(self.job_for(idx, spec));
            }
        }
        self.loader.set_jobs(self.job_for(cur, spec), prefetch);
    }

    fn redraw(&mut self) -> Result<()> {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };

        match self
            .cache
            .get(self.files.index())
            .map(|entry| &entry.result)
        {
            Some(Ok(ready)) => self.shown = Some(Arc::clone(&ready.fit)),
            Some(Err(_)) => self.shown = None,
            None => {}
        }

        surface.resize(w, h).map_err(|err| anyhow!("{err}"))?;
        let mut buf = surface.buffer_mut().map_err(|err| anyhow!("{err}"))?;
        match &self.shown {
            Some(frame) => view::draw(&mut buf, size.width as usize, size.height as usize, frame),
            None => buf.fill(0),
        }
        buf.present().map_err(|err| anyhow!("{err}"))?;

        if size != self.size {
            self.size = size;
            self.schedule();
        }
        Ok(())
    }

    fn on_key(&mut self, el: &ActiveEventLoop, key: &Key) {
        let count = self.count.take();
        let n = isize::try_from(count.unwrap_or(1)).unwrap_or(isize::MAX);
        let jump = isize::try_from(self.cfg.jump).unwrap_or(isize::MAX);
        let from = self.files.index();
        let last = self.files.len().saturating_sub(1);

        let moved = match key.as_ref() {
            Key::Character("q") => {
                el.exit();
                false
            }
            Key::Character("n") | Key::Named(NamedKey::Space) => self.files.step(n),
            Key::Character("p") | Key::Named(NamedKey::Backspace) => self.files.step(-n),
            Key::Character("]") => self.files.step(n.saturating_mul(jump)),
            Key::Character("[") => self.files.step(n.saturating_mul(jump).saturating_neg()),
            Key::Character("g") => self.files.go_to(0),
            Key::Character("f") => {
                self.toggle_fullscreen();
                false
            }
            Key::Character("G") => self
                .files
                .go_to(count.map_or(last, |c| c.saturating_sub(1))),
            Key::Character(c) => {
                if let Some(digit) = c.parse::<usize>().ok().filter(|_| c.len() == 1) {
                    let count = count.unwrap_or(0).saturating_mul(10).saturating_add(digit);
                    self.count = Some(count);
                }
                false
            }
            _ => false,
        };
        if moved {
            let to = self.files.index();
            self.last_step = if to > from {
                isize::try_from(to - from).unwrap_or(isize::MAX)
            } else {
                isize::try_from(from - to).map_or(isize::MIN, |step| -step)
            };
            self.schedule();
            self.request_redraw();
        }
    }

    fn on_loaded(&mut self, loaded: loader::LoadResult) {
        let Some(idx) = self.files.position(loaded.job.idx, &loaded.job.path) else {
            return;
        };
        let cur = self.files.index();
        self.cache.insert(idx, loaded);
        self.cache.trim(cur);
        self.schedule();
        if idx == cur {
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
        self.size = match monitor {
            Some(monitor) if self.args.fullscreen => monitor.size(),
            _ => WINDOW_SIZE,
        };
        self.schedule();

        let res = self.init_window(el);
        self.exit_on_err(el, res);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Listed(list) if list.is_empty() => {
                self.exit_on_err(el, Err(anyhow!("no images to show")));
            }
            ViewerEvent::Listed(list) => {
                self.files.set_list(list);
                self.cache.reindex(|path| self.files.position(0, path));
                self.schedule();
                self.request_redraw();
            }
            ViewerEvent::Loaded(loaded) => self.on_loaded(loaded),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                self.on_key(el, &event.logical_key);
            }
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::RedrawRequested => {
                let res = self.redraw();
                self.exit_on_err(el, res);
            }
            _ => {}
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

fn main() -> Result<()> {
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
        args,
        cfg,
        files,
        size: PhysicalSize::default(),
        last_step: 1,
        shown: None,
        count: None,
        window: None,
        surface: None,
        err: None,
    };
    el.run_app(&mut app)?;
    app.err.map_or(Ok(()), Err)
}
