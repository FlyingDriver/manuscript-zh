#!/usr/bin/env python3
"""Benchmark Manuscript against Apostrophe: startup-to-window, PSS memory, install size.

Stdlib + PyGObject (Atspi) only. See bench/README.md for method and caveats.
"""
import argparse
import datetime
import os
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
APPS = {
    "manuscript": {"id": "io.gitlab.ilshat_apps.manuscript", "name": "Manuscript"},
    "apostrophe": {"id": "org.gnome.gitlab.somas.Apostrophe", "name": "Apostrophe"},
}
FILES = ("small", "large", "readme")
POLL_S = 0.010
WINDOW_TIMEOUT_S = 30.0


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------

def make_small():
    return (
        "# Small document\n\n"
        "A short paragraph with **bold**, *italic* and `inline code`, used to measure\n"
        "the cost of opening a tiny file.\n\n"
        "## Shopping list\n\n- Apples\n- Bread\n- Coffee\n\n"
        "## Steps\n\n1. Open the file\n2. Read it\n3. Close it\n\n"
        "> A blockquote for good measure.\n\n"
        + ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor "
           "incididunt ut labore et dolore magna aliqua.\n\n") * 8
    )


def make_large(target_bytes=1_000_000):
    """Deterministic ~1 MB document: repeated sections, a table every 4th one."""
    code = {
        "rust": "fn fib(n: u64) -> u64 {\n    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n}\n",
        "python": "def fib(n):\n    return n if n < 2 else fib(n - 1) + fib(n - 2)\n",
    }
    out, size, i = ["# Large benchmark document\n\n"], 0, 0
    while size < target_bytes:
        i += 1
        lang = "rust" if i % 2 else "python"
        sec = (
            f"## Section {i}\n\n"
            f"Paragraph {i}: the quick brown fox jumps over the lazy dog. **Bold {i}**, "
            f"*italic {i}*, `code_{i}` and a [link](https://example.com/{i}). "
            "Pack my box with five dozen liquor jugs, how vexingly quick daft zebras jump.\n\n"
            f"### Notes {i}\n\n- First point of {i}\n- Second point with `code`\n"
            "  - Nested item\n- Third point\n\n"
            "1. Step one\n2. Step two\n3. Step three\n\n"
            f"```{lang}\n{code[lang]}```\n\n"
            f"> Quote {i}: simplicity is prerequisite for reliability.\n\n"
        )
        if i % 4 == 0:
            sec += (
                "| Name | Value | Note |\n|------|-------|------|\n"
                + "".join(f"| row{r} | {i * r} | cell text {r} |\n" for r in range(1, 6))
                + "\n"
            )
        out.append(sec)
        size += len(sec.encode())
    return "".join(out)


def make_fixtures(root):
    """Create small.md, large.md and readme.md (+ data/ assets) in root; return name -> path."""
    (root / "small.md").write_text(make_small())
    (root / "large.md").write_text(make_large())
    rd = root / "readme"
    rd.mkdir()
    shutil.copy(REPO / "README.md", rd / "readme.md")
    for sub in ("screenshots", "icons"):
        shutil.copytree(REPO / "data" / sub, rd / "data" / sub)
    return {"small": root / "small.md", "large": root / "large.md", "readme": rd / "readme.md"}


# --------------------------------------------------------------------------
# Process control
# --------------------------------------------------------------------------

