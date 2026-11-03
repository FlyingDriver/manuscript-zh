# Benchmark: Manuscript vs Apostrophe

`bench.py` launches each app on generated Markdown files and measures startup, memory and installed size. It writes `bench/results.md`.

## Prerequisites

- `flatpak` with both apps installed: `io.gitlab.ilshat_apps.manuscript`, `org.gnome.gitlab.somas.Apostrophe` (a missing app is skipped with a message).
- A running session with the accessibility bus (AT-SPI). Toolkit accessibility must be on: `gsettings set org.gnome.desktop.interface toolkit-accessibility true` if no windows are detected.
- Python 3 with PyGObject and the Atspi typelib (`python3-gobject`, `at-spi2-core`). No pip packages.

The script opens real windows on your desktop. Do not touch the machine while it runs, and close other heavy apps.

## Run

```bash
python3 bench/bench.py                       # both apps, 3 files, 10 runs
python3 bench/bench.py --runs 5 --settle 3
python3 bench/bench.py --apps manuscript --files small,large
python3 bench/bench.py --manuscript-bin target/release/manuscript
python3 bench/bench.py --keep-fixtures       # keep the generated files for inspection
```

## Fixtures (generated in a temp dir, deleted at exit)

- `small.md` - about 2 KB: headings, lists, paragraphs.
- `large.md` - about 1 MB, deterministic: repeated sections with headings, paragraphs, bullet/numbered lists, rust/python code blocks, quotes, a table every 4th section.
- `readme.md` - the repo `README.md` (images, table, HTML blocks) with `data/screenshots` and `data/icons` copied next to it.

## What is measured

1. **Startup to window.** Time from `Popen` to the first moment AT-SPI shows an application whose name contains the app name (case-insensitive, or whose process id is in the launched tree) with at least one child window that is SHOWING or ACTIVE. Polled every 10 ms, 30 s timeout. One untimed warm-up launch per app/file, then N timed runs; median, min, max reported. This is "window mapped", not "text painted": an app that maps an empty window early and renders later looks faster than it is.
2. **Memory.** After the window appears plus `--settle` seconds, the sum of **PSS** (`/proc/<pid>/smaps_rollup`) over the whole process tree of the instance, including helpers such as WebKit processes. For Flatpak the root is the sandbox `child-pid` from `flatpak ps`, so the `flatpak run` / bwrap wrapper overhead is not counted. RSS sum is also reported for reference. Median over runs. Unreadable or exited processes count as zero.
3. **Installed size.** The `Installed:` value of `flatpak info APPID` and of its runtime (`flatpak info --show-runtime`), plus the sum. Runtimes are shared between apps, so app + runtime overstates the marginal cost when the runtime is already installed. Flatpak prints SI units (1 MB = 10^6 bytes).

## Fairness and caveats

- Flatpaks run via `flatpak run APPID FILE` with the fixtures in a temp dir under XDG Documents (`xdg-user-dir DOCUMENTS`), which both sandboxes can read directly (Manuscript: `xdg-documents`, Apostrophe: `host`) — so the `readme.md` fixture's relative images resolve in both. With `--manuscript-bin` the binary runs unsandboxed, which is not comparable with the Flatpak Apostrophe numbers.
- Before and after every run `flatpak kill APPID` is executed and the script waits until `flatpak ps` no longer lists the app (a running GApplication instance would swallow the next launch).
- The first launch after boot or after the page cache is dropped is slower; hence the warm-up.
- Startup includes `flatpak run` sandbox setup, which is the same cost for both apps but not zero.
- Results depend on the compositor, load, CPU governor and the theme; compare numbers from one session only.
- Window detection relies on the app exposing a frame to AT-SPI. If every run reports a timeout, check that accessibility is enabled.

## Not implemented

- TODO: scroll benchmark (`--scroll`): automate scrolling the large document and measure frame time / CPU.
