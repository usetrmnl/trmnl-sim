# trmnl-sim

A simulator for TRMNL devices that runs **unmodified compiled firmware**: the merged image
you would flash at 0 (`merged_firmware.bin`), plus its ELF, the same path with the `.elf`
extension (`merged_firmware.elf`). It gives you a window with the e-paper display and an HTTP control API for
automated tests ([trmnl-spec](https://github.com/usetrmnl/trmnl-spec)), locally or in
GitHub Actions.

The device is picked by the PlatformIO env, from `--env` or the name of the image's directory
(`.pio/build/<env>`). The firmware's post-build steps write both files. Envs without a merge step in the firmware's `platformio.ini` (e.g.
`trmnl_test`, `local`, `WAVESHARE_397`) have no image to run.

TRMNL devices:

- `trmnl`
- `trmnl_4clr`
- `TRMNL_X`
- `trmnl_gen2`
- `trmnl_gen2_4clr`

BYOD boards (the firmware's other `device_list[]` rows):

- `seeed_xiao_esp32c3`
- `TRMNL_7inch5_OG_DIY_Kit`
- `TRMNL_7inch5_OG_DIY_Kit_3CLR`
- `TRMNL_7inch5_OG_DIY_Kit_6CLR`
- `TRMNL_4inch26_DIY_Kit`
- `seeed_reTerminal_E1001`
- `seeed_reTerminal_E1002`
- `seeed_reTerminal_E1004`
- `seeed_sticky`
- `xteink_x4`
- `xteink_x3`
- `WAVESHARE_397`
- `m5_paper_mono`
- `m5_paper_color`
- `TRMNL_X_PAPERS3`
- `TRMNL_X_LILYGO_T5PRO`
- `trmnl_steam`
- `TRMNL_X_SENSORIAC5`

## What is simulated

Common to all devices:

| Part | How |
|---|---|
| Boot | The real mask ROM (from Espressif's ROM ELFs) and your 2nd-stage bootloader: partition table, OTA slot selection, SHA-256 check, flash MMU, deep-sleep wake stubs |
| WiFi | A high-level model replaces the binary driver (scans, joins, soft-AP); lwIP, DHCP, DNS, TLS, AsyncTCP and the captive portal are the firmware's own |
| Network | A user-mode router/NAT (`vnet`) to the internet, or `--offline`. The host is `10.0.2.2`; an NTP server at `10.0.2.123` gives the host's clock (offline, NTP names resolve to it) |
| Sleep | Deep sleep (timer and GPIO wake, RTC memory, correct wake cause) and light sleep |
| Persistence | Flash is a file, so credentials, API key and SPIFFS/LittleFS survive restarts; `--erase` gives a factory-fresh device |

## Requirements

- Rust (stable, 1.85+).
- A PlatformIO build of the firmware in `../trmnl-firmware` (`pio run -e trmnl`, `-e TRMNL_X`,
  ... or a BYOD board's env). The X build also needs its `littlefs.bin`, which its post-build
  script downloads.
- The chips' mask ROM ELFs: the C3 and S3 ones come with PlatformIO's `tool-esp-rom-elfs`;
  the C5's production-silicon ROM (`esp32c5_rev100_rom.elf`) comes from Espressif's
  [esp-rom-elfs](https://github.com/espressif/esp-rom-elfs/releases), which
  `scripts/fetch-rom-elfs.sh` (run by `bin/setup`) puts in `rom/`. `--rom` or `TRMNL_SIM_ROM`
  overrides the search.
- PlatformIO needs Python; the integration tests
  ([trmnl-spec](https://github.com/usetrmnl/trmnl-spec)) need Ruby 3.2+ with Bundler.

## Quick start

```sh
bin/setup      # install Rust (rustup), a C toolchain, the ESP32 ROM ELFs
bin/build      # cargo build --release
bin/sim ../trmnl-firmware/.pio/build/trmnl/merged_firmware.bin   # build, then run a merged image
bin/sim ../trmnl-firmware/.pio/build/TRMNL_X/merged_firmware.bin --erase   # extra arguments go to trmnl-sim
bin/test       # fmt, clippy, unit tests (the integration tests: ../trmnl-spec, rake spec)
```

Or by hand:

```sh
cargo build --release
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl/merged_firmware.bin     # TRMNL OG
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl_4clr/merged_firmware.bin  # TRMNL BWRY
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/TRMNL_X/merged_firmware.bin   # TRMNL X
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/seeed_reTerminal_E1002/merged_firmware.bin  # reTerminal E1002
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl_gen2/merged_firmware.bin   # TRMNL OG gen 2 (ESP32-C5)
```

The GUI's left utility panel has tabs for **Server** (the built-in mock server),
**Faults**, **NVS** and **BLE** (the mock Bluetooth controller's state, the decoded
advertisement, and a manual central for raw ATT exchanges); a tab is marked `*` while
something in it is live. Click the selected tab again to collapse the panel. **NVS**
shows the firmware's saved values, grouped by partition and namespace. Search by key, type, or value, copy values, and refresh
manually or automatically. Values are unmasked; blobs appear as hexadecimal bytes.
While the firmware is in deep sleep, you can add, edit, or delete preferences. Changes
are saved immediately without waking the device and take effect on its next wake.
See [Preferences](#preferences) for the HTTP API and editing constraints.

**A fresh device** (`--erase`) boots into WiFi setup. Its portal is at
**http://127.0.0.1:8080/**: pick **TRMNL-Sim** (any password but `fail`) and a server. Use
`--mac D8:3B:DA:12:34:56` to run as your own device.

### Fault injection

The [control API](#control-api) (`POST /faults`), `--faults JSON` and the Ruby client inject
faults:

| Fault | JSON (`POST /faults`, `--faults`) | |
|---|---|---|
| Latency | `{"net": {"latency_ms": 300}}` | added to every packet towards the device |
| Packet loss | `{"net": {"loss": 0.1}}` | each packet, each direction |
| Bandwidth | `{"net": {"bandwidth_bps": 16000}}` | bytes per second, each direction |
| DNS failure | `{"net": {"dns": "servfail"}}` | or `nxdomain`, `empty`, `timeout` |
| No internet | `{"net": {"no_internet": true}}` | WiFi and DHCP work; everything else times out |
| Internet down | `{"net": {"offline": true}}` | only the host is reachable, like `--offline` |
| Cut connections | `{"net": {"tcp_cut": {"after_bytes": 20000, "stall": false, "port": 8080}}}` | new TCP connections get a RST (or stall) after N bytes towards the device |
| Power loss | `{"power_loss": {"partition": "nvs", "op": "program", "nth": 3, "cut": "torn"}}` | one-shot power cut at the `nth` flash `op` (`any`, `program`, `erase`) in a partition or `range`; `cut`: `before`, `torn` (half written) or `after` |
| I2C device absent | `{"i2c_absent": [85]}` | NACKs everything (X: `0x55` gauge, `0x44` touch bar, `0x20` expander, `0x68` PMIC) |
| Panel stuck | `{"panel_busy_stuck": true}` | OG/BWRY: BUSY held; X: the PMIC never reports power good |
| Modem unresponsive | `{"modem_unresponsive": true}` | X: the modem ignores AT commands |
| Modem AT errors | `{"modem_at_errors": ["AT+CWMODE"]}` | X: ERROR for AT commands starting with these |
| Chip temperature | `{"chip_temp_c": 60}` | the on-chip sensor reads this instead of 25 °C |
| Fuel gauge | `{"gauge_reset": true}` | X: the BQ27427 has a power-on reset (once) |
| Touch bar | `{"touch_bar": "reset"}` | X: the IQS323 resets (once); `"lockup"`, `"ati_error"` |

`POST /faults` merges (`null` clears one) and `DELETE /faults` clears all. Faults are the
environment, not device state: save points don't record them. Network faults also apply to
the X's modem requests (but packet loss). HTTP faults (500s, bad JSON, cut or slow bodies) are
set per path on the Ruby mock server: `mock.set_fault("/api/display", status: 500)`.

A power cut shows on the console as `[sim] power lost: program #3 at 0xa060 ...`;
`GET /faults` lists the partition table and flash program/erase counts, and
`RUST_LOG=flash=trace` logs every program and erase.

### Built-in mock server

`--mock-server` (port 8090; `=0` for any) starts a TRMNL server in the simulator, so you can
drive the device's content without a trmnl.app account. The device URL is
`http://10.0.2.2:8090`; keep the port fixed, since the device remembers the URL.

Images added to it are resized and dithered to what the panel takes (1-bit BMP on the OG,
4-color PNG on the BWRY, 4-bit PNG on the X) with server-style filenames. The control API's
[`/mock` endpoints](#control-api) set the images and playlist, the display response (refresh
rate, special function, registration, friendly ID), fields for the next request only (a
firmware update, a reset; the Server tab's firmware update needs the app image chosen first,
or `--ota-firmware`), HTTP failures per route in the firmware's `scripts/mock_server.py`
syntax, and list the recorded requests.

Headless, e.g. for serial output or a screenshot:

```sh
trmnl-sim ../trmnl-firmware/.pio/build/trmnl/merged_firmware.bin --headless --seconds 60 --screenshot screen.png
```

Production builds don't log, so the simulator mirrors the firmware's `Log_*` messages to the
console (`[sim] log I: src/bl.cpp [701]: …`) for tests to wait on.

**A factory-fresh TRMNL X** (`--erase`) flashes its modem (about 13 s of virtual time), then
waits in shipment mode until docked (`POST /dock`) and restarts into setup.

## Command line

| Option | |
|---|---|
| `<firmware>` | Merged flash image (`merged_firmware.bin`); its ELF is the same path with `.elf` |
| `--flash PATH` | Flash image (default `sim-flash.bin` next to the firmware); the firmware is written on every start, NVS and SPIFFS are kept |
| `--erase` | Start from erased flash |
| `--mac AA:BB:..` | eFuse MAC, i.e. the device identity on the server |
| `--headless` | No window; serial output to stdout. Exits with status 2 if the CPU halts |
| `--control ADDR` | Serve the [control API](#control-api), e.g. `127.0.0.1:7878` (port 0 = pick one) |
| `--turbo` | Don't pace to wall-clock time (see [Time](#time)) |
| `--fast-sleep` | Fast-forward deep sleeps |
| `--offline` | Only the host (`10.0.2.2`) and the NTP server are reachable |
| `--dns NAME=IP` | Answer DNS for NAME locally (repeatable) |
| `--portal-port N` | Host port for the captive portal (default 8080, 0 = any) |
| `--mock-server[=PORT]` | Start the [built-in mock server](#built-in-mock-server) (default 8090, 0 = any) |
| `--ota-firmware PATH` | App image (`firmware.bin`) the built-in server offers at `/firmware.bin` for OTA; its ELF (same path, `.elf`) is loaded too |
| `--elf PATH` | Extra firmware ELFs the device may boot after an OTA (repeatable) |
| `--seconds S` | Stop after S seconds of virtual time |
| `--screenshot PNG` | Save the display when the run ends |
| `--panel-rev HEX` | What the panel's REV command returns (the `Panel-Rev` header) |
| `--rom PATH` | ROM ELF for the build's chip (or `$TRMNL_SIM_ROM`) |
| `--trace f1,f2` | Log every call to these firmware functions |
| `--profile` | Print where the CPU spent its time on exit |
| `--coverage FILE` | Write an lcov tracefile on exit (see [Code coverage](#code-coverage)) |
| `--coverage-root DIR` | Source paths relative to DIR (default: the firmware checkout) |
| `--coverage-include P,..` | Only report files under these prefixes, e.g. `src/,lib/` |
| `--memcheck[=halt]` | Check [memory use](#memory-checking); `=halt` stops at the first error |
| `--memcheck-suppress F,..` | Ignore violations with these functions in their stacks |
| `--scale Z` | Initial display zoom (0 = fit) |
| `--restore FILE` | Start from a [save point](#save-points) (its flash and MAC replace `--flash`/`--mac`) |
| `--env NAME` | The PlatformIO env the firmware was built with; picks the board. Default: the firmware directory's name |
| `--sensor NAME` | Environment sensor on an SPI-panel board's I2C (repeatable): `scd41`, `aht20` |
| `--wifi-networks JSON` | Access points in range, replacing the defaults (`ssid`, `password`, `rssi`, `channel`, `open`, `internet`) |
| `--faults JSON` | Inject [faults](#fault-injection) from the start (repeatable, merged) |

The default networks are **TRMNL-Sim** (any password) and **Neighbors WiFi** (`hunter2hunter2`);
**TRMNL-Sim-5G** (channel 36) is seen only by 5 GHz radios (the C5 and the X's modem, which
then does all HTTP). Networks taking any password reject `fail`, to test a failed join.

### Bluetooth

The mock backend runs the firmware's **real NimBLE host, GATT endpoints and
Espressif Security 1** over a portable simulated HCI controller. It has been
verified with the OG ESP32-C3 `trmnl` build using ESP-IDF 4.4.7. Other firmware
controller interfaces are not yet verified. Mock Bluetooth has no host radio or
platform framework dependencies.

```sh
bin/sim ../firmware/.pio/build/trmnl/merged_firmware.bin --headless --control 127.0.0.1:7878
```

Bluetooth is always mocked, on every host platform. It needs no host Bluetooth
adapter, permissions, or backend configuration and does not connect to physical
phones. `/status.bluetooth` reports `active` (`"mock"` while the guest controller
is initialized, otherwise `null`), `initialized` (controller enabled),
`advertising`, `connection`, `advertisement`, and `scan_response`.

The control API provides one mock central and raw ATT exchanges; discover GATT
handles by UUID from guest responses rather than assuming firmware handle numbers:

| POST path | JSON body | Response |
|---|---|---|
| `/bluetooth/connect` | `{}` | `connection` token |
| `/bluetooth/att` | `{"connection": TOKEN, "data": [10, HANDLE_LOW, HANDLE_HIGH]}` | ATT reply in `data` |
| `/bluetooth/receive` | `{"connection": TOKEN}` | Next notification/indication in `data`, or `[]` |
| `/bluetooth/disconnect` | `{"connection": TOKEN}` | Disconnect completion accepted |

For example, after `/status.bluetooth.advertising` becomes `true`, this script
connects, discovers primary GATT services, and disconnects (Python standard library only):

```python
import json
from urllib.request import Request, urlopen


def post(path, body):
    request = Request("http://127.0.0.1:7878" + path,
                      data=json.dumps(body).encode(),
                      headers={"Content-Type": "application/json"})
    with urlopen(request, timeout=15) as response:
        return json.load(response)


connection = post("/bluetooth/connect", {})["connection"]
try:
    reply = post("/bluetooth/att", {
        "connection": connection,
        "data": list(bytes.fromhex("100100ffff0028")),
    })
    print(bytes(reply["data"]).hex())
finally:
    post("/bluetooth/disconnect", {"connection": connection})
```

ATT errors are returned as ATT bytes, not turned into backend failures. A single
request may be pending; its limit is five seconds of virtual time, with an eight
second wall-clock bound when paused. Timeout discards the connection. Reset and
reconnect invalidate tokens, and pending requests are failed without replay.
Notifications are bounded to 64 queued packets; overflow disconnects the central.
Indications are acknowledged when queued. Write commands complete on enqueue.
The implemented radio is peripheral-only, one link, without SMP/link encryption;
application-layer Security 1 still executes in full. Central-role scanning and
whitelists are not implemented.

Firmware integration coverage lives in [trmnl-spec](https://github.com/usetrmnl/trmnl-spec),
with the shared Ruby simulator client and TLS mock. Its OG Bluetooth specs cover discovery,
QR-based Security 1, encrypted status, long writes, oversize rejection, reconnect,
wrong proof, reset invalidation, and encrypted WiFi/setup-code handoff:

```sh
cd ../trmnl-spec
ENVS=trmnl:full FIRMWARE_REPO=../firmware SIM_REPO=../trmnl-sim \
  bundle exec rspec spec/core/og_bluetooth_spec.rb
```

See that repository's setup instructions for the QR decoder and `PAIRING_HOST` override
when testing firmware built for another setup server. The specs use offline networking
and a local TLS server; they do not contact the configured real server.

### Preferences

`GET /preferences` returns `{ "ok": true, "editable": bool, "entries": [...], "warnings": [...] }`.
Each entry contains `partition`, `namespace`, `key`, `type`, and `value`. Values are
strings: decimal integers, literal text, or space-separated hexadecimal blob bytes.
This preserves the full precision of 64-bit integers. Values are never masked.

`PUT /preferences` creates or replaces one key; `DELETE /preferences` deletes one.
Both accept JSON and return the updated snapshot. Partition, namespace and key are
required. PUT also requires `type` and a string `value`:

```sh
curl http://127.0.0.1:7878/preferences
curl -X PUT http://127.0.0.1:7878/preferences -H 'Content-Type: application/json' \
  -d '{"partition":"nvs","namespace":"data","key":"friendly_id","type":"string","value":"TEST123"}'
curl -X DELETE http://127.0.0.1:7878/preferences -H 'Content-Type: application/json' \
  -d '{"partition":"nvs","namespace":"data","key":"friendly_id"}'
```

Supported types: `string`, `blob`, `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`.
Arduino booleans are stored as `u8` (`"0"` or `"1"`). Names contain 1–15 UTF-8 bytes;
strings allow up to 3999 bytes. Blobs accept hexadecimal pairs with optional spaces.
An empty string or blob is allowed. Integer values must fit their declared type.

Reads work in any state. Mutations are checked on the emulator thread and require
**deep sleep**: paused active firmware, light sleep, and halted firmware are rejected.
Pausing an already sleeping device freezes its wake timer and still allows edits.
The device remains asleep with its existing wake schedule; use `POST /wake` to wake
it early. If the simulator process is closed, its HTTP API is unavailable.

Edits repack only the selected NVS partition, preserving namespace IDs and other
committed values, and reserve an erased page for firmware garbage collection. The
new flash image is atomically saved before live flash is replaced. Unreadable or
encrypted partitions, insufficient space, missing delete targets, invalid values,
and disallowed power states return HTTP 409 without applying the change. Malformed
requests return 400; a missing emulator response returns 504. Host edits bypass
simulated flash faults and do not count as guest program/erase operations.

Firmware integration coverage lives in `trmnl-spec`, using its onboarded-device fixtures
and temporary flash copies:

```sh
cd ../trmnl-spec
ENVS=trmnl:full FIRMWARE_REPO=../firmware SIM_REPO=../trmnl-sim \
  bundle exec rspec spec/general/tooling/preferences_spec.rb
```

The specs cover typed edits, validation, multi-page blobs, unchanged unrelated values,
active-CPU rejection, firmware reads after wake, and persistence across process restart.

### Time

Virtual time comes from executed cycles, paced to wall-clock time by default. `--turbo`
fast-forwards idle periods, but runs in real time while the host network is waited on and
while the setup portal is up (`POST /wifi {"portal_client": false}` lifts that), so no
firmware timeout fires early. Deep sleep lasts its real duration unless `--fast-sleep`; end
it early with `POST /wake` or the button.

### Save points

A save point captures the device, e.g. "onboarded, asleep, showing image X", so you can
return to it without redoing setup: `POST /savepoint` / `POST /restore`, or `--restore FILE`.

- **Taken in deep sleep**, it keeps everything that survives deep sleep (flash, RTC memory
  and registers, the pending wake, the screen and panel RAM, the I2C chips, the modem image,
  battery, dock, WiFi) and resumes that sleep with the same time left.
- **Taken otherwise**, it keeps what survives pulling the battery (flash, screen, modem
  flash) and powers on from there.

Files are compressed (about 1 MB for an OG, 2.5 MB for an X) and tied to their firmware
build; restoring onto another build, or saving mid-refresh, is refused.

### Control API

`--control 127.0.0.1:7878` serves JSON over HTTP.

| | |
|---|---|
| `GET /status` | state, virtual time, display busy and refresh count, WiFi/IP, portal URL, boot count, … |
| `POST /button {"down": bool}` | hold or release the button |
| `POST /press {"ms": N}` | press for N virtual ms; returns after release (`"count": 2, "gap_ms": 150`: a double click) |
| `POST /touch {"zone": "left"\|"center"\|"right", "ms": N}` | TRMNL X touch bar tap (`"down": true/false`: hold / lift a finger) |
| `POST /gesture {"gesture": "swipe_next"\|"swipe_back"\|"flick_next"\|"flick_back"}` | TRMNL X slide (slide mode only) |
| `POST /dock {"docked": bool}` | TRMNL X magnetic dock |
| `POST /reset`, `/power-cycle`, `/wake`, `/quit` | |
| `POST /wifi {"available": bool}` | network in or out of range; `{"networks": [...]}` replaces the access points; `{"portal_client": false}` lets an unattended portal run ahead in turbo |
| `POST /battery {"mv": N}` | |
| `POST /turbo {"on": bool}`, `/pause {"on": bool}` | |
| `POST /debug` | dump CPU state, backtrace, FreeRTOS task and board diagnostics to the console |
| `GET /console?since=N` | serial lines with absolute indices |
| `POST /wait {...}` | block on conditions, `timeout_s`, `settle_ms`; 408 on timeout, 409 if the CPU halted |
| `GET /screenshot[?x=&y=&w=&h=]` | grayscale PNG (0 = ink); RGB on color panels |
| `POST /screenshot/compare?tolerance=&max_ratio=[&x=&y=&w=&h=]` | PNG body in; `{"match", "diff_pixels", "diff_ratio"}` |
| `GET /mock` | built-in mock server state |
| `POST /mock/start {"port": N}`, `/mock/stop` | start (returns `device_url`) or stop it |
| `POST /mock/images?name=&current=1&dither=0&fit=contain\|cover\|stretch&raw=1` | add an image, converted for the panel (`raw=1`: served as is) |
| `GET /mock/images/NAME/expected`, `DELETE /mock/images/NAME` | the PNG the screen should show; remove it |
| `POST /mock/display {...}` | `image`, `refresh_rate`, `special_function`, `playlist`, `auto_advance`, `registered`, `friendly_id`, `api_key`, `extra` |
| `POST /mock/queue {...}`, `DELETE /mock/queue` | fields for the next `/api/display` answer only |
| `POST /mock/faults {"display": [...], "image": [...]}`, `DELETE /mock/faults[?route=]` | queue HTTP failures per route in `mock_server.py` syntax (`"503:2"`, `"timeout=20"`, `"slow=1024,20"`) |
| `POST /mock/files?path=/x.bin` | serve the body at that path |
| `GET /mock/requests?since=N` | recorded device requests |
| `POST /savepoint {"path"?: str, "label"?: str}` | take a [save point](#save-points); 409 if refused |
| `POST /restore {"path": str}` or `{"id": N}` | restore one; 409 on failure |
| `GET /savepoints` | the in-memory save points |
| `POST /coverage {"path": "x.info", "reset": bool}` | with `--coverage`: write the tracefile now, optionally start over |
| `GET /preferences` | NVS values, warnings, and whether editing is currently allowed |
| `PUT /preferences` | create/replace a typed NVS value during deep sleep |
| `DELETE /preferences` | delete an NVS key during deep sleep |
| `GET /memcheck` | with `--memcheck`: violations, suppressed ones, heap statistics, stack marks |
| `GET /faults` | injected faults, power losses, flash counts, the partition table |
| `POST /faults {...}`, `DELETE /faults` | merge [faults](#fault-injection); clear them all |

### Code coverage

`--coverage FILE` records which firmware instructions run (across resets and deep sleeps)
and at exit maps them to source lines through the ELF's DWARF line tables, writing an
[lcov](https://github.com/linux-test-project/lcov) tracefile with lines and functions hit
(0/1 per run). The firmware's own paths are relative to its checkout.

```sh
trmnl-sim ../trmnl-firmware/.pio/build/trmnl/merged_firmware.bin --headless --seconds 30 --coverage og.info --coverage-include src/,lib/
```

`POST /coverage` (Ruby: `sim.write_coverage`) writes one mid-run; after an OTA both builds
are reported. trmnl-spec records coverage on every run and merges it into one report.

It costs about 5-8% of emulation speed. Not covered: the bootloader and ROM (no line
tables) and functions replaced by [HLE](#architecture) hooks (left out, not counted as
missed). A killed simulator writes no tracefile.

### Memory checking

`--memcheck` catches the firmware's memory bugs as they happen, with symbolized stacks:

- **heap-use-after-free**, **heap-buffer-overflow**, accesses to unallocated heap or
  allocator metadata, and **stack-overflow**;
- **double-free** and **invalid-free** (not passed on, so the heap stays consistent);
- **stack high-water marks** of every FreeRTOS task, flagged `low` within 256 bytes;
- heap statistics for internal RAM and PSRAM.

```
[sim] memcheck: heap-use-after-free: 1-byte read at 0x3fcaad88 (pc dns_gethostbyname_addrtype+0x8, task "tiT", core 0)
[sim]   backtrace: dns_gethostbyname_addrtype+0x8 <- dns_gethostbyname+0xa <- sntp_request+0x52 <- ...
[sim]   0x3fcaad88 is the start of a 16-byte block at 0x3fcaad88
[sim]   allocated by task "loopTask": _ZN6String12changeBufferEj+0x40 <- ... <- _ZN11Preferences9getStringEPKc6String+0x40
[sim]   freed by task "loopTask": _ZN6String10invalidateEv+0x1c <- _ZN6StringD2Ev+0x8 <- _ZN5Clock14setTimeFromNTPEv+0xec <- ...
```

Each distinct violation is reported once. `--memcheck=halt` stops at the first (`sim.wait`
then fails); `GET /memcheck` (Ruby: `sim.memcheck`) returns the full report, and a summary
prints at exit. trmnl-spec runs every simulator with `--memcheck=halt`.

How it works: HLE hooks on the IDF heap's `multi_heap_*` layer see every block. A shadow
byte per byte of SRAM and PSRAM marks live, freed, header and never-allocated bytes, and
every CPU load and store (but the allocator's own) is checked against it. Freed blocks wait
in a quarantine and `realloc` always moves, so stale pointers hit poisoned memory. It costs
about 5-12% when on.

Limitations: stacks are exact on the X but heuristic on the OG/BWRY (no frame pointers).
Aligned word loads may run past a block's end unreported (`strlen` does that), overflows
into another live block aren't seen, and static buffers and non-heap stacks aren't checked.

### CI

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs format, clippy and unit tests, then
calls trmnl-spec's workflow with this commit: it builds the firmware and the headless
simulator (`--no-default-features`, no GUI), runs the suite and uploads logs, screens and
coverage. The firmware repository calls that workflow to test its PRs. Adjust the
`usetrmnl/...` names if the repositories live elsewhere.

## Architecture

```
trmnl-sim (bin)        CLI, runner (pacing, power states, commands)
├─ arch/               CPU cores behind MemBus + GuestCpu traits (riscv.rs, xtensa.rs)
├─ soc/                one module per chip, implementing the chip-agnostic Machine trait
│  ├─ esp32c3/         memory map, peripherals, crypto/GDMA, boot flow, interrupt routing
│  ├─ esp32s3/         the same for the dual-core S3, plus PSRAM, LCD_CAM, USB console
│  └─ esp32c5/         the C5: CLIC, PCR/LP domain, cache MMU, AHB DMA, ECC/ECDSA, USB console
├─ periph/             IP blocks shared by chips (SYSTIMER, I2C engine, SHA, AES/RSA math, EC math)
├─ board/              what's wired to the pins (spi_epd.rs: OG, BWRY and SPI-panel BYOD boards;
│                      trmnl_x.rs; parallel_byod.rs: PaperS3, T5 Pro); Board trait
├─ devices/            e-paper controllers (UC8179/UC81xx, SSD16xx, dual-CS, parallel), SPI NOR flash,
│                      ESP-AT modem, I2C chips (TCA9535, TPS65185, IQS323, BQ27427, BQ27220, AXP2101,
│                      M5Stack PY32 and M5IOE1)
├─ coverage/           executed-instruction bitmaps, DWARF line mapping, lcov output
├─ hle/                ESP-IDF function replacements by ELF symbol (WiFi driver, sleep,
│                      ADC), ISA-neutral; hooks can call back into guest code or wrap it
├─ memcheck.rs         heap tracker, shadow memory, stack marks (--memcheck)
└─ firmware.rs         build artifacts, ELF symbols, OTA slot and app selection
crates/
├─ sim-api/            the contract between the emulator thread and front-ends
├─ sim-ui/             egui desktop window
├─ sim-control/        HTTP control API
├─ sim-bluetooth/      portable H4 controller; guest NimBLE/GATT stays in firmware
├─ mock-trmnl/         built-in mock TRMNL server and image conversion (/mock API)
└─ vnet/               user-mode router/NAT (smoltcp) + soft-AP client
```

**Chips and boards.** The image header names the chip (C3, S3 or C5); the board comes from
the PlatformIO env (`--env` or the firmware directory's name). The C3 and C5 share the
RISC-V core; the Xtensa windowed ABI is hidden behind a few `GuestCpu` calls, so the HLE
(WiFi for IDF 4.4 and 5.5, sleep, ADC) is the same code on every chip. A new board is a
`Board` plus its devices; a new chip is a `soc/` module.

**HLE and OTA.** Hooks are bound to the ELF's addresses. Each boot, the simulator
matches the slot's app descriptor to a known ELF, so an OTA to another build needs its ELF
(`--ota-firmware` and the window's firmware picker load the one next to the image; else
`--elf`) or the run halts with a clear message. Coverage leaves out an app picked in the window.

## Limitations

- ESP32-C3, ESP32-S3 and ESP32-C5 only (no classic ESP32). BYOD boards model what the
  firmware uses: no SD cards, touch panels or power latches; charging only on the X and gen 2.
- ESP32-C5: no ADC (HLE'd through `analogRead*`), LP core or PARLIO RX; the ECDSA accelerator
  only verifies; no WiFi 6 details or BLE.
- WiFi is modelled at the IDF driver API, not 802.11: signal, roaming and power save are canned.
- TRMNL OG: nothing on the I2C bus. No USB data or serial input on any device.
- TRMNL X: no accelerometer; the modem does station
  mode and HTTP GET only.
- Timing counts instructions (1 per cycle), not cycles; flash operations are instant.
- The e-paper model is exact in pixels but approximate in grays and ghosting; the 4-color
  refresh is a fixed frame sequence.
- OTA to another build needs its ELF.
- Save points: only deep-sleep ones keep chip state; a powered modem comes back off.

## Troubleshooting

- **"CPU exception at …"**: the message has symbolized registers and a stack scan;
  `--trace fn` logs calls into a suspect function.
- **The firmware seems stuck**: `--headless --seconds N --profile` shows where the CPU
  spends its time, often polling an unmodelled register.
- **An unmodelled register**: `RUST_LOG=trmnl_sim=trace` logs first accesses to them.
- **Memory corruption**: `--memcheck` (see [Memory checking](#memory-checking)) catches the
  bad access where it happens.
- **A hang or crash on the TRMNL X**: `POST /debug` (Ruby: `sim.debug`) prints both cores'
  registers, a backtrace and the running task (`SIM_PEEK=addr,addr` adds memory words);
  `RUST_LOG=modem=debug` logs the AT traffic.