def run(cmd):
    # C locale: stable field names and "." decimals in flatpak's output.
    return subprocess.run(cmd, capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"})


def flatpak_installed(app_id):
    return run(["flatpak", "info", app_id]).returncode == 0


def flatpak_instances(app_id):
    """List (wrapper_pid, sandbox_child_pid) of running instances of app_id."""
    r = run(["flatpak", "ps", "--columns=instance,pid,child-pid,application"])
    found = []
    for line in r.stdout.splitlines():
        cols = [c.strip() for c in line.split("\t")]
        if len(cols) >= 4 and cols[3] == app_id and cols[1].isdigit():
            found.append((int(cols[1]), int(cols[2]) if cols[2].isdigit() else 0))
    return found


def children_of(pid):
    """Direct children of pid, from every thread's task/*/children."""
    kids = set()
    try:
        for task in os.listdir(f"/proc/{pid}/task"):
            try:
                kids.update(int(x) for x in Path(f"/proc/{pid}/task/{task}/children").read_text().split())
            except OSError:
                pass
    except OSError:
        pass
    return kids


def process_tree(roots):
    """All pids reachable from roots (inclusive) by walking /proc children."""
    seen, todo = set(), [r for r in roots if r]
    while todo:
        pid = todo.pop()
        if pid not in seen:
            seen.add(pid)
            todo.extend(children_of(pid))
    return seen


def kill_instance(spec, proc):
    """Make sure no instance of the app survives (errors ignored)."""
    if spec["flatpak"]:
        run(["flatpak", "kill", spec["id"]])
        deadline = time.monotonic() + 10
        while flatpak_instances(spec["id"]) and time.monotonic() < deadline:
            time.sleep(0.1)
    elif proc is not None:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except OSError:
            pass
    if proc is not None:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


def documents_dir():
    """XDG Documents (readable by both sandboxes); ~/Documents fallback."""
    out = run(["xdg-user-dir", "DOCUMENTS"]).stdout.strip() if shutil.which("xdg-user-dir") else ""
    d = Path(out) if out and Path(out) != Path.home() else Path.home() / "Documents"
    d.mkdir(parents=True, exist_ok=True)
    return d


def launch(spec, path):
    # Plain path, no --file-forwarding: fixtures live in XDG Documents, which
    # both sandboxes can read directly (Manuscript: xdg-documents,
    # Apostrophe: host). The portal would export only the single file, so
    # the README fixture's relative images wouldn't resolve in either app.
    cmd = (["flatpak", "run", spec["id"], str(path)]
           if spec["flatpak"] else [spec["bin"], str(path)])
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)


# --------------------------------------------------------------------------
# Metric 1: startup to mapped window (AT-SPI)
# --------------------------------------------------------------------------

def atspi_has_window(Atspi, name, pids):
    """True if an AT-SPI application matching name (or one of pids) has a showing window."""
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        try:
            app = desktop.get_child_at_index(i)
            if app is None:
                continue
            app_name = (app.get_name() or "").lower()
            try:
                app_pid = app.get_process_id()
            except Exception:
                app_pid = -1
            if name.lower() not in app_name and app_pid not in pids:
                continue
            for j in range(app.get_child_count()):
                win = app.get_child_at_index(j)
                states = win.get_state_set() if win else None
                if states and (states.contains(Atspi.StateType.SHOWING)
                               or states.contains(Atspi.StateType.ACTIVE)):
                    return True
        except Exception:
            continue  # accessible vanished mid-walk
    return False


def wait_for_window(Atspi, spec, proc, t0):
    """Seconds from t0 until the window shows, or None on timeout."""
    deadline = t0 + WINDOW_TIMEOUT_S
    while time.monotonic() < deadline:
        if atspi_has_window(Atspi, spec["name"], process_tree([proc.pid])):
            return time.monotonic() - t0
        time.sleep(POLL_S)
    return None


def wait_atspi_gone(Atspi, spec, timeout=5.0):
    """A stale AT-SPI registration from a killed run would give a false instant match."""
    deadline = time.monotonic() + timeout
    while atspi_has_window(Atspi, spec["name"], set()) and time.monotonic() < deadline:
        time.sleep(0.1)


# --------------------------------------------------------------------------
# Metric 2: memory (PSS over the whole process tree)
# --------------------------------------------------------------------------

def read_kb(pid, key):
    """One kB field of /proc/<pid>/smaps_rollup; 0 if the process is gone or unreadable."""
    try:
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            if line.startswith(key + ":"):
                return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return 0


def memory_kb(spec, proc):
    """(PSS KiB, RSS KiB, process count) summed over the app's tree, helpers included."""
    roots = [proc.pid]
    if spec["flatpak"]:
        # Count the sandbox (child-pid) subtree, not the flatpak/bwrap wrapper overhead.
        roots = [c for _, c in flatpak_instances(spec["id"]) if c] or roots
    pids = process_tree(roots)
    return (sum(read_kb(p, "Pss") for p in pids),
            sum(read_kb(p, "Rss") for p in pids), len(pids))


# --------------------------------------------------------------------------
# Metric 3: installed size
# --------------------------------------------------------------------------

UNITS = {"bytes": 1, "kB": 1e3, "MB": 1e6, "GB": 1e9}


