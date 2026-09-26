# trmnl-sim

A simulator for TRMNL devices that runs **unmodified compiled firmware**: the same
`bootloader.bin`, `partitions.bin` and `firmware.bin` you would flash, plus the
`firmware.elf` from the same build. You get an interactive window with the
e-paper display and the device's controls (button or touch bar, battery, dock), and
an HTTP control API that drives the device from automated integration tests, locally
or in GitHub Actions.

Supported devices, picked automatically from the build:

| PlatformIO env | Device | Chip | Display | Controls |
|---|---|---|---|---|
| `trmnl` | TRMNL OG | ESP32-C3 (RISC-V) | 7.5" 800×480, UC8179 over SPI | button |
| `trmnl_4clr` | TRMNL BWRY | ESP32-C3 (RISC-V) | 7.5" 800×480 black/white/yellow/red (GDEM075F52) | button |
| `TRMNL_X` | TRMNL X | ESP32-S3 (dual-core Xtensa LX7) + ESP32-C5 modem | 10.3" 1872×1404 parallel panel, 16 grays | touch bar (left/center/right), magnetic dock |

![setup screen as rendered by the simulator](tests/integration/golden/setup_screen.png)

## What is simulated

Common to both devices:

| Part | How |
|---|---|
| Boot | The real mask ROM code (from Espressif's ROM ELFs) and your real 2nd-stage bootloader: partition table, OTA slot selection, image SHA-256 check, flash MMU, deep-sleep wake stubs |
| WiFi | The binary WiFi driver is replaced by a high-level model: scans, joins, soft-AP. lwIP, DHCP, DNS, TLS (mbedTLS on the emulated crypto accelerators), AsyncTCP and the captive portal are all the firmware's own code |
| Network | A user-mode router/NAT (`vnet`): DHCP, DNS, TCP/UDP to the internet, or `--offline` for hermetic tests. The device reaches the host machine at `10.0.2.2` |
| Sleep | Deep sleep (timer + GPIO wake, RTC memory kept, correct wake cause) and light sleep |
| Persistence | Flash is a file: WiFi credentials, API key and SPIFFS/LittleFS survive restarts. `--erase` gives a factory-fresh device |

**TRMNL OG** (`trmnl`) and **TRMNL BWRY** (`trmnl_4clr`, the same board with a 4-color
panel; detected from the firmware):

| Part | How |
|---|---|
| CPU | RV32IMC(A) interpreter, 160 MHz, cycle-counted virtual time |
| Peripherals | Register-level models of UART0, GPIO/IO_MUX, interrupt matrix, SYSTIMER, TIMG, RTC_CNTL, eFuse (MAC, chip rev 0.3), SPI flash controller + 4 MB NOR flash, GPSPI2, I2C master, SAR ADC one-shot, cache/MMU, RNG, SHA, AES + GDMA, RSA/MPI |
| Display | UC8179 controller driven over SPI/GPIO. Refreshes are simulated from the LUT waveforms, so full, fast and partial refreshes, 4-gray mode, flashing and BUSY timing behave like the panel. The panel `REV` read (bit-banged) returns `--panel-rev` |
| 4-color panel (BWRY) | The same UC81xx command set with one 2 bit/pixel image (`DTM1`) and the panel's built-in ~16 s refresh: the screen flashes rapidly black and white (every 100 ms for 9 s), then shows the image in exact black/white/yellow/red. The window's **Refresh flashing** checkbox turns the flashes off (same timing). Screenshots and the window are in color |
| Button | GPIO2 with pull-up: presses, holds, double-clicks, and deep-sleep GPIO wake |
| Battery | ADC on GPIO3 behind the ½ divider; settable voltage |

**TRMNL X** (`TRMNL_X`):

| Part | How |
|---|---|
| CPU | Two Xtensa LX7 cores (windowed ABI, FPU, MAC16, loops, atomics) in lockstep at the firmware's CPU clock; idle cores sleep until an interrupt |
| Peripherals | ESP32-S3 memory map with 16 MB flash and 8 MB octal PSRAM behind the cache MMU, interrupt matrix, SYSTIMER, GPIO (49 pins), UART0 with a 128-byte RX FIFO and flow control, USB serial/JTAG console, I2C, LCD_CAM i80 + GDMA, SHA/AES/RSA, eFuse, RTC |
| Display | EPDiy-style 1872×1404 panel: the firmware (FastEPD) clocks rows over the LCD_CAM 16-bit bus and drives SPV/CKV/LE on GPIOs; the TPS65185 PMIC supplies the rails. The panel model applies the per-frame drive to each pixel, so 1-bit and 16-gray images come out as the firmware drew them |
| Touch bar | IQS323 capacitive controller on I2C: left, center and right taps and holds (several fingers at once), with RDY as the deep-sleep (EXT0) wake source. The wake stub's bit-banged I2C read of the touch state works too |
| Dock | Placing the device on its magnetic dock powers USB and the charger: TCA9535 expander inputs (charger power-good and status), the fuel gauge's charging flag, and the `USB-Connected` / `Battery-Charging` request headers. Docking wakes a device in shipment mode |
| Battery | BQ27427 fuel gauge (voltage, state of charge, charging) |
| 5 GHz modem | ESP32-C5 running ESP-AT on UART0 at 5 Mbit/s with RTS/CTS: the factory flash over its ROM loader (`esp-serial-flasher`), then `AT+CWJAP`, SNTP, `AT+HTTPCLIENT` with custom headers. Its HTTP requests are made from the host, so 5 GHz onboarding and image downloads use the same mock servers as the 2.4 GHz path |
## Requirements

- Rust (stable, 1.85+).
- A PlatformIO build of the firmware: `pio run -e trmnl`, `-e trmnl_4clr` and/or
  `-e TRMNL_X` in `trmnl-firmware`. The TRMNL X build also needs its `littlefs.bin` (factory images
  and the modem firmware); its post-build script downloads it into the build dir.
- The ESP32-C3 / ESP32-S3 ROM ELFs from PlatformIO's `tool-esp-rom-elfs` package.
  They are usually already installed; if not, run
  `pio pkg install -g -t platformio/tool-esp-rom-elfs`, or pass `--rom path/to/rom.elf`.
- Python 3.9+ for the integration tests (standard library only).

## Quick start

```sh
cargo build --release
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl     # TRMNL OG
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl_4clr  # TRMNL BWRY
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/TRMNL_X   # TRMNL X
```

The window shows the device. On the OG, click and hold the button on screen, or hold
**Space**. On the X, tap the touch bar under the screen, or use **←/↓/→** (or
**1/2/3**) for left/center/right; hold them for holds. The side panel has reset,
power-cycle, "wake now", WiFi in/out of range, battery voltage, turbo, pause and, on
the X, the dock. The serial console is at the bottom.

**A fresh device** (`--erase`) boots into WiFi setup, like a new TRMNL. Its captive
portal is forwarded to **http://127.0.0.1:8080/**; open it in a browser, pick
**TRMNL-Sim** (any password is accepted) and choose the server. To use your own
device's account, run with its MAC: `--mac D8:3B:DA:12:34:56`.

Headless, e.g. to watch serial output or grab a screenshot:

```sh
trmnl-sim ../trmnl-firmware/.pio/build/trmnl --headless --seconds 60 --screenshot screen.png
```

Production builds never start the firmware's log output. The simulator mirrors the
firmware's `Log_*` messages to the console anyway, as `[sim] log I: src/bl.cpp [701]: …`
lines (on builds where they go through ArduinoLog, like the TRMNL X), so tests can wait
on them. `-D DEV_FIRMWARE` builds print them natively.

**A factory-fresh TRMNL X** (`--erase`) runs the QA path first: it flashes the modem
("Flashing modem firmware…", about 13 s of virtual time), then goes into shipment
mode and waits in light sleep until it is docked. Dock it (side panel, or
`POST /dock`) and it restarts into WiFi setup.

## Command line

| Option | |
|---|---|
| `<build_dir>` | PlatformIO build dir (`firmware.elf`, `bootloader.bin`, `partitions.bin`, `firmware.bin`, or `merged_firmware.bin`) |
| `--flash PATH` | Flash image file (default `<build_dir>/sim-flash.bin`). Firmware images are written into it on every start, like `pio run -t upload`; NVS and SPIFFS are kept |
| `--erase` | Start from erased flash |
| `--mac AA:BB:..` | eFuse MAC, i.e. the device identity on the server |
| `--headless` | No window; serial output to stdout. Exits with status 2 if the emulated CPU halts |
| `--control ADDR` | Serve the [control API](#control-api), e.g. `127.0.0.1:7878` (port 0 = pick one; the address is printed) |
| `--turbo` | Don't pace to wall-clock time (see [Time](#time)) |
| `--fast-sleep` | Fast-forward deep sleeps instead of waiting them out |
| `--offline` | Hermetic network: only the host (`10.0.2.2` → `127.0.0.1`) is reachable |
| `--dns NAME=IP` | Answer DNS for NAME locally (repeatable) |
| `--portal-port N` | Host port forwarded to the captive portal (default 8080, 0 = any free port) |
| `--elf PATH` | Extra firmware ELFs the device may boot after an OTA (repeatable) |
| `--seconds S` | Stop after S seconds of virtual time |
| `--screenshot PNG` | Save the display when the run ends |
| `--panel-rev HEX` | Value returned by the panel's REV command (sent as the `Panel-Rev` header) |
| `--rom PATH` | ROM ELF for the build's chip (or `$TRMNL_SIM_ROM`) |
| `--trace f1,f2` | Log every call to these firmware functions, with arguments and caller |
| `--profile` | Print where the CPU spent its time, and CPU state, on exit |
| `--scale Z` | Initial display zoom (0 = fit) |

The simulated WiFi environment has two networks: **TRMNL-Sim** (any password
works) and **Neighbors WiFi** (password `hunter2hunter2`), at −54 and −81 dBm. The
TRMNL X modem also sees **TRMNL-Sim-5G** (channel 36, −48 dBm, any password); joining
it makes the firmware do all its HTTP through the modem.

### Time

The simulator keeps *virtual time* from executed cycles. By default it is paced to
wall-clock time, so the device behaves in real time. With `--turbo`, idle periods
(FreeRTOS idle, display BUSY waits, light sleep) are fast-forwarded. Turbo still runs
in real time while the host network is being waited on (an open TCP connection, a
DNS lookup, a modem HTTP request) and while the setup portal is up, so no firmware
timeout fires early because of the simulator. A light sleep with no timer armed
(shipment mode, waiting for the dock) also runs in real time. Deep sleep always lasts its real duration unless
`--fast-sleep` is given; end it early with **Wake** or the button.

## Integration testing

```sh
scripts/integration-tests.sh                  # build the sim, run the whole suite
scripts/integration-tests.sh --build-firmware # also `pio run -e trmnl -e trmnl_4clr -e TRMNL_X` first
scripts/integration-tests.sh test_refresh_cycle.RefreshCycle.test_button_press_wakes_and_refreshes
scripts/integration-tests.sh test_trmnl_x     # only the TRMNL X tests
```

The suite ([tests/integration](tests/integration)) runs in about two minutes and needs
no internet. For the TRMNL OG it covers:

- the first-boot setup screen and captive portal;
- onboarding, and WiFi failures (unknown SSID, wrong password);
- `/api/setup` and `/api/display` requests and their headers;
- pixel-exact image rendering;
- sleep duration from `refresh_rate`;
- timer and button wake sources;
- battery reporting;
- persistence across power cycles;
- long-press WiFi reset;
- WiFi out of range;
- a full **OTA update** into the second app slot, and booting it.

For the TRMNL BWRY ([test_trmnl_bwry.py](tests/integration/test_trmnl_bwry.py); skipped if
there is no `trmnl_4clr` build): the device identity (`Model: og_4clr`), a 4-color image
rendered exactly (compared as RGB), and the panel's long refresh.

For the TRMNL X ([test_trmnl_x.py](tests/integration/test_trmnl_x.py); skipped if there is
no `TRMNL_X` build):

- the factory flow: modem flashing, then shipment mode until docked;
- onboarding on 2.4 GHz (the S3's own radio) and on 5 GHz (through the modem);
- request headers, including `Width`/`Height`/`Model`, the RSSI of the radio in use,
  and `USB-Connected`/`Battery-Charging` on and off the dock;
- pixel-exact 1-bit PNGs and a 16-level 4-bit gray ramp on the 1872×1404 panel;
- sleep duration;
- a center tap waking the device (`Update-Source: EXT0`);
- a left tap showing the previous cached image without touching the network.

| Env var | |
|---|---|
| `TRMNL_FIRMWARE_BUILD` | TRMNL OG build dir (default `../trmnl-firmware/.pio/build/trmnl`) |
| `TRMNL_BWRY_BUILD` | TRMNL BWRY build dir (default `../trmnl-firmware/.pio/build/trmnl_4clr`) |
| `TRMNL_X_BUILD` | TRMNL X build dir (default `../trmnl-firmware/.pio/build/TRMNL_X`) |
| `TRMNL_SIM_REALTIME=1` | Run the tests without turbo |
| `TRMNL_SIM_UPDATE_GOLDEN=1` | Rewrite golden screenshots from this run |
| `TRMNL_SIM_ARTIFACTS=DIR` | Save every simulator's log and final screen here |
| `TRMNL_SIM_NETWORK=1` | Also run tests against the real trmnl.app |
| `TRMNL_SIM_BIN` | Simulator binary (default `target/release/trmnl-sim`) |

### Writing tests

Two standard-library Python modules live in [python/](python):

- **`trmnl_sim.Simulator`** launches a headless simulator with the control API and
  wraps every action.
- **`trmnl_mock.MockTrmnl`** is a fake TRMNL API server. It serves `/api/setup`,
  `/api/display`, `/api/log`, images and firmware files, and records every request.
  It also generates BMP images (OG), 1/2/4/8-bit gray PNGs (`set_png`, X) and 4-color
  palette PNGs (`set_color_png`, BWRY; like the TRMNL server, colors are reduced to the
  panel's four first, since the OG-family PNG decoder can't take 800 px truecolor rows), with
  server-style `plugin-<id>-<timestamp>` filenames the X uses for its image cache, and
  returns the PNG you should expect on screen. Request header lookups are
  case-insensitive.

```python
from trmnl_sim import Simulator
from trmnl_mock import MockTrmnl, big_number

with MockTrmnl() as mock, Simulator(BUILD, erase=True, turbo=True, extra_args=("--offline",)) as sim:
    expected = mock.set_image("hello", big_number("42"))
    mock.display = {"image": "hello", "refresh_rate": 600}

    sim.wait(portal=True)                                  # fresh device in setup mode
    sim.portal_connect("TRMNL-Sim", "pw", server=mock.device_url)

    req = mock.wait_for_request("/api/display")
    assert req.headers["Access-Token"] == mock.api_key
    sim.wait(state="deep_sleep")
    assert sim.compare_screen(expected)["match"]           # exact pixels

    sim.press(150)                                         # short press wakes it
    req = mock.wait_for_request("/api/display", after=len(mock.requests) - 1)
    assert req.headers["Update-Source"] == "button"
```

Useful pieces:

- `sim.wait(...)` blocks until all given conditions hold:
  - `console=` (regex over serial output, continuing from the last match)
  - `state=` (`running`, `deep_sleep`, `halted`, …)
  - `min_refreshes=`
  - `display_idle=`
  - `wifi_connected=`
  - `portal=`
  - `min_boots=`
- `sim.press(ms)` holds the button for exactly `ms` of *virtual* time, so hold-duration
  logic is deterministic. `button(down)` holds or releases it indefinitely.
- `sim.touch("left" | "center" | "right", ms)` taps the TRMNL X touch bar the same way;
  `sim.dock(True/False)` puts it on or takes it off the dock.
- `sim.assert_screen(golden, region=(x, y, w, h))` compares against a golden PNG. It
  creates the golden if missing, and writes `*.actual.png` on mismatch.
- `ProvisionedDevice` in `tests/integration/support.py` onboards once, then boots
  copies of that flash. Tests start from a registered device in seconds.
  `tests/integration/support_x.py` does the same for the X: `ShippedX` is a device fresh
  from the factory (QA done, modem flashed, in shipment mode) and `ProvisionedX`
  onboards a copy of it on 5 GHz (or `ssid=SSID_24`).

### Control API

`--control 127.0.0.1:7878` serves JSON over HTTP; the Python client is a thin wrapper.

| | |
|---|---|
| `GET /status` | state, virtual time, display busy and refresh count, WiFi/IP, portal URL, boot count, … |
| `POST /button {"down": bool}` | hold or release the button |
| `POST /press {"ms": N}` | press for N virtual ms; returns after release |
| `POST /touch {"zone": "left"\|"center"\|"right", "ms": N}` | TRMNL X touch bar tap; returns after lift |
| `POST /dock {"docked": bool}` | TRMNL X magnetic dock |
| `POST /reset`, `/power-cycle`, `/wake`, `/quit` | |
| `POST /wifi {"available": bool}` | network in or out of range |
| `POST /battery {"mv": N}` | |
| `POST /turbo {"on": bool}`, `/pause {"on": bool}` | |
| `POST /debug` | dump CPU state (registers, backtrace, current FreeRTOS task) and board diagnostics to the console |
| `GET /console?since=N` | serial lines with absolute indices |
| `POST /wait {...}` | block on conditions (see above), `timeout_s`, `settle_ms`; 408 on timeout, 409 if the CPU halted |
| `GET /screenshot[?x=&y=&w=&h=]` | 8-bit grayscale PNG (0 = ink, 255 = paper); RGB on color panels |
| `POST /screenshot/compare?tolerance=&max_ratio=[&x=&y=&w=&h=]` | PNG body in; `{"match", "diff_pixels", "diff_ratio"}`; on color panels a pixel differs if any channel is off by more than `tolerance` |

### CI

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs format, clippy and unit
tests, then builds the firmware with PlatformIO and runs the integration suite. On
failure it uploads simulator logs, final screens and screen diffs. It can also be
called from the firmware repository to test every firmware PR; see
[docs/firmware-repo-workflow.yml](docs/firmware-repo-workflow.yml). Adjust the
`usetrmnl/trmnl-sim` repository name if the simulator is hosted elsewhere. CI builds
use `--no-default-features`, which leaves out the GUI.

## Architecture

```
trmnl-sim (bin)        CLI, runner (pacing, power states, commands)
├─ arch/               CPU cores behind MemBus + GuestCpu traits (riscv.rs, xtensa.rs)
├─ soc/                one module per chip, implementing the chip-agnostic Machine trait
│  ├─ esp32c3/         memory map, peripherals, crypto/GDMA, boot flow, interrupt routing
│  └─ esp32s3/         the same for the dual-core S3, plus PSRAM, LCD_CAM, USB console
├─ periph/             IP blocks shared by chips (SYSTIMER, I2C engine, SHA, AES/RSA math)
├─ board/              what's wired to the pins (trmnl_og.rs, trmnl_x.rs); Board trait
├─ devices/            UC8179 (B/W and 4-color) and parallel EPD panels, SPI NOR flash, ESP-AT modem,
│                      I2C chips (TCA9535, TPS65185, IQS323, BQ27427)
├─ hle/                ESP-IDF function replacements by ELF symbol (WiFi driver, sleep,
│                      ADC), ISA-neutral; hooks can call back into guest code
└─ firmware.rs         build artifacts, ELF symbols, OTA slot and app selection
crates/
├─ sim-api/            the contract between the emulator thread and front-ends
├─ sim-ui/             egui desktop window
├─ sim-control/        HTTP control API
└─ vnet/               user-mode router/NAT (smoltcp) + soft-AP client
```

**Chips and boards.** The build's image header names the chip (C3 or S3), and the
board follows from the chip. A core implements `GuestCpu`; the Xtensa windowed ABI
lives behind `arg`, `return_from_hook`, `alloc_scratch` and `begin_call`, so the
IDF-level HLE (WiFi, sleep, ADC) is the same code on both chips. The WiFi model
handles the struct layouts of both IDF 4.4 and 5.5. A new board is a `Board`
implementation plus its devices; a new chip is a `soc/` module.

**HLE and OTA.** Hooks are bound to addresses from `firmware.elf`. On every boot the
simulator reads the app descriptor of the slot the bootloader will start, and
matches its ELF SHA-256 against known ELFs. An OTA to a different build therefore
needs that build's ELF via `--elf`; otherwise the run halts with a clear message.

## Limitations

- TRMNL OG, BWRY and X only. Other envs need their panel and board wiring.
- No 802.11 emulation: WiFi is modelled at the ESP-IDF driver API. Signal strength,
  roaming and power-save behaviour are canned.
- TRMNL OG: no sensors on the I2C bus (all addresses NACK). No USB data, no serial
  input on either device.
- TRMNL X: touch-bar swipes and flicks (slide mode) aren't driven from the UI yet;
  taps and holds are. The accelerometer and the modem's own WiFi/BLE beyond ESP-AT
  station mode and HTTP GET aren't modelled.
- Timing is instruction-counted (1 instruction = 1 cycle), not cycle-accurate.
  Flash operations complete instantly.
- The e-paper waveform model is qualitative. Pixels are exact, but grays and ghosting
  are approximations. The 4-color panel's refresh is a fixed sequence of solid frames,
  not a waveform model.
- OTA to a different build needs that build's ELF (see above).

## Troubleshooting

- **The run halts with "CPU exception at …"**: the message has symbolized registers
  and a stack scan. `--trace fn` logs calls into a suspect function.
- **The firmware seems stuck**: `--headless --seconds N --profile` shows which
  functions the CPU spends its time in, often a polling loop on an unmodelled
  register.
- **A peripheral register isn't modelled**: run with `RUST_LOG=trmnl_sim=trace` to log
  first accesses to unmodelled registers.
- **Something hangs or crashes on the TRMNL X**: `POST /debug` (Python: `sim.debug()`)
  prints both cores' registers, a windowed-ABI backtrace (through HLE calls) and the
  running FreeRTOS task. `SIM_PEEK=addr,addr` adds memory words to that dump.
  `RUST_LOG=modem=debug` logs every AT command and modem reply.
