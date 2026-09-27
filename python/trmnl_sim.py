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
        sim.set_net_faults(dns="servfail")                # inject faults (see set_faults)

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
import urllib.parse
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
        faults: faults injected from the start (as for `set_faults`), e.g.
            {"power_loss": {"partition": "nvs"}}.
        memcheck: run with the memory checker: "halt" (stop at the first violation) or
            "log" (report and carry on). Leaving the `with` block then fails if there
            were violations; `memcheck()` returns the full report.
        memcheck_suppress: functions whose (known) memory bugs to tolerate: a violation
            with one of them in its backtrace, allocation or free stack is only counted.
        name: label for artifacts. If $TRMNL_SIM_ARTIFACTS is set, the log and final
            screen of every simulator are saved there on close (handy in CI).
        restore: start from this save point file (see `save_point`) instead of booting.
        host_ports: {guest port: host port}; the device's connections to 10.0.2.2:guest
            port go to host port instead (`--host-port`), e.g. for a device onboarded
            against a server that has since moved to another port.
        coverage: record firmware code coverage and write an lcov tracefile here when the
            simulator exits (`--coverage`). If $TRMNL_SIM_COVERAGE is set to a directory,
            every simulator writes one there (`<name>-*.info`); merge them with
            scripts/coverage.py.
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
        faults: Optional[dict] = None,
        networks: Optional[list[dict]] = None,
        startup_timeout_s: float = 30,
        name: Optional[str] = None,
        restore: Optional[str | os.PathLike] = None,
        host_ports: Optional[dict[int, int]] = None,
        coverage: Optional[str | os.PathLike] = None,
        memcheck: Optional[str] = None,
        memcheck_suppress: tuple[str, ...] = (),
    ):
        self.build_dir = Path(build_dir)
        self._tmpdir = tempfile.mkdtemp(prefix="trmnl-sim-")
        self.flash = Path(flash) if flash else Path(self._tmpdir) / "flash.bin"
        self.log_path = Path(self._tmpdir) / "sim.log"
        self.cursor = 0  # console line index after the last matched line
        self.name = name or "sim"
        self.memcheck_mode = memcheck
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
        if restore:
            args += ["--restore", str(Path(restore).resolve())]
        for guest, host in (host_ports or {}).items():
            args += ["--host-port", f"{guest}={host}"]
        cov_dir = os.environ.get("TRMNL_SIM_COVERAGE")
        if coverage is None and cov_dir:
            Path(cov_dir).mkdir(parents=True, exist_ok=True)
            stem = re.sub(r"[^\w.-]+", "_", self.name)
            fd, coverage = tempfile.mkstemp(prefix=f"{stem}-", suffix=".info", dir=cov_dir)
            os.close(fd)
        self.coverage_path = Path(coverage) if coverage else None
        if self.coverage_path:
            args += ["--coverage", str(self.coverage_path)]
        if faults:
            args += ["--faults", json.dumps(faults)]
        if networks is not None:
            args += ["--wifi-networks", json.dumps(networks)]
        if memcheck:
            args.append(f"--memcheck={memcheck}")
            if memcheck_suppress:
                args += ["--memcheck-suppress", ",".join(memcheck_suppress)]
        args += list(extra_args)
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(args, stdout=self._log, stderr=subprocess.STDOUT)
        try:
            self.base = self._discover_url(startup_timeout_s)
        except BaseException:
            self.proc.kill()
            self._log.close()
            raise

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
                # Writing the coverage report reads the ELF's line tables first.
                self.proc.wait(timeout=60 if self.coverage_path else 10)
            except Exception:
                self.proc.kill()
        self._log.close()
        shutil.rmtree(self._tmpdir, ignore_errors=True)

    def __enter__(self) -> "Simulator":
        return self

    def __exit__(self, exc_type, *exc) -> None:
        try:
            if exc_type is None and self.memcheck_mode and self.proc.poll() is None:
                self.assert_no_memory_errors()
        finally:
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

    def _delete(self, path: str) -> dict:
        code, _, data = self._request("DELETE", path)
        out = json.loads(data or b"{}")
        if code != 200:
            raise SimError(f"DELETE {path} -> {code}: {out.get('error', out)}")
        return out

    def _get_json(self, path: str) -> dict:
        code, _, data = self._request("GET", path)
        if code != 200:
            raise SimError(f"GET {path} -> {code}: {data[:200]!r}")
        return json.loads(data)

    # ---- coverage -------------------------------------------------------------------------------

    def write_coverage(self, path: Optional[str | os.PathLike] = None, reset: bool = False) -> dict:
        """Write the firmware code coverage so far as an lcov tracefile (default: the
        `coverage` path) and return its totals (`lines_found`, `lines_hit`,
        `functions_found`, `functions_hit`, `files`, `path`). `reset` starts over
        afterwards. Needs `coverage=` (or $TRMNL_SIM_COVERAGE)."""
        body: dict[str, Any] = {"reset": reset}
        if path is not None:
            body["path"] = str(Path(path).resolve())
        return self._post("/coverage", body, timeout=120)

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
        """Two presses of `ms`, `gap_ms` apart, timed in virtual time."""
        self._post("/press", {"ms": ms, "count": 2, "gap_ms": gap_ms}, timeout=(ms + gap_ms) * 2 / 1000 * 20 + 30)

    def touch(self, zone: str, ms: int = 120) -> None:
        """Tap the touch bar ("left", "center" or "right") for `ms` of virtual time (TRMNL X)."""
        self._post("/touch", {"zone": zone, "ms": ms}, timeout=ms / 1000 * 20 + 30)

    def touch_down(self, zone: str) -> None:
        """Put a finger on a touch bar zone and keep it there (several may be down)."""
        self._post("/touch", {"zone": zone, "down": True})

    def touch_up(self, zone: str) -> None:
        self._post("/touch", {"zone": zone, "down": False})

    def gesture(self, name: str) -> None:
        """A slide along the touch bar: "swipe_next", "swipe_back", "flick_next" or
        "flick_back" (only reported in slide mode)."""
        self._post("/gesture", {"gesture": name})

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

    def set_networks(self, networks: list[dict]) -> None:
        """Replace the access points in range: dicts with "ssid" and optionally "password"
        (None: any), "rssi", "channel", "open", "internet" (see --wifi-networks)."""
        self._post("/wifi", {"networks": networks})

    def set_portal_client(self, on: bool) -> None:
        """Whether the host's portal client joins the setup access point (the default). It
        keeps the simulation at wall-clock pace, even in turbo mode; without it an
        unattended portal runs ahead (e.g. to its 15-minute timeout)."""
        self._post("/wifi", {"portal_client": on})

    def set_battery(self, mv: int) -> None:
        self._post("/battery", {"mv": mv})

    def set_turbo(self, on: bool = True) -> None:
        self._post("/turbo", {"on": on})

    def pause(self, on: bool = True) -> None:
        self._post("/pause", {"on": on})

    # ---- save points -------------------------------------------------------------------------------

    def save_point(self, path: Optional[str | os.PathLike] = None, label: Optional[str] = None) -> dict:
        """Take a save point: in deep sleep the full device state, otherwise only what survives
        a battery pull (flash, screen). Kept in memory (see `save_points`) and, with `path`,
        written to that file for `restore` or `Simulator(restore=...)`. Returns its info
        (`id`, `label`, `deep_sleep`, `sim_time_s`, `wake_at_s`, `path`, `bytes`)."""
        body: dict[str, Any] = {}
        if path is not None:
            body["path"] = str(Path(path).resolve())
        if label is not None:
            body["label"] = label
        return self._post("/savepoint", body, timeout=150)["savepoint"]

    def restore(self, path: Optional[str | os.PathLike] = None, id: Optional[int] = None) -> dict:
        """Replace the device with a save point from a file or an in-memory slot `id`."""
        body = {"path": str(Path(path).resolve())} if path is not None else {"id": id}
        return self._post("/restore", body, timeout=150)["savepoint"]

    def save_points(self) -> list[dict]:
        """The in-memory save points, oldest first."""
        return self._get_json("/savepoints")["savepoints"]

    # ---- faults -------------------------------------------------------------------------------------

    def faults(self) -> dict:
        """Current faults: {"faults", "summary", "power_losses", "flash": {"programs", "erases"},
        "partitions": [{"label", "type", "subtype", "offset", "size"}]}."""
        return self._get_json("/faults")

    def set_faults(self, faults: Optional[dict] = None, **kw) -> dict:
        """Merge faults into the current ones (keys left out are kept, None clears one):

            net: {latency_ms, loss (0..1), bandwidth_bps, dns ("servfail" | "nxdomain" |
                  "empty" | "timeout"), no_internet, offline,
                  tcp_cut: {after_bytes, stall, port}}
            power_loss: {op ("any" | "program" | "erase"), partition ("nvs", "otadata",
                         "ota_0", "spiffs", ...), range: [start, end], nth, cut
                         ("before" | "torn" | "after")}
            i2c_absent: [0x55, ...]
            panel_busy_stuck: bool
            modem_unresponsive: bool
            modem_at_errors: ["AT+CWMODE", ...] (TRMNL X: answer ERROR to these commands)
            touch_bar: "reset" | "lockup" | "ati_error" (TRMNL X IQS323)
            gauge_reset: True (TRMNL X BQ27427 power-on reset, once, when set)
        """
        return self._post("/faults", {**(faults or {}), **kw})

    def set_net_faults(self, **kw) -> dict:
        """Shortcut for set_faults(net={...}), e.g. set_net_faults(latency_ms=300, loss=0.1)."""
        return self.set_faults(net=kw)

    def arm_power_loss(self, partition: Optional[str] = None, *, op: str = "any", nth: int = 1,
                       cut: str = "before", range: Optional[tuple[int, int]] = None) -> dict:
        """Cut power at the `nth` flash `op` into `partition` / `range` (one-shot). With
        cut="torn" the interrupted program/erase is left half done."""
        spec: dict = {"op": op, "nth": nth, "cut": cut}
        if partition:
            spec["partition"] = partition
        if range:
            spec["range"] = list(range)
        return self.set_faults(power_loss=spec)

    def clear_faults(self) -> dict:
        return self._delete("/faults")

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

    # ---- memory checking ---------------------------------------------------------------------------

    def memcheck(self) -> dict:
        """The --memcheck report: `violations` (each with `kind`, `address`, `backtrace`,
        `report` lines, and for heap errors the block's allocation and free stacks),
        `suppressed` (the same, for violations matching --memcheck-suppress), `heap`
        statistics (live and peak bytes per memory) and `stacks` (per-task high-water marks,
        `low` if within `stack_margin` bytes of overflowing). `{"enabled": False}` without
        --memcheck."""
        return self._get_json("/memcheck")

    def assert_no_memory_errors(self) -> None:
        """Fail with the full reports if memcheck found violations."""
        report = self.memcheck()
        violations = report.get("violations", [])
        if violations:
            text = "\n".join(l for v in violations for l in v["report"])
            raise AssertionError(f"memcheck found {len(violations)} violation(s):\n{text}")

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

    # ---- built-in mock server ----------------------------------------------------------------------

    @property
    def mock(self) -> "BuiltinServer":
        """The simulator's built-in mock TRMNL server (an alternative to trmnl_mock.MockTrmnl)."""
        return BuiltinServer(self)