def parse_size(info_text):
    # "Installed Size: 6,9 MB" — older flatpak prints "Installed:", and the
    # decimal separator follows the locale (run() forces LC_ALL=C anyway).
    m = re.search(r"Installed(?: Size)?:\s*([\d.,]+)\s*(\S+)", info_text)
    if not m:
        return None
    return float(m.group(1).replace(",", ".")) * UNITS.get(m.group(2), 1)


def field(info_text, key):
    m = re.search(rf"^\s*{key}:\s*(.+)$", info_text, re.M)
    return m.group(1).strip() if m else "?"


def install_info(app_id):
    """Version, app size and runtime name/size (sizes in bytes, None if unparsable)."""
    info = run(["flatpak", "info", app_id]).stdout
    runtime = run(["flatpak", "info", "--show-runtime", app_id]).stdout.strip()
    rt_info = run(["flatpak", "info", runtime]).stdout if runtime else ""
    return {"version": field(info, "Version"), "app": parse_size(info),
            "runtime": parse_size(rt_info), "runtime_name": runtime or "?"}


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------

def one_run(Atspi, spec, path, settle, measure):
    """Launch once; return (startup_s, pss_kb, rss_kb); None fields on failure/warm-up."""
    kill_instance(spec, None)
    wait_atspi_gone(Atspi, spec)
    t0 = time.monotonic()
    proc = launch(spec, path)
    try:
        startup = wait_for_window(Atspi, spec, proc, t0)
        if startup is None or not measure:
            return startup, None, None
        time.sleep(settle)
        pss, rss, n = memory_kb(spec, proc)
        log(f"    {startup * 1000:7.0f} ms  PSS {pss / 1024:7.1f} MiB  ({n} procs)")
        return startup, pss, rss
    finally:
        kill_instance(spec, proc)


def bench_app(Atspi, key, spec, files, runs, settle):
    res = {}
    for fname, path in files.items():
        log(f"[{key}] {fname}: warm-up")
        one_run(Atspi, spec, path, 0, False)
        samples = {"startup": [], "pss": [], "rss": []}
        for n in range(1, runs + 1):
            log(f"[{key}] {fname}: run {n}/{runs}")
            s, p, r = one_run(Atspi, spec, path, settle, True)
            if s is None or p is None:
                log("    window did not appear within timeout; run discarded")
                continue
            samples["startup"].append(s)
            samples["pss"].append(p)
            samples["rss"].append(r)
        res[fname] = samples
    return res


# --------------------------------------------------------------------------
# Report
# --------------------------------------------------------------------------

def med(vals):
    return statistics.median(vals) if vals else None


def stat(vals, scale, unit, digits):
    if not vals:
        return "n/a"
    f = lambda v: f"{v / scale:.{digits}f}"
    return f"{f(med(vals))} {unit} ({f(min(vals))}-{f(max(vals))})"


def ratio(a, m):
    return f"{a / m:.2f}x" if a and m else "n/a"


def cpu_and_ram():
    cpu, mem_kb = "?", 0
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            cpu = line.split(":", 1)[1].strip()
            break
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            mem_kb = int(line.split()[1])
    return cpu, mem_kb / 1048576


def metric_table(title, apps, results, files, kind, scale, unit, digits, with_ratio):
    """Rows = files, columns = apps; ratio column when both apps ran."""
    ratio_col = with_ratio and "manuscript" in apps and "apostrophe" in apps
    cols = [APPS[a]["name"] for a in apps] + (["Apostrophe / Manuscript"] if ratio_col else [])
    lines = [f"### {title}", "", "median (min-max)", "",
             "| File | " + " | ".join(cols) + " |", "|---" * (len(cols) + 1) + "|"]
    for f in files:
        row = [stat(results[a][f][kind], scale, unit, digits) for a in apps]
        if ratio_col:
            row.append(ratio(med(results["apostrophe"][f][kind]), med(results["manuscript"][f][kind])))
        lines.append(f"| {f} | " + " | ".join(row) + " |")
    return lines + [""]


