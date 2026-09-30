# truetm

A terminal multiplexer inspired by [dvtm](https://www.brain-dump.org/projects/dvtm/) with truecolor support.

## Features

- **Truecolor support** - Full 24-bit RGB color passthrough
- **dvtm-style tagging** - Windows can have multiple tags, views can show multiple tags
- **Tiling layout** - Master window on left, stack on right

## Installation

### Requirements

- Rust 1.70+ and Cargo
- A C compiler (gcc or clang)

On Debian/Ubuntu:
```sh
apt install build-essential
```

On Arch:
```sh
pacman -S base-devel
```

On macOS, install Xcode Command Line Tools:
```sh
xcode-select --install
```

### From source

```sh
git clone https://github.com/theludd/truetm
cd truetm
cargo build --release
sudo cp target/release/truetm /usr/local/bin/
```

### Arch Linux (pacman)

A `PKGBUILD` is included in the repo:

```sh
git clone https://github.com/theludd/truetm
cd truetm
makepkg -si
```

Since configuration is compile-time (see [Configuration](#configuration)), edit `src/config.rs` *before* running `makepkg`. The package is built from your working tree, so your changes are baked in. To change settings later, edit `src/config.rs`, bump `pkgrel` in the `PKGBUILD`, and run `makepkg -fsi` again.

### Alpine Linux (apk)

An `APKBUILD` is included in the repo:

```sh
abuild-keygen -a -i
git clone https://github.com/theludd/truetm
cd truetm
abuild -r
```

`abuild-keygen -a -i` generates a local signing key and installs it into `/etc/apk/keys`; it only needs to run once per machine. As with the `PKGBUILD`, this builds from your working tree, so edit `src/config.rs` before running `abuild -r` to bake in your settings, and bump `pkgrel` before rerunning it to pick up later changes.

## Usage

```sh
truetm
```

## Configuration

truetm follows the dwm philosophy: configuration is done at compile time by editing `src/config.rs`. This file contains all keybindings and settings in a readable format. After making changes, recompile with `cargo build --release` (or rebuild the package with `makepkg -f` on Arch).

## Default Keybindings

All keybindings use `Ctrl+B` as the prefix key.

### Window Management

| Key            | Action                                       |
| -------------- | -------------------------------------------- |
| `Ctrl+B c`     | Create new window                            |
| `Ctrl+B x`     | Close focused window                         |
| `Ctrl+B h`     | Focus window to the left                     |
| `Ctrl+B j`     | Focus window below                           |
| `Ctrl+B k`     | Focus window above                           |
| `Ctrl+B l`     | Focus window to the right                    |
| `Ctrl+B Enter` | Swap focused window with master              |
| `Ctrl+B H`     | Decrease master width                        |
| `Ctrl+B L`     | Increase master width                        |
| `Ctrl+B z`     | Toggle zoom (fullscreen focused window)      |
| `Ctrl+B 1-9`   | Focus window by number                       |
| `Ctrl+B a`     | Toggle broadcast mode (input to all windows) |
| `Ctrl+B Q`     | Quit truetm                                  |
| `Ctrl+B b`     | Send literal Ctrl+B to window                |

### Tags (Workspaces)

| Key          | Action                                            |
| ------------ | ------------------------------------------------- |
| `Ctrl+B v N` | View tag N (1-9)                                  |
| `Ctrl+B t N` | Set tag N on focused window (replaces other tags) |
| `Ctrl+B T N` | Toggle tag N on focused window                    |

Tags work like virtual desktops but more flexible:
- A window can have multiple tags (appear in multiple views)
- Closing the last window in a tag returns to the previously visited tag

The status bar labels each tag with the working directory of its master
window, so tags read as `1 (truetm) 2 (Documents) 3 (aios-a…build)` instead of
bare numbers. The label follows the shell as it `cd`s, and long names are
trimmed in the middle - sibling worktrees share a prefix and differ at the
tail, so both ends are kept. A tag holding more than one window shows the
count, as in `2 (Documents×3)`. Tags fall back to a bare number when the
status bar runs out of width.

### Copy Mode (Vim-style Scrollback)

Enter copy mode with:
- `Ctrl+B s` - enter copy mode
- `Ctrl+B PgUp/PgDown` - enter and page up/down
- `Ctrl+B ↑/↓` - enter and move up/down

Supports numeric counts (e.g., `5j` to move 5 lines down). Press `Esc` to exit visual selection, `Esc` again (or `q`) to exit copy mode.

#### Basic Movement

| Key           | Action                              |
| ------------- | ----------------------------------- |
| `h/j/k/l`     | Move cursor left/down/up/right      |
| `0`           | Move to start of line               |
| `$`           | Move to end of line                 |
| `^`           | Move to first non-blank character   |
| `g`           | Go to top of scrollback             |
| `G`           | Go to bottom (live view)            |
| `H/M/L`       | Move to top/middle/bottom of screen |
| `PgUp/PgDown` | Page up/down                        |

#### Word Motions

| Key   | Action                              |
| ----- | ----------------------------------- |
| `w/W` | Move to start of next word/WORD     |
| `b/B` | Move to start of previous word/WORD |
| `e/E` | Move to end of word/WORD            |

#### Search

| Key   | Action                         |
| ----- | ------------------------------ |
| `/`   | Search forward (regex)         |
| `?`   | Search backward (regex)        |
| `n`   | Jump to next match             |
| `N`   | Jump to previous match         |
| `f/F` | Find char forward/backward     |
| `t/T` | Find char (till) forward/back  |
| `;`   | Repeat last find               |
| `,`   | Repeat last find (reverse)     |

#### Visual Mode & Text Objects

| Key   | Action                              |
| ----- | ----------------------------------- |
| `v`   | Start character-wise visual select  |
| `V`   | Start line-wise visual select       |
| `y`   | Yank (copy) selection to clipboard  |
| `iw`  | Select inner word                   |
| `aw`  | Select around word (includes space) |
| `i"`  | Select inside quotes                |
| `a"`  | Select around quotes                |
| `i(`  | Select inside parentheses           |
| `a(`  | Select around parentheses           |
| `i[`  | Select inside brackets              |
| `i{`  | Select inside braces                |

#### Exit

| Key         | Action         |
| ----------- | -------------- |
| `q` / `Esc` | Exit copy mode |

Scrollback stores up to 10,000 lines of history per window.

### Mouse

| Action         | Effect                                      |
| -------------- | ------------------------------------------- |
| Click and drag | Select text within a pane                   |
| Scroll wheel   | Scroll through scrollback (enters copy mode)|

Applications that enable mouse reporting (Claude Code, vim, less with
`--mouse`, htop, ...) receive mouse events directly, so the wheel scrolls
the application rather than truetm's scrollback. Hold Shift to bypass the
application and use truetm's own selection and scrollback instead.

## Author

Fully vibe coded with [Claude Code](https://claude.com/claude-code) and Opus 4.5.

## License

[Unlicense](UNLICENSE) - Public domain. Do whatever you want.