class BuiltinServer:
    """Drives the simulator's built-in mock TRMNL server over the control API (`/mock/...`).
    Images you add are converted for the simulated panel; `expected(name)` is the PNG a
    screenshot should then match.

        url = sim.mock.start()
        sim.mock.add_image("hello", png_bytes, current=True)
        sim.mock.display(refresh_rate=600)
        sim.portal_connect("TRMNL-Sim", "pw", server=url)
        req = sim.mock.wait_for_request("/api/display")
        assert sim.compare_screen(sim.mock.expected("hello"))["match"]
    """

    def __init__(self, sim: Simulator):
        self.sim = sim

    def state(self) -> dict:
        return self.sim._get_json("/mock")

    def start(self, port: int = 0) -> str:
        """Start listening (0 = any free port); returns the device URL (http://10.0.2.2:PORT)."""
        return self.sim._post("/mock/start", {"port": port})["device_url"]

    def stop(self) -> None:
        self.sim._post("/mock/stop")

    @property
    def device_url(self) -> Optional[str]:
        return self.state()["device_url"]

    def add_image(self, name: str, data: bytes, *, current: bool = False, dither: bool = True,
                  fit: str = "contain", raw: bool = False) -> dict:
        """Add (or replace) an image from PNG/JPEG/BMP/GIF bytes, converted for the panel
        (raw=True serves a PNG/BMP unchanged). Names: letters, digits, - and _."""
        q = urllib.parse.urlencode({"name": name, "current": int(current), "dither": int(dither), "fit": fit,
                                    "raw": int(raw)})
        code, _, body = self.sim._request("POST", "/mock/images?" + q, raw=data)
        out = json.loads(body)
        if code != 200:
            raise SimError(f"add_image({name}) -> {code}: {out.get('error', out)}")
        return out

    def expected(self, name: str) -> bytes:
        """The PNG the screen should show for image `name`."""
        code, _, data = self.sim._request("GET", f"/mock/images/{name}/expected")
        if code != 200:
            raise SimError(f"expected({name}) -> {code}: {data[:200]!r}")
        return data

    def remove_image(self, name: str) -> None:
        self.sim._request("DELETE", f"/mock/images/{name}")

    def display(self, **fields) -> dict:
        """Change the /api/display answer: image, refresh_rate, special_function, playlist,
        auto_advance, registered, friendly_id, api_key, extra (raw fields; None removes)."""
        return self.sim._post("/mock/display", fields)

    def queue(self, **fields) -> None:
        """Raw /api/display fields for the next answer only, e.g. update_firmware=True,
        firmware_url=..., or reset_firmware=True (plus image=NAME)."""
        self.sim._post("/mock/queue", fields)

    def set_file(self, path: str, data: bytes) -> str:
        """Serve bytes at `path` (e.g. a firmware.bin for OTA); returns the device URL."""
        code, _, body = self.sim._request("POST", "/mock/files?" + urllib.parse.urlencode({"path": path}), raw=data)
        if code != 200:
            raise SimError(f"set_file -> {code}: {body[:200]!r}")
        return json.loads(body)["url"]

    def requests(self, since: int = 0) -> list:
        """Recorded requests (dicts: i, method, path, headers (case-insensitive), body,
        status, summary, sim_time_s)."""
        from trmnl_mock import Headers

        reqs = self.sim._get_json(f"/mock/requests?since={since}")["requests"]
        for r in reqs:
            r["headers"] = Headers(r["headers"].items())
        return reqs

    def count(self, path: str) -> int:
        return sum(1 for r in self.requests() if r["path"] == path)

    def wait_for_request(self, path: str, after: int = 0, timeout_s: float = 60) -> dict:
        """Wait for a request to `path` with index >= `after` (use `state()["total_requests"]`
        as a cursor)."""
        deadline = time.time() + timeout_s
        while True:
            for r in self.requests(after):
                if r["path"] == path:
                    return r
            if time.time() > deadline:
                seen = [r["path"] for r in self.requests()]
                raise TimeoutError(f"no request to {path} within {timeout_s}s (seen: {seen})")
            time.sleep(0.1)
