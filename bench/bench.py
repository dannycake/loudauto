#!/usr/bin/env python3
"""LOUDAUTO against Spotify's official desktop client, on the machine it runs on.

What it measures, for each client, alternating between them run by run:
  - startup: from launching the client until KWin maps its window (a KWin script logs each
    new window with a millisecond timestamp);
  - memory: PSS (proportional set size: shared pages split between the processes sharing
    them) summed over every process of the client, after it has sat idle for 20 s, and again
    after playing for 70 s;
  - CPU: the client's processes' user + system time over 30 s idle (window open, nothing
    playing) and over 60 s playing, as a percentage of one core;
  - processes and threads;
  - disk: the installed files.

Both clients play the same track. Nothing is heard: a temporary PipeWire null sink is made the
default and both clients are pointed at it; Easy Effects, which would move streams to its
own sink, is quit for the run. Everything is put back at the end, also on error or Ctrl-C:
the default sink, Easy Effects, LOUDAUTO's settings file, the KWin script.

Needs: KDE Plasma 6 on Wayland (KWin), PipeWire with pipewire-pulse (pactl), playerctl,
LOUDAUTO installed by scripts/install.sh, Spotify installed by spotify-launcher, both logged
in to the same Premium account. Playing takes over the account's playback for a few minutes
and the plays count as plays.

usage: bench/bench.py --track spotify:track:<id> [--runs 5] [--play-runs 2] [--out FILE]
"""

import argparse
import json
import os
import platform
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path
from statistics import median

HOME = Path.home()
BIN = HOME / ".local/bin"
SOLOIST = HOME / ".local/share/loudauto/soloist/soloist"
SETTINGS = Path(os.environ.get("XDG_CONFIG_HOME", HOME / ".config")) / "loudauto/settings.json"
SPOTIFY_DIR = HOME / ".local/share/spotify-launcher/install"
SPOTIFY_BIN = SPOTIFY_DIR / "usr/share/spotify/spotify"
SINK = "loudauto_bench"
CLK = os.sysconf("SC_CLK_TCK")
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024
KWIN_SCRIPT = """
workspace.windowAdded.connect(function (w) {
    print("BENCHWIN " + Date.now() + " " + w.resourceClass);
});
"""


def run(*cmd, check=True, **kw):
    return subprocess.run(cmd, check=check, capture_output=True, text=True, **kw).stdout.strip()


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


# ---- processes ---------------------------------------------------------------------------

def exe(pid):
    try:
        return os.path.realpath(f"/proc/{pid}/exe")
    except OSError:
        return ""


def pids_where(test):
    out = []
    for d in os.listdir("/proc"):
        if d.isdigit():
            e = exe(d)
            if e and test(e):
                out.append(int(d))
    return out


def pss_mb(pids):
    kb = 0
    for p in pids:
        try:
            for line in open(f"/proc/{p}/smaps_rollup"):
                if line.startswith("Pss:"):
                    kb += int(line.split()[1])
        except OSError:
            pass
    return kb / 1024


def threads(pids):
    n = 0
    for p in pids:
        try:
            n += int(re.search(r"^Threads:\s+(\d+)", open(f"/proc/{p}/status").read(), re.M)[1])
        except (OSError, TypeError):
            pass
    return n


def cpu_ticks(pids):
    ticks = {}
    for p in pids:
        try:
            fields = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            ticks[p] = int(fields[11]) + int(fields[12])  # utime, stime
        except (OSError, IndexError):
            pass
    return ticks


def cpu_percent(client, seconds):
    """Average CPU of the client's processes over `seconds`, in % of one core."""
    before = cpu_ticks(client.pids())
    t0 = time.monotonic()
    time.sleep(seconds)
    after = cpu_ticks(client.pids())
    spent = sum(t - before.get(p, 0) for p, t in after.items())
    return 100 * spent / CLK / (time.monotonic() - t0)


