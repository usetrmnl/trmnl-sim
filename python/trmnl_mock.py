"""A mock TRMNL API server for hermetic integration tests (standard library only).

The simulated device reaches the host as 10.0.2.2, so point the device at
`server.device_url` (e.g. via `Simulator.portal_connect(..., server=mock.device_url)`).

    mock = MockTrmnl()
    mock.set_image("hello", checkerboard(40))
    mock.display = {"image": "hello", "refresh_rate": 300}
    ...
    req = mock.wait_for_request("/api/display")
    assert req.headers["Battery-Voltage"].startswith("4.")
"""

from __future__ import annotations

import json
import struct
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


def png_rgb(color: Callable[[int, int], tuple], width: int = 800, height: int = 480) -> bytes:
    """A truecolor PNG; `color(x, y)` returns (r, g, b)."""
    raw = b"".join(b"\x00" + bytes(c for x in range(width) for c in color(x, y)) for y in range(height))
    return _png(struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0), raw)


def png_palette(color: Callable[[int, int], tuple], palette: list, width: int = 800, height: int = 480) -> bytes:
    """A 2-bit indexed PNG (as TRMNL serves color images); `color(x, y)` must return one
    of the `palette` colors (up to 4)."""
    index = {c: i for i, c in enumerate(palette)}
    rows = []
    for y in range(height):
        row = bytearray((width + 3) // 4)
        for x in range(width):
            row[x // 4] |= index[tuple(color(x, y))] << (6 - 2 * (x % 4))
        rows.append(b"\x00" + bytes(row))
    plte = b"".join(bytes(c) for c in palette)
    ihdr = struct.pack(">IIBBBBB", width, height, 2, 3, 0, 0, 0)

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

    def json(self):
        return json.loads(self.body or b"null")


class MockTrmnl:
    """Serves /api/setup, /api/display, /api/log and /images/<name>.bmp.

    Attributes you can change between steps:
        setup: dict merged into the /api/setup response (set to None to answer 404 "not registered").
        display: dict describing the next /api/display answer: image (name), refresh_rate,
                 plus any raw fields (update_firmware, firmware_url, special_function, ...).
        display_queue: list of such dicts consumed first, one per request.
    """

    def __init__(self, host: str = "127.0.0.1", port: int = 0):
        self.requests: list[RecordedRequest] = []
        self.images: dict[str, bytes] = {}
        self.filenames: dict[str, str] = {}
        self.files: dict[str, tuple[str, bytes]] = {}
        self.api_key = "sim-test-api-key"
        self.friendly_id = "SIMTST"
        self.setup: Optional[dict] = {}
        self.display: dict = {"image": "default", "refresh_rate": 900}
        self.display_queue: list[dict] = []
        self._cv = threading.Condition()
        self.set_image("default", big_number("0"))
        mock = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _handle(self):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n) if n else b""
                rec = RecordedRequest(self.command, self.path.split("?")[0], Headers(self.headers.items()), body)
                with mock._cv:
                    mock.requests.append(rec)
                    mock._cv.notify_all()
                code, ctype, payload = mock._respond(rec)
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            do_GET = do_POST = _handle

        self.httpd = ThreadingHTTPServer((host, port), Handler)
        self.port = self.httpd.server_address[1]
        self._thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self._thread.start()

    # URLs as seen from the device (10.0.2.2 is the host) and from the host.
    @property
    def device_url(self) -> str:
        return f"http://10.0.2.2:{self.port}"

    @property
    def host_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def close(self) -> None:
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
