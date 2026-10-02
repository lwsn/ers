# ers

A terminal ebook reader written in Rust. It is a port of
[epy](https://github.com/wustho/epy) by wustho, built on
[ratatui](https://ratatui.rs).

Supported formats: EPUB (2 and 3), FictionBook (`.fb2`) and HTML pages by URL.
MOBI/AZW are not supported yet.

## Install

```sh
cargo install --path .
```

## Usage

```sh
ers /path/to/ebook    # read a file
ers                   # reopen the last read book
ers 3                 # read #3 from the reading history
ers count monte       # read the history entry matching "count monte"
ers -r                # print reading history
ers -d book.epub      # dump the book's text to stdout
```

Press `?` while reading to see all key bindings. The most common:

| Key | Action |
| --- | --- |
| `j` / `k` | scroll down / up |
| `l` / `h`, `Space` / `Backspace` | next / previous page |
| `L` / `H` | next / previous chapter |
| `t` | table of contents |
| `%` | go to a percentage of the book (Enter to jump, Esc to cancel) |
| `/` | regex search (`n` / `N` for next / previous match) |
| `-` / `+` | narrower / wider page (10 columns per press) |
| `=` | toggle between 80 columns and full width |
| `D` | toggle double-page spread |
| `,` / `.` | cycle top / bottom padding (0, 2, 4, 8 rows) |
| `b` / `B` | add bookmark / show bookmarks |
| `R` | library |
| `q` | quit |

Prefix a movement with a number to repeat it, eg. `5j`.

## Configuration

Settings and key bindings live in `~/.config/ers/configuration.json`
(or `~/.ers/` if `~/.config` doesn't exist), created on first run. Reading
positions, bookmarks and history are stored in `states.db` next to it.

## License

GPL-3.0-only, the same as epy. See [LICENSE](LICENSE).
