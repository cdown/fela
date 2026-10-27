use anyhow::{anyhow, Context, Result};
use clap::Parser;
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::Key;
use winit::window::{Window, WindowId};

mod decode;
mod view;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(help = "DecodedImage to show")]
    path: PathBuf,
}

struct Viewer {
    img: decode::DecodedImage,
    frame: Option<decode::RenderedImage>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    err: Option<anyhow::Error>,
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
        if stale {
            self.frame = Some(decode::fit(&self.img, size.width, size.height, false)?);
        }
        let frame = self.frame.as_ref().context("BUG: no frame to draw")?;

        surface.resize(w, h).map_err(|err| anyhow!("{err}"))?;
        let mut buf = surface.buffer_mut().map_err(|err| anyhow!("{err}"))?;
        view::draw(&mut buf, size.width as usize, size.height as usize, frame);
        buf.present().map_err(|err| anyhow!("{err}"))
    }

    fn exit_on_err(&mut self, el: &ActiveEventLoop, res: Result<()>) {
        if let Err(err) = res {
            self.err = Some(err);
            el.exit();
        }
    }
}

impl ApplicationHandler for Viewer {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() {
            let res = self.init_window(el);
            self.exit_on_err(el, res);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                if event.logical_key == Key::Character("q".into()) {
                    el.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                let res = self.redraw();
                self.exit_on_err(el, res);
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let data = std::fs::read(&args.path)
        .with_context(|| format!("failed to read {}", args.path.display()))?;
    let mut app = Viewer {
        img: decode::decode(&data)?,
        frame: None,
        window: None,
        surface: None,
        err: None,
    };

    EventLoop::new()?.run_app(&mut app)?;
    app.err.map_or(Ok(()), Err)
}
