# trmnl-sim

A simulator for TRMNL devices that runs **unmodified compiled firmware**: the same
`bootloader.bin`, `partitions.bin` and `firmware.bin` you would flash, plus the
`firmware.elf` from the same build. You get an interactive window with the
e-paper display, the button and the battery, and an HTTP control API that
drives the device from automated integration tests, locally or in GitHub Actions.

Supported today: the **`trmnl` env (TRMNL OG: ESP32-C3, 7.5" UC8179 800×480 panel)**.
The architecture is built for more cores and boards (see [Architecture](#architecture)).

![setup screen as rendered by the simulator](tests/integration/golden/setup_screen.png)

## What is simulated

| Part | How |
|---|---|
| CPU | RV32IMC(A) interpreter, 160 MHz, cycle-counted virtual time |
| Boot | The real ESP32-C3 mask ROM code (from Espressif's ROM ELF) and your real 2nd-stage bootloader: partition table, OTA slot selection, image SHA-256 check, flash MMU |
| Peripherals | Register-level models of UART0, GPIO/IO_MUX, interrupt matrix, SYSTIMER, TIMG, RTC_CNTL, eFuse (MAC, chip rev 0.3), SPI flash controller + 4 MB NOR flash, GPSPI2, I2C master, SAR ADC one-shot, cache/MMU, RNG, SHA, AES + GDMA, RSA/MPI |
| Display | UC8179 controller driven over SPI/GPIO. Refreshes are simulated from the LUT waveforms, so full, fast and partial refreshes, 4-gray mode, flashing and BUSY timing behave like the panel. The panel `REV` read (bit-banged) returns `--panel-rev` |
| Button | GPIO2 with pull-up: presses, holds, double-clicks, and deep-sleep GPIO wake |
| Battery | ADC on GPIO3 behind the ½ divider; settable voltage |
| WiFi | The binary WiFi driver is replaced by a high-level model: scans, joins, soft-AP. lwIP, DHCP, DNS, TLS (mbedTLS on the emulated crypto accelerators), AsyncTCP and the captive portal are all the firmware's own code |
| Network | A user-mode router/NAT (`vnet`): DHCP, DNS, TCP/UDP to the internet, or `--offline` for hermetic tests. The device reaches the host machine at `10.0.2.2` |
| Sleep | Deep sleep (timer + GPIO wake, RTC memory kept, correct wake cause) and light sleep |
| Persistence | Flash is a file: WiFi credentials, API key and SPIFFS survive restarts. `--erase` gives a factory-fresh device |

## Requirements

- Rust (stable, 1.85+).
- A PlatformIO build of the firmware: `pio run -e trmnl` in `trmnl-firmware`.
- The ESP32-C3 ROM ELF from PlatformIO's `tool-esp-rom-elfs` package. It is usually
  already installed; if not, run `pio pkg install -g -t platformio/tool-esp-rom-elfs`,
  or pass `--rom path/to/esp32c3_rev3_rom.elf`.
- Python 3.9+ for the integration tests (standard library only).

## Quick start

```sh
cargo build --release
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl
```

The window shows the device. Click and hold the button on screen, or hold **Space**.
The side panel has reset, power-cycle, "wake now", WiFi in/out of range, battery
voltage, turbo and pause. The serial console is at the bottom.

**A fresh device** (`--erase`) boots into WiFi setup, like a new TRMNL. Its captive
portal is forwarded to **http://127.0.0.1:8080/**; open it in a browser, pick
**TRMNL-Sim** (any password is accepted) and choose the server. To use your own
device's account, run with its MAC: `--mac D8:3B:DA:12:34:56`.

Headless, e.g. to watch serial output or grab a screenshot:

```sh
trmnl-sim ../trmnl-firmware/.pio/build/trmnl --headless --seconds 60 --screenshot screen.png
```

Production builds only print ESP-IDF logs. Build with `-D DEV_FIRMWARE` for the
firmware's own `Log_*` output.

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
| `--rom PATH` | ESP32-C3 ROM ELF (or `$TRMNL_SIM_ROM`) |
| `--trace f1,f2` | Log every call to these firmware functions, with arguments and caller |
| `--profile` | Print where the CPU spent its time, and CPU state, on exit |
| `--scale Z` | Initial display zoom (0 = fit) |

The simulated WiFi environment has two networks: **TRMNL-Sim** (any password
works) and **Neighbors WiFi** (password `hunter2hunter2`), at −54 and −81 dBm.

### Time

The simulator keeps *virtual time* from executed cycles. By default it is paced to
wall-clock time, so the device behaves in real time. With `--turbo`, idle periods
(FreeRTOS idle, display BUSY waits, light sleep) are fast-forwarded. Turbo still runs
in real time while the host network is being waited on (an open TCP connection, a
DNS lookup) and while the setup portal is up, so no firmware timeout fires early
because of the simulator. Deep sleep always lasts its real duration unless
`--fast-sleep` is given; end it early with **Wake** or the button.

## Integration testing

```sh
scripts/integration-tests.sh                  # build the sim, run the whole suite
scripts/integration-tests.sh --build-firmware # also `pio run -e trmnl` first
scripts/integration-tests.sh test_refresh_cycle.RefreshCycle.test_button_press_wakes_and_refreshes
```

The suite ([tests/integration](tests/integration)) runs in under a minute and needs
no internet. It covers:

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

| Env var | |
|---|---|
| `TRMNL_FIRMWARE_BUILD` | Build dir to test (default `../trmnl-firmware/.pio/build/trmnl`) |
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
  It also generates BMP images and the PNG you should expect on screen.

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
- `sim.assert_screen(golden, region=(x, y, w, h))` compares against a golden PNG. It
  creates the golden if missing, and writes `*.actual.png` on mismatch.
- `ProvisionedDevice` in `tests/integration/support.py` onboards once, then boots
  copies of that flash. Tests start from a registered device in seconds.

### Control API

`--control 127.0.0.1:7878` serves JSON over HTTP; the Python client is a thin wrapper.

| | |
|---|---|
| `GET /status` | state, virtual time, display busy and refresh count, WiFi/IP, portal URL, boot count, … |
| `POST /button {"down": bool}` | hold or release the button |
| `POST /press {"ms": N}` | press for N virtual ms; returns after release |
| `POST /reset`, `/power-cycle`, `/wake`, `/quit` | |
| `POST /wifi {"available": bool}` | network in or out of range |
| `POST /battery {"mv": N}` | |
| `POST /turbo {"on": bool}`, `/pause {"on": bool}` | |
| `GET /console?since=N` | serial lines with absolute indices |
| `POST /wait {...}` | block on conditions (see above), `timeout_s`, `settle_ms`; 408 on timeout, 409 if the CPU halted |
| `GET /screenshot[?x=&y=&w=&h=]` | 8-bit grayscale PNG (0 = ink, 255 = paper) |
| `POST /screenshot/compare?tolerance=&max_ratio=[&x=&y=&w=&h=]` | PNG body in; `{"match", "diff_pixels", "diff_ratio"}` |

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
├─ arch/               CPU cores behind MemBus + GuestCpu traits (riscv.rs; xtensa next)
├─ soc/                one module per chip, implementing the chip-agnostic Machine trait
│  └─ esp32c3/         memory map, peripherals, crypto/GDMA, boot flow, interrupt routing
├─ board/              what's wired to the pins (trmnl_og.rs); chip-agnostic Board trait
├─ devices/            UC8179 panel, SPI NOR flash
├─ hle/                ESP-IDF function replacements by ELF symbol (WiFi driver, sleep,
│                      ADC), ISA-neutral; hooks can call back into guest code
└─ firmware.rs         build artifacts, ELF symbols, OTA slot and app selection
crates/
├─ sim-api/            the contract between the emulator thread and front-ends
├─ sim-ui/             egui desktop window
├─ sim-control/        HTTP control API
└─ vnet/               user-mode router/NAT (smoltcp) + soft-AP client
```

**Adding a core or chip (e.g. ESP32-S3).** Add an Xtensa LX7 core in `arch/`
implementing `GuestCpu`; its windowed ABI lives behind `arg`, `return_from_hook` and
`begin_call`, so HLE hooks don't change. Then add `soc/esp32s3/` with the S3 memory
map and peripherals (many IP blocks match the C3 ones), and a board in `board/`. The
IDF-level HLE (WiFi, sleep) is shared because the API is the same.

**HLE and OTA.** Hooks are bound to addresses from `firmware.elf`. On every boot the
simulator reads the app descriptor of the slot the bootloader will start, and
matches its ELF SHA-256 against known ELFs. An OTA to a different build therefore
needs that build's ELF via `--elf`; otherwise the run halts with a clear message.

## Limitations

- Only the TRMNL OG (`trmnl` env). Other envs need their panel, board wiring and, for
  S3-based devices, the Xtensa core.
- No 802.11 emulation: WiFi is modelled at the ESP-IDF driver API. Signal strength,
  roaming and power-save behaviour are canned.
- No sensors on the I2C bus (all addresses NACK), no USB, no serial input.
- Timing is instruction-counted (1 instruction = 1 cycle), not cycle-accurate.
  Flash operations complete instantly.
- The e-paper waveform model is qualitative. Pixels are exact, but grays and ghosting
  are approximations.
- OTA to a different build needs that build's ELF (see above).

## Troubleshooting

- **The run halts with "CPU exception at …"**: the message has symbolized registers
  and a stack scan. `--trace fn` logs calls into a suspect function.
- **The firmware seems stuck**: `--headless --seconds N --profile` shows which
  functions the CPU spends its time in, often a polling loop on an unmodelled
  register.
- **A peripheral register isn't modelled**: run with `RUST_LOG=trmnl_sim=trace` to log
  first accesses to unmodelled registers.