# ---- the two clients ---------------------------------------------------------------------

class Spotify:
    name = "Spotify (official)"
    window_class = "spotify"
    mpris = "spotify"

    def __init__(self):
        args = []
        conf = HOME / ".config/spotify-launcher.conf"
        if conf.exists():  # the same flags spotify-launcher would add
            args = tomllib.loads(conf.read_text()).get("spotify", {}).get("extra_arguments", [])
        self.cmd = [str(SPOTIFY_BIN), *args]
        self.proc = None

    def pids(self):
        root = str(SPOTIFY_DIR)
        return pids_where(lambda e: e.startswith(root))

    def launch(self):
        env = dict(os.environ, PULSE_SINK=SINK, PIPEWIRE_NODE=SINK)
        self.proc = subprocess.Popen(self.cmd, env=env, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, start_new_session=True)

    def play(self, uri):
        run("playerctl", "-p", "spotify", "open", uri)

    def pause(self):
        run("playerctl", "-p", "spotify", "pause", check=False)

    def quit(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
        stop_all(self.pids)

    def version(self):
        state = SPOTIFY_DIR.parent / "state.json"
        return json.loads(state.read_text()).get("version", "?") if state.exists() else "?"


class Loudauto:
    name = "LOUDAUTO"
    window_class = "loudauto"
    mpris = "loudauto"

    def __init__(self):
        self.files = {str(BIN / "loudauto"), str(BIN / "loudautod"), str(SOLOIST)}
        self.proc = None

    def pids(self):
        return pids_where(lambda e: e in self.files)

    def launch(self):
        # The window starts the engine (systemctl --user start loudauto.service), as when
        # it's opened from the app menu.
        self.proc = subprocess.Popen([str(BIN / "loudauto")], stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, start_new_session=True)

    def request(self, req):
        """One request over the engine's socket: big-endian u32 length, then JSON."""
        path = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "loudauto/engine.sock"
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10)
            s.connect(str(path))

            def send(msg):
                body = json.dumps(msg).encode()
                s.sendall(struct.pack(">I", len(body)) + body)

            def recv():
                n = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))[0]
                return json.loads(s.recv(n, socket.MSG_WAITALL))

            send({"t": "Hello", "version": 2, "role": {"t": "Cli"}})
            recv()  # Welcome
            send({"t": "Request", "id": 1, "req": req})
            while True:
                msg = recv()
                if msg.get("t") == "Reply" and msg["c"]["id"] == 1:
                    return msg["c"]["result"]

    def play(self, uri):
        self.request({"t": "PlayUri", "c": {"uri": uri}})

    def pause(self):
        try:
            self.request({"t": "Pause"})
        except OSError:
            pass

    def quit(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
        subprocess.run(["systemctl", "--user", "stop", "loudauto.service"], check=False)
        stop_all(self.pids)

    def version(self):
        repo = Path(__file__).resolve().parent.parent
        commit = run("git", "-C", str(repo), "rev-parse", "--short", "HEAD", check=False)
        return f"{commit} (Soloist {run(str(SOLOIST), '--version', check=False).split(' build')[0]})"


def stop_all(pids_fn):
    for _ in range(50):
        if not pids_fn():
            return
        time.sleep(0.2)
    for p in pids_fn():
        try:
            os.kill(p, signal.SIGKILL)
        except OSError:
            pass
    time.sleep(1)


# ---- the window logger, the sink, Easy Effects -------------------------------------------

def window_mapped_ms(cls, since_ms, timeout=60):
    end = time.time() + timeout
    since = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(since_ms / 1000 - 1))
    while time.time() < end:
        out = run("journalctl", "--user", "-u", "plasma-kwin_wayland.service", "--since", since,
                  "-o", "cat", "--no-pager", check=False)
        for line in out.splitlines():
            m = re.search(r"BENCHWIN (\d+) (\S*)", line)
            if m and int(m[1]) >= since_ms and cls in m[2].lower():
                return int(m[1]) - since_ms
        time.sleep(0.1)
    raise RuntimeError(f"no window of class {cls} within {timeout} s")


