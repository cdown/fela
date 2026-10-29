use anyhow::{anyhow, Context, Result};
use clap::Parser;
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

mod decode;
mod files;
mod view;

const JUMP: isize = 10;

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
}

enum ViewerEvent {
    Listed(Vec<PathBuf>),
}

struct Viewer {
    files: files::FileList,
    img: Option<decode::DecodedImage>,
    frame: Option<decode::RenderedImage>,
    count: Option<usize>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    err: Option<anyhow::Error>,
}

fn read_image(path: &Path) -> Result<decode::DecodedImage> {
    decode::decode(&std::fs::read(path)?)
}

impl Viewer {
    fn init_window(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("fela")
            .with_inner_size(PhysicalSize::new(800, 600));
        let window = Rc::new(el.create_window(attrs)?);
        let ctx = softbuffer::Context::new(window.clone()).map_err(|err| anyhow!("{err}"))?;
        self.surface = Some(Surface::new(&ctx, window.clone()).map_err(|err| anyhow!("{err}"))?);
        self.window = Some(window);
        Ok(())
    }

    fn load_current(&mut self) {
        self.frame = None;
        self.img = self.files.current().and_then(|path| {
            read_image(path)
                .inspect_err(|err| eprintln!("{}: {err:#}", path.display()))
                .ok()
        });
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn redraw(&mut self) -> Result<()> {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };

        let stale = self.frame.as_ref().is_none_or(|f| {
            f.w > size.width || f.h > size.height || (f.w < size.width && f.h < size.height)
        });
        if let (true, Some(img)) = (stale, &self.img) {
            self.frame = Some(decode::fit(img, size.width, size.height, false)?);
        }

        surface.resize(w, h).map_err(|err| anyhow!("{err}"))?;
        let mut buf = surface.buffer_mut().map_err(|err| anyhow!("{err}"))?;
        match &self.frame {
            Some(frame) => view::draw(&mut buf, size.width as usize, size.height as usize, frame),
            None => buf.fill(0),
        }
        buf.present().map_err(|err| anyhow!("{err}"))
    }

    fn on_key(&mut self, el: &ActiveEventLoop, key: &Key) {
        let count = self.count.take();
        let n = isize::try_from(count.unwrap_or(1)).unwrap_or(isize::MAX);
        let last = self.files.len().saturating_sub(1);

        let moved = match key.as_ref() {
            Key::Character("q") => {
                el.exit();
                false
            }
            Key::Character("n") | Key::Named(NamedKey::Space) => self.files.step(n),
            Key::Character("p") | Key::Named(NamedKey::Backspace) => self.files.step(-n),
            Key::Character("]") => self.files.step(n.saturating_mul(JUMP)),
            Key::Character("[") => self.files.step(n.saturating_mul(-JUMP)),
            Key::Character("g") => self.files.go_to(0),
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
            self.load_current();
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
        if self.window.is_none() {
            let res = self.init_window(el);
            self.exit_on_err(el, res);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Listed(list) if list.is_empty() => {
                self.exit_on_err(el, Err(anyhow!("no images to show")));
            }
            ViewerEvent::Listed(list) => {
                let shown = self.files.current().cloned();
                self.files.set_list(list);
                if self.files.current() != shown.as_ref() {
                    self.load_current();
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                self.on_key(el, &event.logical_key);
            }
            WindowEvent::RedrawRequested => {
                let res = self.redraw();
                self.exit_on_err(el, res);
            }
            _ => {}
        }
    }
}

fn spawn_lister(args: Vec<PathBuf>, proxy: EventLoopProxy<ViewerEvent>) {
    std::thread::spawn(move || {
        // Only fails if the event loop has gone away, in which case nobody wants the list anyway
        let _ = proxy.send_event(ViewerEvent::Listed(files::expand_args(&args)));
    });
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut paths = args.paths;
    if args.stdin {
        for line in std::io::stdin().lines() {
            paths.push(line.context("failed to read paths from stdin")?.into());
        }
    }

    let mut app = Viewer {
        files: files::FileList::new(&paths),
        img: None,
        frame: None,
        count: None,
        window: None,
        surface: None,
        err: None,
    };
    app.load_current();

    let el = EventLoop::<ViewerEvent>::with_user_event().build()?;
    spawn_lister(paths, el.create_proxy());
    el.run_app(&mut app)?;
    app.err.map_or(Ok(()), Err)
}
