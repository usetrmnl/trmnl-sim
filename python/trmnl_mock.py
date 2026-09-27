"""A mock TRMNL API server for hermetic integration tests (standard library only).

The simulated device reaches the host as 10.0.2.2, so point the device at
`server.device_url` (e.g. via `Simulator.portal_connect(..., server=mock.device_url)`).

    mock = MockTrmnl()
    mock.set_image("hello", checkerboard(40))
    mock.display = {"image": "hello", "refresh_rate": 300}
    ...
    req = mock.wait_for_request("/api/display")
    assert req.headers["Battery-Voltage"].startswith("4.")

HTTP-level faults are per path (exact, or a prefix ending in "*"):

    mock.set_fault("/api/display", status=500)           # every /api/display answers 500
    mock.set_fault("/images/*", truncate=1000, times=1)  # the next image stops after 1000 bytes
    mock.clear_faults()

`MockTrmnl(tls=True)` serves HTTPS instead (a throwaway self-signed certificate made
with the `openssl` command line tool), to exercise the device's TLS stack.
"""

from __future__ import annotations

import json
import os
import shutil
import ssl
import struct
import subprocess
import tempfile
import threading
import time
import zlib
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Callable, Optional

WIDTH, HEIGHT = 800, 480


# ---- images ------------------------------------------------------------------------------------------