def stream_ids():
    return {i["index"] for i in json.loads(run("pactl", "-f", "json", "list", "sink-inputs"))}


def routing(client, before):
    """Where the client's streams that appeared since `before` play: "silent" (all into the
    bench sink), "elsewhere" (any into another sink: audible), or "none" yet. A stream is the
    client's if one of its processes made it, or if it's named "Spotify" (Spotify's client and
    Soloist both name theirs so), which errs on the side of stopping."""
    sinks = {s["index"]: s["name"] for s in json.loads(run("pactl", "-f", "json", "list", "sinks"))}
    mine = set(map(str, client.pids()))
    new = [i for i in json.loads(run("pactl", "-f", "json", "list", "sink-inputs"))
           if i["index"] not in before]
    ours = [i for i in new if i["properties"].get("application.process.id") in mine
            or i["properties"].get("application.name") == "Spotify"]
    for i in new:
        if i not in ours and i["index"] not in routing.seen:
            routing.seen.add(i["index"])
            log("    (another app's new stream, not counted:",
                i["properties"].get("application.name"), "->", sinks.get(i["sink"]), ")")
    wrong = [i for i in ours if sinks.get(i["sink"]) != SINK]
    for i in wrong:
        p = i["properties"]
        log("    stream elsewhere:", p.get("application.name"), p.get("application.process.binary"),
            "->", sinks.get(i["sink"]))
    if wrong:
        return "elsewhere"
    return "silent" if ours else "none"


routing.seen = set()


def pause_and_wait(client):
    """Pause, and wait until the account says so: a client closed while the account still
    plays hands the song to the next device that starts."""
    client.pause()
    for _ in range(50):
        if run("playerctl", "-p", client.mpris, "status", check=False) != "Playing":
            return
        time.sleep(0.1)
        client.pause()


def playing_elsewhere():
    """Whether the account is already playing somewhere (both clients show a remote device's
    playback over MPRIS): the benchmark would take it over, and idle wouldn't be idle."""
    return any(run("playerctl", "-p", p, "status", check=False) == "Playing"
               for p in ("loudauto", "spotify"))


