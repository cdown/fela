use crate::ViewerEvent;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Debug, Default)]
pub struct HandlerChanges {
    pub gone: Vec<PathBuf>,
    pub changed: Vec<PathBuf>,
}

pub fn key_name(key: &Key, mods: ModifiersState) -> Option<String> {
    let (name, is_shift_in_name) = match key {
        Key::Character(text) if text.chars().count() == 1 => (text.to_string(), true),
        Key::Named(
            NamedKey::Alt
            | NamedKey::AltGraph
            | NamedKey::CapsLock
            | NamedKey::Control
            | NamedKey::Fn
            | NamedKey::FnLock
            | NamedKey::Hyper
            | NamedKey::Meta
            | NamedKey::NumLock
            | NamedKey::ScrollLock
            | NamedKey::Shift
            | NamedKey::Super
            | NamedKey::Symbol
            | NamedKey::SymbolLock,
        ) => return None,
        Key::Named(named) => (format!("{named:?}"), false),
        _ => return None,
    };
    let ctrl = if mods.control_key() { "C-" } else { "" };
    let alt = if mods.alt_key() { "M-" } else { "" };
    let shift = if mods.shift_key() && !is_shift_in_name {
        "S-"
    } else {
        ""
    };
    Some(format!("{ctrl}{alt}{shift}{name}"))
}

pub fn write_path(out: &mut impl Write, path: &Path) -> std::io::Result<()> {
    out.write_all(path.as_os_str().as_encoded_bytes())?;
    out.write_all(b"\n")
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

fn run(argv: &[String], key: &str, paths: &[PathBuf]) -> Result<HandlerChanges> {
    let (program, args) = argv.split_first().context("no key handler configured")?;
    let before: Vec<_> = paths.iter().map(|path| modified(path)).collect();

    let mut cmd = Command::new(program);
    cmd.args(args).arg(key).stdin(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    let mut stdin = child.stdin.take().context("BUG: handler has no stdin")?;
    for path in paths {
        // A handler is free to not read its input, in which case this fails with a broken pipe
        if write_path(&mut stdin, path).is_err() {
            break;
        }
    }
    drop(stdin);
    child.wait()?;

    let mut outcome = HandlerChanges::default();
    for (path, before) in paths.iter().zip(before) {
        match modified(path) {
            None => outcome.gone.push(path.clone()),
            after if after != before => outcome.changed.push(path.clone()),
            _ => {}
        }
    }
    Ok(outcome)
}

pub fn spawn(
    argv: Vec<String>,
    key: String,
    paths: Vec<PathBuf>,
    proxy: EventLoopProxy<ViewerEvent>,
) {
    std::thread::spawn(move || {
        let result = run(&argv, &key, &paths).map_err(|err| format!("{err:#}"));
        // Only fails if the event loop has gone away, in which case we are exiting
        let _ = proxy.send_event(ViewerEvent::HandlerDone(result));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(key: Key, mods: ModifiersState) -> Option<String> {
        key_name(&key, mods)
    }

    #[test]
    fn test_key_name_plain() {
        let none = ModifiersState::empty();
        assert_eq!(name(Key::Character("d".into()), none).unwrap(), "d");
        assert_eq!(name(Key::Character(",".into()), none).unwrap(), ",");
        assert_eq!(name(Key::Named(NamedKey::Enter), none).unwrap(), "Enter");
    }

    #[test]
    fn test_key_name_modifiers() {
        let ctrl = ModifiersState::CONTROL;
        let all = ctrl | ModifiersState::ALT | ModifiersState::SHIFT;
        assert_eq!(name(Key::Character("d".into()), ctrl).unwrap(), "C-d");
        assert_eq!(name(Key::Character(".".into()), ctrl).unwrap(), "C-.");
        assert_eq!(
            name(Key::Named(NamedKey::Delete), all).unwrap(),
            "C-M-S-Delete"
        );
    }

    #[test]
    fn test_key_name_shift_is_folded_into_characters() {
        let shift = ModifiersState::SHIFT;
        assert_eq!(name(Key::Character("K".into()), shift).unwrap(), "K");
        assert_eq!(name(Key::Character("<".into()), shift).unwrap(), "<");
    }

    #[test]
    fn test_key_name_unusable() {
        let none = ModifiersState::empty();
        assert_eq!(name(Key::Named(NamedKey::Control), none), None);
        assert_eq!(name(Key::Character("ab".into()), none), None);
    }

    #[test]
    fn test_write_path() {
        let path = r"\\wsl.localhost\Arch\home\cdown\a b.jpg";
        let mut out = Vec::new();
        write_path(&mut out, Path::new(path)).unwrap();
        assert_eq!(out, format!("{path}\n").as_bytes());
    }
}