def bmp_1bit(pixel: Callable[[int, int], bool]) -> bytes:
    """800x480 1-bit BMP in the format TRMNL serves. `pixel(x, y)` returns True for black."""
    row = WIDTH // 8  # 100 bytes, already 4-byte aligned
    data = bytearray(row * HEIGHT)
    for y in range(HEIGHT):
        base = (HEIGHT - 1 - y) * row  # bottom-up
        for x in range(WIDTH):
            if not pixel(x, y):  # palette index 1 = white
                data[base + x // 8] |= 0x80 >> (x % 8)
    palette = bytes([0, 0, 0, 0, 255, 255, 255, 0])
    offset = 14 + 40 + len(palette)
    header = b"BM" + struct.pack("<IHHI", offset + len(data), 0, 0, offset)
    info = struct.pack("<IiiHHIIiiII", 40, WIDTH, HEIGHT, 1, 1, 0, len(data), 2835, 2835, 2, 2)
    return header + info + palette + bytes(data)


def png_gray(pixel: Callable[[int, int], bool]) -> bytes:
    """The PNG the simulator's screenshot of that image should match (0 = ink, 255 = paper)."""
    raw = b"".join(b"\x00" + bytes(0 if pixel(x, y) else 255 for x in range(WIDTH)) for y in range(HEIGHT))

    def chunk(tag: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 0, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def png_image(level: Callable[[int, int], int], width: int, height: int, bits: int = 1) -> bytes:
    """A grayscale PNG of `bits` depth (1, 2, 4 or 8) as TRMNL servers produce them.
    `level(x, y)` returns 0 (black) .. 2**bits - 1 (white)."""
    assert bits in (1, 2, 4, 8)
    per_byte = 8 // bits
    rows = []
    for y in range(height):
        row = bytearray((width * bits + 7) // 8)
        for x in range(width):
            v = level(x, y) & ((1 << bits) - 1)
            row[x // per_byte] |= v << (8 - bits * (x % per_byte + 1))
        rows.append(b"\x00" + bytes(row))
    ihdr = struct.pack(">IIBBBBB", width, height, bits, 0, 0, 0, 0)
    return _png(ihdr, b"".join(rows))


def expected_gray(level: Callable[[int, int], int], width: int, height: int, bits: int = 1) -> bytes:
    """The 8-bit PNG a simulator screenshot of `png_image(level, ...)` should match (0 = ink)."""
    top = (1 << bits) - 1
    raw = b"".join(
        b"\x00" + bytes(round(255 * (level(x, y) & top) / top) for x in range(width)) for y in range(height)
    )
    return _png(struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0), raw)


BWRY_RGB = {"black": (0, 0, 0), "white": (255, 255, 255), "yellow": (255, 255, 0), "red": (255, 0, 0)}
SPECTRA6_RGB = {**BWRY_RGB, "blue": (0, 0, 255), "green": (0, 255, 0)}


def png_rgb(color: Callable[[int, int], tuple], width: int = 800, height: int = 480) -> bytes:
    """A truecolor PNG; `color(x, y)` returns (r, g, b)."""
    raw = b"".join(b"\x00" + bytes(c for x in range(width) for c in color(x, y)) for y in range(height))
    return _png(struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0), raw)


def png_rgba(color: Callable[[int, int], tuple], width: int = 800, height: int = 480) -> bytes:
    """A truecolor PNG with an (opaque) alpha channel; `color(x, y)` returns (r, g, b)."""
    raw = b"".join(b"\x00" + bytes(c for x in range(width) for c in (*color(x, y), 255)) for y in range(height))
    return _png(struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0), raw)


def png_palette(color: Callable[[int, int], tuple], palette: list, width: int = 800, height: int = 480,
                bits: Optional[int] = None) -> bytes:
    """An indexed PNG (as TRMNL serves color images), by default 2 bits per pixel for up
    to 4 colors, else 4; `color(x, y)` must return one of the `palette` colors."""
    index = {c: i for i, c in enumerate(palette)}
    bits = bits or (2 if len(palette) <= 4 else 4)
    assert bits in (1, 2, 4, 8) and len(palette) <= 1 << bits
    per_byte = 8 // bits
    rows = []
    for y in range(height):
        row = bytearray((width + per_byte - 1) // per_byte)
        for x in range(width):
            row[x // per_byte] |= index[tuple(color(x, y))] << (8 - bits * (x % per_byte + 1))
        rows.append(b"\x00" + bytes(row))
    plte = b"".join(bytes(c) for c in palette)
    ihdr = struct.pack(">IIBBBBB", width, height, bits, 3, 0, 0, 0)

    def chunk(tag: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"PLTE", plte)
            + chunk(b"IDAT", zlib.compress(b"".join(rows))) + chunk(b"IEND", b""))


def bwry_quantize(r: int, g: int, b: int) -> tuple:
    """The TRMNL BWRY firmware's color reduction (GetBWYRPixel in display.cpp)."""
    gr = (b + r + g * 2) >> 2
    if r > b or g > b:
        if gr < 90 and r < 80 and g < 80:
            return BWRY_RGB["black"]
        if r - b > 32 and r - g > r // 2:
            return BWRY_RGB["red"]
        if r - b > 32 and g - b > 32:
            return BWRY_RGB["yellow"]
        return BWRY_RGB["white"]
    return BWRY_RGB["white"] if gr >= 100 else BWRY_RGB["black"]


def expected_bwry(color: Callable[[int, int], tuple], width: int = 800, height: int = 480) -> bytes:
    """The RGB PNG a simulator screenshot of `png_rgb(color)` on a TRMNL BWRY should match."""
    raw = b"".join(
        b"\x00" + bytes(c for x in range(width) for c in bwry_quantize(*color(x, y))) for y in range(height)
    )
    return _png(struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0), raw)


def spectra6_quantize(r: int, g: int, b: int) -> tuple:
    """The Spectra 6 firmware's color reduction (GetSpectraPixel in display.cpp): the
    nearest of its reference inks to the color's RGB333 value."""
    inks = [(0, 0, 0), (192, 192, 192), (192, 192, 0), (192, 0, 0), (0, 0, 192), (0, 192, 0)]
    r, g, b = (r >> 5) * 36, (g >> 5) * 36, (b >> 5) * 36
    dist = [(r - i[0]) ** 2 + (g - i[1]) ** 2 + (b - i[2]) ** 2 for i in inks]
    return list(SPECTRA6_RGB.values())[dist.index(min(dist))]


def expected_spectra6(color: Callable[[int, int], tuple], width: int = 800, height: int = 480) -> bytes:
    """The RGB PNG a simulator screenshot of an image of `color` on a Spectra 6 panel
    (reTerminal E1002) should match."""
    raw = b"".join(
        b"\x00" + bytes(c for x in range(width) for c in spectra6_quantize(*color(x, y))) for y in range(height)
    )
    return _png(struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0), raw)