def size_table(sizes):
    mb = lambda v: f"{v / 1e6:.1f} MB" if v else "n/a"
    names = [APPS[a]["name"] for a in sizes]
    return ["### Installed size", "", "| | " + " | ".join(names) + " |", "|---" * (len(names) + 1) + "|",
            "| App | " + " | ".join(mb(s["app"]) for s in sizes.values()) + " |",
            "| Runtime | " + " | ".join(f"{mb(s['runtime'])} ({s['runtime_name']})" for s in sizes.values()) + " |",
            "| App + runtime | " + " | ".join(mb((s["app"] or 0) + (s["runtime"] or 0)) for s in sizes.values()) + " |",
            ""]


def write_report(path, args, apps, files, results, sizes):
    cpu, ram = cpu_and_ram()
    lines = ["# Benchmark results", "", "## Environment", "",
             f"- Date: {datetime.datetime.now().astimezone().isoformat(timespec='seconds')}",
             f"- Kernel: {platform.release()}", f"- CPU: {cpu}", f"- RAM: {ram:.1f} GiB",
             f"- Runs per file: {args.runs} (after 1 untimed warm-up), settle {args.settle} s"]
    for a in apps:
        how = f"local binary `{args.manuscript_bin}`" if a == "manuscript" and args.manuscript_bin else "Flatpak"
        version = sizes[a]["version"] if a in sizes else "n/a"
        lines.append(f"- {APPS[a]['name']}: {how}; installed Flatpak version {version}")
    lines.append("")
    lines += metric_table("Startup to window", apps, results, files, "startup", 1e-3, "ms", 0, True)
    lines += metric_table("Memory: PSS, whole process tree", apps, results, files, "pss", 1024, "MiB", 1, True)
    lines += metric_table("Memory: RSS sum (reference)", apps, results, files, "rss", 1024, "MiB", 1, False)
    if sizes:
        lines += size_table(sizes)
    path.write_text("\n".join(lines))


def parse_args():
    p = argparse.ArgumentParser(description="Benchmark Manuscript vs Apostrophe (startup, PSS memory, install size).")
    p.add_argument("--apps", default="manuscript,apostrophe", help="comma list: manuscript,apostrophe")
    p.add_argument("--files", default=",".join(FILES), help="comma list: small,large,readme")
    p.add_argument("--runs", type=int, default=10, help="timed runs per app/file (default 10)")
    p.add_argument("--settle", type=float, default=5.0, help="seconds before measuring memory (default 5)")
    p.add_argument("--manuscript-bin", metavar="PATH", help="run this local Manuscript binary instead of the Flatpak")
    p.add_argument("--keep-fixtures", action="store_true", help="keep the generated fixture directory")
    p.add_argument("--output", default=str(Path(__file__).parent / "results.md"), help="results file")
    a = p.parse_args()
    a.app_list = [x for x in a.apps.split(",") if x]
    a.file_list = [x for x in a.files.split(",") if x]
    for x in a.app_list:
        if x not in APPS:
            p.error(f"unknown app {x!r}")
    for x in a.file_list:
        if x not in FILES:
            p.error(f"unknown file {x!r}")
    return a


def main():
    args = parse_args()
    try:
        import gi
        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi
    except (ImportError, ValueError) as e:
        sys.exit(f"error: PyGObject with the Atspi typelib is required (python3-gobject): {e}")
    if not shutil.which("flatpak"):
        sys.exit("error: flatpak not found")
    Atspi.init()

    specs, sizes = {}, {}
    for key in args.app_list:
        spec = dict(APPS[key], flatpak=True, bin=None)
        if key == "manuscript" and args.manuscript_bin:
            spec.update(flatpak=False, bin=args.manuscript_bin)
        installed = flatpak_installed(spec["id"])
        if spec["flatpak"] and not installed:
            log(f"skip {key}: Flatpak {spec['id']} is not installed")
            continue
        specs[key] = spec
        if installed:
            sizes[key] = install_info(spec["id"])
    if not specs:
        sys.exit("error: no benchmarkable apps")

    tmp = Path(tempfile.mkdtemp(prefix="manuscript-bench-", dir=documents_dir()))
    try:
        fixtures = make_fixtures(tmp)
        files = {f: fixtures[f] for f in args.file_list}
        log(f"fixtures in {tmp}")
        results = {k: bench_app(Atspi, k, s, files, args.runs, args.settle) for k, s in specs.items()}
    finally:
        if args.keep_fixtures:
            log(f"fixtures kept in {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    write_report(Path(args.output), args, list(specs), list(files), results, sizes)
    log(f"wrote {args.output}")


if __name__ == "__main__":
    main()