class Machine:
    """What the benchmark changes, and puts back."""

    def __enter__(self):
        self.script = Path(tempfile.mkstemp(suffix=".js")[1])
        self.script.write_text(KWIN_SCRIPT)
        run("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript",
            str(self.script), "loudauto-bench")
        run("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.start")
        self.module = run("pactl", "load-module", "module-null-sink", f"sink_name={SINK}",
                          "rate=44100", "channels=2", "sink_properties=device.description=LOUDAUTO-bench")
        self.default = run("pactl", "get-default-sink")
        run("pactl", "set-default-sink", SINK)
        self.ee_unit = None
        if run("pgrep", "-x", "easyeffects", check=False):
            units = run("systemctl", "--user", "list-units", "--plain", "--no-legend", "app-*easyeffects*",
                        check=False).split()
            self.ee_unit = units[0] if units else ""
            run("easyeffects", "-q", check=False)
            stop_all(lambda: pids_where(lambda e: e.endswith("/easyeffects")))
        self.settings = SETTINGS.read_text() if SETTINGS.exists() else None
        s = json.loads(self.settings or "{}")
        s["output_device"] = SINK
        SETTINGS.parent.mkdir(parents=True, exist_ok=True)
        SETTINGS.write_text(json.dumps(s, indent=2))
        return self

    def __exit__(self, *exc):
        if self.settings is not None:
            SETTINGS.write_text(self.settings)
        run("pactl", "set-default-sink", self.default, check=False)
        run("pactl", "unload-module", self.module, check=False)
        if self.ee_unit:
            subprocess.run(["systemctl", "--user", "start", self.ee_unit], check=False)
        elif self.ee_unit == "":
            subprocess.Popen(["easyeffects", "--gapplication-service"], start_new_session=True,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        run("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript",
            "loudauto-bench", check=False)
        # Nothing is reopened: a player started now could pick up whatever the account last
        # played and play it out loud.
        self.script.unlink(missing_ok=True)


# ---- one run -----------------------------------------------------------------------------

class Busy(RuntimeError):
    pass


def one_run(client, track, play):
    client.quit()
    time.sleep(2)
    t0 = int(time.time() * 1000)
    client.launch()
    played = False
    try:
        r = {"startup_ms": window_mapped_ms(client.window_class, t0)}
        time.sleep(20)
        if playing_elsewhere():
            raise Busy("Spotify is playing on another device; run the benchmark when it isn't")
        pids = client.pids()
        r |= {"idle_pss_mb": pss_mb(pids), "processes": len(pids), "threads": threads(pids)}
        r["idle_cpu"] = cpu_percent(client, 30)
        if play:
            if playing_elsewhere():
                raise Busy("Spotify started playing on another device; stopped before playing")
            before = stream_ids()
            played = True
            client.play(track)
            end = time.time() + 15
            where = routing(client, before)
            while where == "none" and time.time() < end:
                time.sleep(0.05)
                where = routing(client, before)
            if where != "silent":
                raise RuntimeError(f"{client.name}'s audio went {where}, not to the silent sink; stopped")
            time.sleep(10)
            r["play_cpu"] = cpu_percent(client, 60)
            r["play_pss_mb"] = pss_mb(client.pids())
            if routing(client, before) != "silent":
                raise RuntimeError(f"{client.name} stopped playing or moved its audio; stopped")
        return r
    finally:
        if played:
            pause_and_wait(client)
        client.quit()


def disk_mb(paths):
    total = 0
    for p in paths:
        p = Path(p)
        if p.is_file():
            total += p.stat().st_size
        elif p.is_dir():
            total += sum(f.stat().st_size for f in p.rglob("*") if f.is_file() and not f.is_symlink())
    return total / 1e6


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--track", required=True)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--play-runs", type=int, default=2)
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()
    clients = [Loudauto(), Spotify()]
    results = {c.name: [] for c in clients}
    with Machine():
        for n in range(a.runs + 1):  # run 0 warms the disk cache and isn't kept
            for c in clients:
                play = 1 <= n <= a.play_runs
                log(f"run {n} {c.name}{' (playing)' if play else ''}")
                r = one_run(c, a.track, play)
                log("   ", json.dumps({k: round(v, 1) for k, v in r.items()}))
                if n:
                    results[c.name].append(r)
    loud, spot = clients
    summary = {
        "date": time.strftime("%Y-%m-%d"),
        "machine": {
            "cpu": re.search(r"model name\s*:\s*(.*)", open("/proc/cpuinfo").read())[1],
            "memory_gb": round(os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE") / 2**30),
            "kernel": platform.release(),
            "plasma": run("plasmashell", "--version", check=False),
            "pipewire": run("pipewire", "--version", check=False).splitlines()[-1],
        },
        "track": a.track,
        "versions": {loud.name: loud.version(), spot.name: spot.version()},
        "disk_mb": {
            loud.name: disk_mb([BIN / "loudauto", BIN / "loudautod", BIN / "loudauto-cli", SOLOIST.parent]),
            spot.name: disk_mb([SPOTIFY_DIR]),
        },
        "runs": results,
        "median": {
            name: {k: median(r[k] for r in rs if k in r) for k in rs[0] | rs[-1]}
            for name, rs in results.items()
        },
    }
    text = json.dumps(summary, indent=2)
    print(text)
    if a.out:
        a.out.parent.mkdir(parents=True, exist_ok=True)
        a.out.write_text(text + "\n")


if __name__ == "__main__":
    sys.exit(main())