def spectra_bars(x: int, y: int) -> tuple:
    """Vertical bars of the six Spectra 6 inks, with a blue/green checker band in the middle."""
    names = list(SPECTRA6_RGB)
    if 200 <= y < 280:
        return SPECTRA6_RGB[names[4 + (x // 40 + y // 40) % 2]]
    return SPECTRA6_RGB[names[min(5, x * 6 // 800)]]


def color_bars(x: int, y: int) -> tuple:
    """Vertical black/white/yellow/red bars, with a red/yellow checker band in the middle."""
    names = ["black", "white", "yellow", "red"]
    if 200 <= y < 280:
        return BWRY_RGB[names[2 + (x // 40 + y // 40) % 2]]
    return BWRY_RGB[names[min(3, x * 4 // 800)]]


def _png(ihdr: bytes, raw: bytes) -> bytes:
    def chunk(tag: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def ramp(levels: int = 16) -> Callable[[int, int], int]:
    """Vertical bands, one per gray level (0 = black at the left)."""
    return lambda x, y: min(levels - 1, x * levels // 1872)


def checkerboard(size: int = 40) -> Callable[[int, int], bool]:
    return lambda x, y: (x // size + y // size) % 2 == 0


def bars(n: int = 8) -> Callable[[int, int], bool]:
    """Vertical stripes; asymmetric (left half thin bars) so orientation bugs show up."""
    return lambda x, y: (x // (WIDTH // n)) % 2 == 0 if x < WIDTH // 2 else (y // 60) % 2 == 0


_FONT = {  # 5x7 digits
    "0": "01110100011001110101110011000101110", "1": "00100011000010000100001000010001110",
    "2": "01110100010000100010001000100011111", "3": "11111000100010000010000011000101110",
    "4": "00010001100101010010111110001000010", "5": "11111100001111000001000011000101110",
    "6": "00110010001000011110100011000101110", "7": "11111000010001000100010000100001000",
    "8": "01110100011000101110100011000101110", "9": "01110100011000101111000010001001100",
}


def big_number(text: str, scale: int = 24) -> Callable[[int, int], bool]:
    """Digits drawn huge and centred: easy to eyeball, distinct per test step."""
    w = len(text) * 6 * scale - scale
    x0, y0 = (WIDTH - w) // 2, (HEIGHT - 7 * scale) // 2

    def pixel(x: int, y: int) -> bool:
        cx, cy = (x - x0) // scale, (y - y0) // scale
        if cx < 0 or cy < 0 or cy >= 7:
            return False
        i, col = divmod(cx, 6)
        if i >= len(text) or col >= 5:
            return False
        return _FONT[text[i]][cy * 5 + col] == "1"

    return pixel


# ---- server ------------------------------------------------------------------------------------------


class Headers(dict):
    """Request headers with case-insensitive lookup (HTTP header names are case-insensitive;
    the TRMNL X modem path delivers them lowercased)."""

    def __init__(self, items=()):
        super().__init__()
        for k, v in items:
            self[k] = v

    def __setitem__(self, k, v):
        super().__setitem__(k.lower(), v)

    def __getitem__(self, k):
        return super().__getitem__(k.lower())

    def __contains__(self, k):
        return isinstance(k, str) and super().__contains__(k.lower())

    def get(self, k, default=None):
        return super().get(k.lower(), default)


@dataclass
class RecordedRequest:
    method: str
    path: str
    headers: dict
    body: bytes
    at: float = field(default_factory=time.time)
    # HTTPS (`MockTrmnl(tls=True)`): whether the connection resumed an earlier TLS session
    tls_resumed: Optional[bool] = None

    def json(self):
        return json.loads(self.body or b"null")


def _tls_context(host: str) -> ssl.SSLContext:
    """Server context with a fresh self-signed P-384 ECDSA certificate."""
    tmp = tempfile.mkdtemp(prefix="trmnl-mock-tls-")
    try:
        key, cert, conf = (os.path.join(tmp, f) for f in ("key.pem", "cert.pem", "req.cnf"))
        # A v3 certificate (mbedTLS rejects v1); a config file works with OpenSSL and LibreSSL.
        san = f"IP:{host}" if host.replace(".", "").isdigit() else f"DNS:{host}"
        with open(conf, "w") as f:
            f.write("[req]\ndistinguished_name = dn\nx509_extensions = v3\nprompt = no\n"
                    f"[dn]\nCN = {host}\n"
                    f"[v3]\nbasicConstraints = CA:FALSE\nsubjectAltName = {san}\n")
        subprocess.run(
            # named_curve: LibreSSL (macOS) otherwise writes explicit curve parameters,
            # which mbedTLS rejects ("bad certificate", even with verification off)
            ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:secp384r1",
             "-pkeyopt", "ec_param_enc:named_curve", "-nodes",
             "-keyout", key, "-out", cert, "-days", "2", "-config", conf, "-sha384"],
            check=True, capture_output=True,
        )
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.minimum_version = ctx.maximum_version = ssl.TLSVersion.TLSv1_2
        ctx.set_ciphers("ECDHE-ECDSA-AES256-GCM-SHA384")
        ctx.load_cert_chain(cert, key)
        return ctx
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


class MockTrmnl:
    """Serves /api/setup, /api/display, /api/log and /images/<name>.bmp.

    Attributes you can change between steps:
        setup: dict merged into the /api/setup response (set to None to answer 404 "not registered").
        display: dict describing the next /api/display answer: image (name), refresh_rate,
                 plus any raw fields (update_firmware, firmware_url, special_function, ...).
        display_queue: list of such dicts consumed first, one per request.

    With `tls=True` it speaks HTTPS: TLS 1.2 only, ECDHE-ECDSA-AES256-GCM-SHA384 with a
    P-384 certificate, so the handshake needs SHA-384, ECDSA, ECDH and AES-GCM (the
    devices connect with certificate checks off, like they do to trmnl.app).
    """

    def __init__(self, host: str = "127.0.0.1", port: int = 0, tls: bool = False):
        self.requests: list[RecordedRequest] = []
        self.images: dict[str, bytes] = {}
        self.filenames: dict[str, str] = {}
        self.files: dict[str, tuple[str, bytes]] = {}
        self.api_key = "sim-test-api-key"
        self.friendly_id = "SIMTST"
        self.setup: Optional[dict] = {}
        self.display: dict = {"image": "default", "refresh_rate": 900}
        self.display_queue: list[dict] = []
        self.faults: dict[str, dict] = {}
        # How the device names this server. Use a hostname (with the simulator's
        # `--dns NAME=10.0.2.2`) to make the device resolve it, e.g. for DNS fault tests.
        self.device_host = "10.0.2.2"
        self._closing = threading.Event()
        self._cv = threading.Condition()
        self.set_image("default", big_number("0"))
        mock = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _handle(self):
                try:
                    self._serve()
                except (BrokenPipeError, ConnectionResetError):
                    pass  # the device went away (e.g. a power-loss fault mid-download)

            def _serve(self):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n) if n else b""
                rec = RecordedRequest(self.command, self.path.split("?")[0], Headers(self.headers.items()), body,
                                      tls_resumed=getattr(self.connection, "session_reused", None))
                with mock._cv:
                    mock.requests.append(rec)
                    mock._cv.notify_all()
                fault = mock._take_fault(rec.path) or {}
                if fault.get("hang"):
                    mock._closing.wait(300)
                    return
                if fault.get("delay"):
                    mock._closing.wait(fault["delay"])
                if fault.get("close"):
                    self.close_connection = True
                    return
                if fault.get("redirect"):
                    self.send_response(fault.get("status") or 307)
                    self.send_header("Location", fault["redirect"])
                    self.send_header("Content-Length", "0")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    return
                code, ctype, payload = mock._respond(rec)
                if fault.get("status") is not None:
                    code, ctype, payload = fault["status"], "text/plain", f"fault: HTTP {fault['status']}".encode()
                if fault.get("body") is not None:
                    body = fault["body"]
                    payload = body.encode() if isinstance(body, str) else bytes(body)
                ctype = fault.get("content_type") or ctype
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                if fault.get("chunked"):
                    # No Content-Length: the body is sent chunked.
                    self.send_header("Transfer-Encoding", "chunked")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    body = payload[: fault["truncate"]] if fault.get("truncate") is not None else payload
                    for i in range(0, len(body), 4096):
                        part = body[i:i + 4096]
                        self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
                    if fault.get("truncate") is None:
                        self.wfile.write(b"0\r\n\r\n")
                    return
                # A truncated body still announces its full length, like a connection that dies.
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                if fault.get("truncate") is not None:
                    payload = payload[: fault["truncate"]]
                rate = fault.get("rate")
                if rate:
                    step = max(1, rate // 20)
                    for i in range(0, len(payload), step):
                        self.wfile.write(payload[i:i + step])
                        self.wfile.flush()
                        if mock._closing.wait(step / rate):
                            return
                else:
                    self.wfile.write(payload)

            do_GET = do_POST = _handle

        self.httpd = ThreadingHTTPServer((host, port), Handler)
        self.tls = tls
        if tls:
            # Handshake in the handler thread, not in the accept loop.
            self.httpd.socket = _tls_context(self.device_host).wrap_socket(
                self.httpd.socket, server_side=True, do_handshake_on_connect=False)
        self.port = self.httpd.server_address[1]
        self._thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self._thread.start()

    # URLs as seen from the device (10.0.2.2 is the host) and from the host.
    @property
    def device_url(self) -> str:
        return f"{self._scheme}://{self.device_host}:{self.port}"

    @property
    def host_url(self) -> str:
        return f"{self._scheme}://127.0.0.1:{self.port}"

    @property
    def _scheme(self) -> str:
        return "https" if self.tls else "http"

    def close(self) -> None:
        self._closing.set()
        self.httpd.shutdown()
        self.httpd.server_close()

    def __enter__(self):
        return self

    def __exit__(self, *e):
        self.close()

    def set_image(self, name: str, pixel: Callable[[int, int], bool]) -> bytes:
        """Register an 800x480 1-bit BMP (TRMNL OG); returns the PNG a matching screenshot should equal."""
        self.images[name] = bmp_1bit(pixel)
        self._stamp(name)
        return png_gray(pixel)

    def set_png(self, name: str, level: Callable[[int, int], int], width: int = 1872, height: int = 1404,
                bits: int = 1) -> bytes:
        """Register a grayscale PNG (TRMNL X: 1872x1404, 1- or 4-bit); served as images/<name>.png.
        Returns the expected screenshot PNG (exact for 1-bit; 4-bit grays are panel-model approximations)."""
        self.images[name + ".png"] = png_image(level, width, height, bits)
        self._stamp(name)
        return expected_gray(level, width, height, bits)

    def _stamp(self, name: str) -> None:
        """Server-style filename for an image: plugin-<6 hex id>-<epoch>. The TRMNL X caches
        images under it: the first 14 chars identify the plugin (a new version replaces the
        old one) and files whose timestamp is over 24 h old are purged."""
        import hashlib

        uid = hashlib.sha1(name.encode()).hexdigest()[:6]
        self.filenames[name] = f"plugin-{uid}-{int(time.time())}"

    def set_color_png(self, name: str, color: Callable[[int, int], tuple], width: int = 800,
                      height: int = 480) -> bytes:
        """Register a color image (TRMNL BWRY), reduced to black/white/yellow/red and served
        as a 2-bit palette PNG like the TRMNL server does (the OG-family firmware's PNG
        decoder can't take 800 px wide truecolor rows). Returns the RGB PNG a screenshot
        should match."""
        palette = list(BWRY_RGB.values())
        self.images[name + ".png"] = png_palette(lambda x, y: bwry_quantize(*color(x, y)), palette, width, height)
        self._stamp(name)
        return expected_bwry(color, width, height)

    def set_spectra6_png(self, name: str, color: Callable[[int, int], tuple], width: int = 800,
                         height: int = 480) -> bytes:
        """Register a color image for a Spectra 6 panel (reTerminal E1002), reduced to its
        six inks and served as a 4-bit palette PNG. Returns the RGB PNG a screenshot should
        match."""
        palette = list(SPECTRA6_RGB.values())
        self.images[name + ".png"] = png_palette(lambda x, y: spectra6_quantize(*color(x, y)), palette, width, height)
        self._stamp(name)
        return expected_spectra6(color, width, height)

    def set_fault(self, path: str, *, status: Optional[int] = None, body: Optional[str | bytes] = None,
                  content_type: Optional[str] = None, delay: float = 0, hang: bool = False,
                  truncate: Optional[int] = None, rate: Optional[int] = None, close: bool = False,
                  redirect: Optional[str] = None, chunked: bool = False, times: Optional[int] = None) -> None:
        """Make requests to `path` (exact, or a prefix ending in "*", e.g. "/images/*") misbehave:

            status: answer with this HTTP status (and a short text body)
            body: answer with this body instead (e.g. malformed JSON: '{"status": 0, "image_')
            content_type: override the Content-Type
            delay: seconds to wait before answering (longer than the device's timeout = a timeout)
            hang: never answer (until the mock is closed)
            truncate: send only this many bytes of the body (Content-Length is still the full
                size), then close the connection
            rate: send the body at this many bytes per second (a slow download)
            close: close the connection without answering
            redirect: answer with a redirect to this URL (status 307 unless `status` is given)
            chunked: send the body chunked, without a Content-Length (with `truncate`: stop
                after that many bytes without the final chunk)
            times: only the next N matching requests (default: until cleared)
        """
        spec = {"status": status, "body": body, "content_type": content_type, "delay": delay, "hang": hang,
                "truncate": truncate, "rate": rate, "close": close, "redirect": redirect, "chunked": chunked,
                "times": times}
        with self._cv:
            self.faults[path] = spec

    def clear_faults(self) -> None:
        with self._cv:
            self.faults.clear()

    def _take_fault(self, path: str) -> Optional[dict]:
        with self._cv:
            for pattern, spec in self.faults.items():
                if pattern == path or (pattern.endswith("*") and path.startswith(pattern[:-1])):
                    if spec["times"] is not None:
                        spec["times"] -= 1
                        if spec["times"] <= 0:
                            del self.faults[pattern]
                    return spec
        return None

    def set_file(self, path: str, content_type: str, data: bytes) -> str:
        """Serve arbitrary bytes (e.g. a firmware binary for OTA tests); returns the device URL."""
        self.files[path] = (content_type, data)
        return self.device_url + path

    def _respond(self, rec: RecordedRequest) -> tuple[int, str, bytes]:
        js = lambda obj, code=200: (code, "application/json", json.dumps(obj).encode())
        if rec.path == "/api/setup":
            if self.setup is None:
                return js({"status": 404, "api_key": None, "friendly_id": None, "image_url": None,
                           "message": "MAC Address not registered"}, 404)
            return js({"status": 200, "api_key": self.api_key, "friendly_id": self.friendly_id,
                       "image_url": f"{self.device_url}/images/default.bmp",
                       "message": "Register at usetrmnl.com/signup with Device ID 'SIMTST'", **self.setup})
        if rec.path == "/api/display":
            d = dict(self.display_queue.pop(0) if self.display_queue else self.display)
            image = d.pop("image", "default")
            ext = "png" if image + ".png" in self.images else "bmp"
            resp = {"status": 0, "image_url": f"{self.device_url}/images/{image}.{ext}", "filename": self.filenames.get(image, image),
                    "refresh_rate": 900, "update_firmware": False, "firmware_url": None, "reset_firmware": False,
                    "special_function": "sleep"}
            resp.update(d)
            return js(resp)
        if rec.path == "/api/log":
            return js({"status": 200})
        if rec.path.startswith("/images/") and rec.path.endswith(".png"):
            img = self.images.get(rec.path[len("/images/"):])
            if img is None:
                return 404, "text/plain", b"no such image"
            return 200, "image/png", img
        if rec.path.startswith("/images/") and rec.path.endswith(".bmp"):
            img = self.images.get(rec.path[len("/images/"):-4])
            if img is None:
                return 404, "text/plain", b"no such image"
            return 200, "image/bmp", img
        if rec.path in self.files:
            ctype, data = self.files[rec.path]
            return 200, ctype, data
        return 404, "text/plain", b"not found"

    def wait_for_request(self, path: str, after: int = 0, timeout_s: float = 60) -> RecordedRequest:
        """Wait for request number >= `after` to `path` (use `len(mock.requests)` as a cursor)."""
        deadline = time.time() + timeout_s
        with self._cv:
            while True:
                for r in self.requests[after:]:
                    if r.path == path:
                        return r
                left = deadline - time.time()
                if left <= 0:
                    seen = [r.path for r in self.requests]
                    raise TimeoutError(f"no request to {path} within {timeout_s}s (seen: {seen})")
                self._cv.wait(left)

    def count(self, path: str) -> int:
        return sum(1 for r in self.requests if r.path == path)
