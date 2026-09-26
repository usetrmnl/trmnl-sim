"""Client for the trmnl-sim control API, for integration tests and scripts.

Standard library only. Typical use:

    from trmnl_sim import Simulator

    with Simulator("../trmnl-firmware/.pio/build/trmnl", erase=True) as sim:
        sim.wait(portal=True, timeout_s=60)               # device is in WiFi setup mode
        sim.wait(display_idle=True, min_refreshes=1)
        sim.assert_screen("golden/setup.png")
        sim.portal_connect("TRMNL-Sim", "secret")
        sim.wait(wifi_connected=True)
        sim.press(1200)                                   # 1.2 s button hold (virtual time)
        sim.wait_for_console(r"deep sleep")

Every GUI action has a method here; see `Simulator` for the full list.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Optional

REPO = Path(__file__).resolve().parent.parent


class SimError(RuntimeError):
    pass


class Simulator:
    """Launches trmnl-sim (headless unless `gui=True`) and drives it over HTTP.

    Args:
        build_dir: PlatformIO build dir containing firmware.elf etc.
        flash: flash image path; defaults to a fresh temp file (deleted on close).
        erase: start from erased flash (factory reset). Implied for a new temp flash.
        mac: device MAC, e.g. "D8:3B:DA:F5:28:3C" (the server identity).
        turbo: run unthrottled (virtual time faster than wall time when idle).
        gui: show the window too.
        binary: path to the trmnl-sim executable (default: $TRMNL_SIM_BIN or target/release/trmnl-sim).
        extra_args: more CLI arguments.
        name: label for artifacts. If $TRMNL_SIM_ARTIFACTS is set, the log and final
            screen of every simulator are saved there on close (handy in CI).
    """

    def __init__(
        self,
        build_dir: str | os.PathLike,
        *,
        flash: Optional[str | os.PathLike] = None,
        erase: bool = False,
        mac: Optional[str] = None,
        turbo: bool = False,
        gui: bool = False,
        binary: Optional[str | os.PathLike] = None,
        extra_args: tuple[str, ...] = (),
        startup_timeout_s: float = 30,
        name: Optional[str] = None,
    ):
        self.build_dir = Path(build_dir)
        self._tmpdir = tempfile.mkdtemp(prefix="trmnl-sim-")
        self.flash = Path(flash) if flash else Path(self._tmpdir) / "flash.bin"
        self.log_path = Path(self._tmpdir) / "sim.log"
        self.cursor = 0  # console line index after the last matched line
        self.name = name or "sim"
        binary = Path(binary or os.environ.get("TRMNL_SIM_BIN") or REPO / "target" / "release" / "trmnl-sim")
        if not binary.exists():
            raise SimError(f"{binary} not found; run `cargo build --release` in {REPO}")
        args = [
            str(binary),
            str(self.build_dir),
            "--control", "127.0.0.1:0",
            "--portal-port", "0",
            "--flash", str(self.flash),
        ]
        if not gui:
            args.append("--headless")
        if erase or not flash:
            args.append("--erase")
        if mac:
            args += ["--mac", mac]
        if turbo:
            args.append("--turbo")
        args += list(extra_args)
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(args, stdout=self._log, stderr=subprocess.STDOUT)
        self.base = self._discover_url(startup_timeout_s)

    # ---- lifecycle ------------------------------------------------------------------------

    def _discover_url(self, timeout_s: float) -> str:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SimError(f"simulator exited early:\n{self.log()}")
            m = re.search(r"control API on (http://[\d.:]+/)", self.log())
            if m:
                return m.group(1).rstrip("/")
            time.sleep(0.05)
        raise SimError("simulator did not start its control API")

    def log(self) -> str:
        """Everything the simulator printed (serial output, [sim] messages)."""
        return self.log_path.read_text(errors="replace")

    def close(self) -> None:
        artifacts = os.environ.get("TRMNL_SIM_ARTIFACTS")
        if artifacts:
            out = Path(artifacts)
            out.mkdir(parents=True, exist_ok=True)
            stem = re.sub(r"[^\w.-]+", "_", self.name)
            n = 0
            while (out / f"{stem}-{n}.log").exists():
                n += 1
            if self.proc.poll() is None:
                try:
                    self.screenshot(out / f"{stem}-{n}.png")
                except Exception:
                    pass
            shutil.copy(self.log_path, out / f"{stem}-{n}.log")
        if self.proc.poll() is None:
            try:
                self._post("/quit")
                self.proc.wait(timeout=10)
            except Exception:
                self.proc.kill()
        self._log.close()
        shutil.rmtree(self._tmpdir, ignore_errors=True)

    def __enter__(self) -> "Simulator":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # ---- HTTP -------------------------------------------------------------------------------

    def _request(self, method: str, path: str, body: Any = None, raw: bytes | None = None, timeout: float = 30):
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        req = urllib.request.Request(self.base + path, data=data, method=method)
        if body is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, r.headers.get("Content-Type", ""), r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.headers.get("Content-Type", ""), e.read()

    def _post(self, path: str, body: Any = None, timeout: float = 30) -> dict:
        code, _, data = self._request("POST", path, body if body is not None else {}, timeout=timeout)
        out = json.loads(data or b"{}")
        if code != 200:
            raise SimError(f"POST {path} -> {code}: {out.get('error', out)}")
        return out

    def _get_json(self, path: str) -> dict:
        code, _, data = self._request("GET", path)
        if code != 200:
            raise SimError(f"GET {path} -> {code}: {data[:200]!r}")
        return json.loads(data)

    # ---- UI actions ---------------------------------------------------------------------------

    def status(self) -> dict:
        return self._get_json("/status")

    def button(self, down: bool) -> None:
        """Hold (True) or release (False) the physical button."""
        self._post("/button", {"down": down})

    def press(self, ms: int = 100) -> None:
        """Press and hold the button for `ms` of virtual time, then release."""
        self._post("/press", {"ms": ms}, timeout=ms / 1000 * 20 + 30)

    def double_click(self, ms: int = 80, gap_ms: int = 150) -> None:
        self.press(ms)
        time.sleep(gap_ms / 1000)
        self.press(ms)

    def touch(self, zone: str, ms: int = 120) -> None:
        """Tap the touch bar ("left", "center" or "right") for `ms` of virtual time (TRMNL X)."""
        self._post("/touch", {"zone": zone, "ms": ms}, timeout=ms / 1000 * 20 + 30)

    def dock(self, docked: bool = True) -> None:
        """Put the device on / take it off its magnetic dock (TRMNL X). Returns once applied."""
        self._post("/dock", {"docked": docked})
        deadline = time.time() + 10
        while self.status()["docked"] != docked:
            if time.time() > deadline:
                raise SimError("dock state did not change (is the simulator paused?)")
            time.sleep(0.02)

    def debug(self) -> list[str]:
        """Dump CPU state and board diagnostics; returns the dumped lines."""
        c = self.status()["console_total"]
        self._post("/debug")
        time.sleep(0.3)
        return self.console(c)

    def reset(self) -> None:
        self._post("/reset")

    def power_cycle(self) -> None:
        self._post("/power-cycle")

    def wake(self) -> None:
        """End a deep sleep now, as if its timer expired."""
        self._post("/wake")

    def set_wifi(self, available: bool) -> None:
        self._post("/wifi", {"available": available})

    def set_battery(self, mv: int) -> None:
        self._post("/battery", {"mv": mv})

    def set_turbo(self, on: bool = True) -> None:
        self._post("/turbo", {"on": on})

    def pause(self, on: bool = True) -> None:
        self._post("/pause", {"on": on})

    # ---- observation ------------------------------------------------------------------------------

    def console(self, since: int = 0) -> list[str]:
        return [l["text"] for l in self._get_json(f"/console?since={since}")["lines"]]

    def wait(self, *, timeout_s: float = 60, **cond) -> dict:
        """Block until all conditions hold. Conditions: console (regex, searched from
        `since`, default = self.cursor), state ("running", "deep_sleep", "light_sleep",
        "halted", ...), min_refreshes, min_boots, display_idle, wifi_connected,
        portal (captive portal up), settle_ms."""
        if "console" in cond:
            cond.setdefault("since", self.cursor)
        code, _, data = self._request("POST", "/wait", {**cond, "timeout_s": timeout_s}, timeout=timeout_s + 30)
        out = json.loads(data)
        if code != 200:
            tail = "\n".join(self.log().splitlines()[-40:])
            raise SimError(f"wait({cond}) failed: {out.get('error')}\nstatus: {out.get('status')}\n--- log tail ---\n{tail}")
        line = out.get("line")
        if isinstance(line, dict):
            self.cursor = line["i"] + 1
        return out

    def wait_for_console(self, regex: str, timeout_s: float = 60) -> str:
        return self.wait(console=regex, timeout_s=timeout_s)["line"]["text"]

    def wait_for_refresh(self, timeout_s: float = 90) -> dict:
        """Wait for the next completed display refresh (BUSY released)."""
        n = self.status()["display_refreshes"]
        return self.wait(min_refreshes=n + 1, display_idle=True, settle_ms=300, timeout_s=timeout_s)

    def screenshot(self, path: Optional[str | os.PathLike] = None, region: Optional[tuple[int, int, int, int]] = None) -> bytes:
        """PNG of the e-paper (8-bit gray, 0 = ink, 255 = paper); optionally cropped to (x, y, w, h)."""
        q = "?x={}&y={}&w={}&h={}".format(*region) if region else ""
        code, _, data = self._request("GET", "/screenshot" + q)
        if code != 200:
            raise SimError(f"screenshot failed: {data[:200]!r}")
        if path:
            Path(path).write_bytes(data)
        return data

    def compare_screen(
        self,
        reference_png: str | os.PathLike | bytes,
        region: Optional[tuple[int, int, int, int]] = None,
        tolerance: int = 48,
        max_ratio: float = 0.001,
    ) -> dict:
        """Compare the screen (or region) to a reference PNG. Returns match/diff stats."""
        ref = reference_png if isinstance(reference_png, bytes) else Path(reference_png).read_bytes()
        q = f"?tolerance={tolerance}&max_ratio={max_ratio}"
        if region:
            q += "&x={}&y={}&w={}&h={}".format(*region)
        code, _, data = self._request("POST", "/screenshot/compare" + q, raw=ref)
        out = json.loads(data)
        if code != 200:
            raise SimError(f"compare failed: {out.get('error')}")
        return out

    def assert_screen(self, golden: str | os.PathLike, region=None, **kw) -> None:
        """Assert the screen matches a golden PNG. With TRMNL_SIM_UPDATE_GOLDEN=1 (or when
        the golden is missing) the current screen is written as the new golden instead."""
        golden = Path(golden)
        if os.environ.get("TRMNL_SIM_UPDATE_GOLDEN") == "1" or not golden.exists():
            golden.parent.mkdir(parents=True, exist_ok=True)
            self.screenshot(golden, region)
            return
        r = self.compare_screen(golden, region, **kw)
        if not r["match"]:
            actual = golden.with_suffix(".actual.png")
            self.screenshot(actual, region)
            raise AssertionError(f"screen differs from {golden}: {r['diff_pixels']} px ({r['diff_ratio']:.4%}); actual saved to {actual}")

    # ---- captive portal ---------------------------------------------------------------------------

    def portal_url(self, timeout_s: float = 60) -> str:
        return self.wait(portal=True, timeout_s=timeout_s)["status"]["portal_url"].rstrip("/")

    def portal_request(self, path: str, body: Any = None, timeout: float = 30, retry_s: float = 30) -> tuple[int, bytes]:
        """HTTP request to the device's own web server (while in setup mode). Connection
        failures are retried for `retry_s`: the device may still be (re)starting its AP."""
        data = json.dumps(body).encode() if body is not None else None
        deadline = time.time() + retry_s
        while True:
            req = urllib.request.Request(self.portal_url() + path, data=data, method="POST" if data else "GET")
            if data:
                req.add_header("Content-Type", "application/json")
            try:
                with urllib.request.urlopen(req, timeout=timeout) as r:
                    return r.status, r.read()
            except urllib.error.HTTPError as e:
                return e.code, e.read()
            except (ConnectionError, urllib.error.URLError, TimeoutError):
                if time.time() > deadline:
                    raise
                time.sleep(0.5)

    def portal_scan(self) -> dict:
        code, data = self.portal_request("/scan")
        if code != 200:
            raise SimError(f"/scan -> {code}")
        return json.loads(data)

    def portal_connect(self, ssid: str, password: str = "", server: str = "https://trmnl.app") -> dict:
        """Submit WiFi credentials through the setup page, like a phone would."""
        code, data = self.portal_request("/connect", {"ssid": ssid, "pswd": password, "server": server})
        if code != 200:
            raise SimError(f"/connect -> {code}: {data[:200]!r}")
        return json.loads(data)
