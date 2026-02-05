# ccache-tui

Live terminal UI for `ccache -s` with trends and summary stats.

## Features
- Polls `ccache -s` on a configurable interval
- Charts for hits/misses, hit rate, and cache size
- Rolling window history (default 5 minutes)
- Error overlay with stderr and exit code
- Linux static binary release workflow (musl)

## Install
### From release
Download the `ccache-tui-linux-x86_64-musl` binary from the GitHub release page and make it executable.

### From source
```bash
cargo build --release
```

## Usage
```bash
cargo run -- --window-seconds 300 --poll-ms 1000 --theme classic
```

## Options
- `--window-seconds`: history window length in seconds (default: 300)
- `--poll-ms`: polling interval in milliseconds (default: 1000)
- `--theme`: color theme (`classic`, `mono`, `solar`)

## Controls
- `q` or `Esc`: quit
- `Ctrl-C`: quit
