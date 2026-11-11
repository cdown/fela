# fela

fela is a small, keyboard-driven image viewer for Linux and Windows. It is
heavily inspired by [(n)sxiv](https://nsxiv.codeberg.page/).

Compared to (n)sxiv, it is made for going through large JPEGs and camera RAWs
quickly, including on slow disks and network paths.

## Usage

    % fela ~/photos
    % fela -f DSC01234.ARW
    % find . -name '*.jpg' | fela -i -o

## Keys

| Key                     | Action                                               |
|-------------------------|------------------------------------------------------|
| `n`, Space              | Next image                                           |
| `p`, Backspace          | Previous image                                       |
| `]`, `[`                | Forward and back by `jump` images                    |
| `g`, `G`                | First image, last image or image number *count*      |
| `+`, `-`                | Zoom in and out                                      |
| `=`                     | One image pixel per screen pixel, or *count* percent |
| `w`, `W`                | Fit large images to the window, fit all images       |
| `h` `j` `k` `l`, arrows | Pan when zoomed in                                   |
| `s`                     | Toggle sharpness checking                            |
| `<`, `>`                | Rotate                                               |
| `m`, `M`                | Mark, unmark                                         |
| `N`, `P`                | Next and previous marked image                       |
| Ctrl-u                  | Remove all marks                                     |
| `D`                     | Drop the image from the list                         |
| `r`                     | Reload                                               |
| `b`                     | Toggle the status bar                                |
| `f`                     | Toggle fullscreen                                    |
| Ctrl-x                  | Send the next key to the key handler                 |
| `q`                     | Quit if no files are marked                          |
| `Q`                     | Quit even with marked files                         |

Most keys take a vim-style count typed before them. For example, `5n` goes five
images forward.

## Configuration

fela reads `%APPDATA%\fela\config.toml` on Windows, and
`$XDG_CONFIG_HOME/fela/config.toml` on Linux. These are the defaults:

    jump = 5
    prefetch_ahead = 4
    prefetch_behind = 1
    prefetch_jump = true
    prefetch_inflight = 4
    cache_mb = 512
    fullscreen = false

## Key handling

fela supports key handlers. To use them, press Ctrl-x and then another key.
fela then runs the handler with the key sent as its last argument and sends
marked paths on stdin, one per line.

You can write key handlers in any language. For example, on Linux with a script
or program:

    handler = ["/home/cdown/.config/nsxiv/exec/key-handler"]

Or on Windows, with a handler written in PowerShell:

    handler = ["powershell", "-NoProfile", "-File", "C:\\Users\\cdown\\key-handler.ps1"]

    param($key)
    $files = @($input)
    switch ($key) {
        'C-d' { Remove-Item -LiteralPath $files }
    }
