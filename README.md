# trmnl-sim

A simulator for TRMNL devices that runs **unmodified compiled firmware**: the same
`bootloader.bin`, `partitions.bin` and `firmware.bin` you would flash, plus the
`firmware.elf` from the same build. You get an interactive window with the
e-paper display and the device's controls (button or touch bar, battery, dock), and
an HTTP control API that drives the device from automated integration tests, locally
or in GitHub Actions.

Supported devices, picked from the build directory's name (the PlatformIO env) or `--board`:

| PlatformIO env | Device | Chip | Display | Controls |
|---|---|---|---|---|
| `trmnl` | TRMNL OG | ESP32-C3 (RISC-V) | 7.5" 800×480, UC8179 over SPI | button |
| `trmnl_4clr` | TRMNL BWRY | ESP32-C3 (RISC-V) | 7.5" 800×480 black/white/yellow/red (GDEM075F52) | button |
| `TRMNL_X` | TRMNL X | ESP32-S3 (dual-core Xtensa LX7) + ESP32-C5 modem | 10.3" 1872×1404 parallel panel, 16 grays | touch bar (left/center/right), magnetic dock |
| `seeed_reTerminal_E1002` | Seeed reTerminal E1002 | ESP32-S3 (XIAO, 8 MB octal PSRAM) | 7.3" 800×480 Spectra 6: black/white/yellow/red/blue/green (GDEP073E01) over SPI | button |
| `trmnl_gen2` | TRMNL OG gen 2 | ESP32-C5 (RISC-V, 2.4 + 5 GHz WiFi) | 7.5" 800×480, UC8179 over SPI | button, USB power (dock switch) |
| `trmnl_gen2_4clr` | TRMNL BWRY gen 2 | ESP32-C5 | 7.5" 800×480 black/white/yellow/red | button, USB power (dock switch) |

BYOD boards (the firmware's other `device_list[]` rows; `--board` takes the `DEVICE_MODEL`):

| PlatformIO env | `--board` | Device | Chip | Display | Battery |
|---|---|---|---|---|---|
| `seeed_xiao_esp32c3` | `seeed_esp32c3` | XIAO ESP32-C3 + 7.5" panel | ESP32-C3 | 7.5" 800×480 UC8179 | none wired |
| `seeed_xiao_esp32s3` ¹ | `seeed_esp32s3` | XIAO ESP32-S3 + 7.5" panel | ESP32-S3 | 7.5" 800×480 UC8179 | none wired |
| `TRMNL_7inch5_OG_DIY_Kit` | `xiao_epaper_display` | TRMNL 7.5" DIY Kit | ESP32-S3 | 7.5" 800×480 UC8179 | ADC, switched divider |
| `TRMNL_7inch5_OG_DIY_Kit_3CLR` | `xiao_epaper_3clr` | TRMNL 7.5" BWR DIY Kit | ESP32-S3 | 7.5" 800×480 black/white/red UC8179 (two planes) | ADC, switched divider |
| `TRMNL_7inch5_OG_DIY_Kit_6CLR` | `xiao_epaper_6clr` | TRMNL 7.3" Spectra 6 DIY Kit | ESP32-S3 | 7.3" 800×480 Spectra 6 | ADC, switched divider |
| `TRMNL_4inch26_DIY_Kit` | `xiao_epaper_mini` | TRMNL 4.26" DIY Kit | ESP32-S3 | 4.26" 800×480 SSD1677 | ADC, switched divider |
| `seeed_reTerminal_E1001` | `reterminal_e1001` | Seeed reTerminal E1001 | ESP32-S3 | 7.5" 800×480 UC8179 | ADC, switched divider |
| `seeed_reTerminal_E1004` | `reterminal_e1004` | Seeed reTerminal E1004 | ESP32-S3 | 13.3" 1200×1600 Spectra 6, two controllers (CS/CS2) | ADC, switched divider |
| `seeed_sticky` | `seeed_sticky` | Seeed Sticky | ESP32-S3 | 3.97" 800×480 SSD1677, switched supply | BQ27220 |
| `xteink_x4` | `xteink_x4` | Xteink X4 | ESP32-C3 | 4.26" 800×480 SSD1677 | (not read) |
| `xteink_x3` | `xteink_x3` | Xteink X3 | ESP32-C3 | 3.68" 792×528 UC81xx | BQ27220 |
| `WAVESHARE_397` | `waveshare_397` | Waveshare ESP32-S3 3.97" | ESP32-S3 | 3.97" 800×480 SSD1677 | AXP2101 |
| `CrowPanel42` ¹ | `crowpanel42` | Elecrow CrowPanel 4.2" | ESP32-S3 | 4.2" 400×300 SSD1683, switched supply | none (4.2 V) |
| `m5_paper_mono` | `m5_paper_mono` | M5Paper Mono | ESP32-S3 | 800×480 SSD1677; supply and RST on an M5IOE1 expander | none (4.2 V) |
| `m5_paper_color` | `m5_paper_color` | M5Paper Color | ESP32-S3 | 4" 400×600 Spectra 6; supply from the PY32 PMIC | none (4.2 V) |
| `TRMNL_X_PAPERS3` | `m5_papers3` | M5Stack PaperS3 | ESP32-S3 | 4.7" 960×540 parallel (ED047TC1), 16 grays | ADC |
| `TRMNL_X_LILYGO_T5PRO` | `lilygo_t5pro` | LilyGo T5 4.7" S3 Pro | ESP32-S3 | 4.7" 960×540 parallel, EPDiy V7 (TCA9535 + TPS65185) | BQ27220 |
| `trmnl_steam` | `trmnl_steam` | TRMNL Steam | ESP32-C3 | 5.83" 648×480 UC81xx | ADC |
| `TRMNL_X_SENSORIAC5` | `sensoria_c5` | Sensoria C5 | ESP32-C5 (8 MB quad PSRAM) | 1280×720 parallel over PARLIO, 16 grays (PCA9535 + TPS65185) | (not read) |

¹ main's `platformio.ini` can't build these envs: `seeed_xiao_esp32s3` lacks `framework = arduino`
and `CrowPanel42` lacks `lib_deps` (build them from a copy of the ini with those added, and a
separate `[platformio] build_dir`: a different project config makes pio wipe `.pio/build`).
Boards with an ESP32 (classic) chip (`waveshare`, `esp32dev`) need a CPU/SoC model the
simulator doesn't have. The `esp32-c5-devkitc-1` env doesn't build (no platform override, so
PlatformIO's espressif32 6.x doesn't know the board) and has no `DEVICE_MODEL`.

![setup screen as rendered by the simulator](tests/integration/golden/setup_screen.png)

## What is simulated

Common to all devices:

| Part | How |
|---|---|
| Boot | The real mask ROM code (from Espressif's ROM ELFs) and your real 2nd-stage bootloader: partition table, OTA slot selection, image SHA-256 check, flash MMU, deep-sleep wake stubs |
| WiFi | The binary WiFi driver is replaced by a high-level model: scans, joins, soft-AP. lwIP, DHCP, DNS, TLS (mbedTLS on the emulated crypto accelerators), AsyncTCP and the captive portal are all the firmware's own code |
| Network | A user-mode router/NAT (`vnet`): DHCP, DNS, TCP/UDP to the internet, or `--offline` for hermetic tests. The device reaches the host machine at `10.0.2.2`. An NTP server at `10.0.2.123` answers with the host's clock; offline, NTP server names (containing "ntp" or starting with "time.") resolve to it, so the device knows the time without internet |
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

**TRMNL OG gen 2** (`trmnl_gen2`) and **TRMNL BWRY gen 2** (`trmnl_gen2_4clr`; the
firmware's `og_gen2` / `og_gen2_4clr` rows): the OG's panels on an ESP32-C5 board (SCK 6,
MOSI 1, CS 4, RST 2, DC 5, BUSY 0, button GPIO 3).

| Part | How |
|---|---|
| CPU | RV32IMAC interpreter with the C5's CLIC (hardware-vectored interrupts through `mtvt`, `mintthresh`/`mintstatus` levels, nesting), at the firmware's clock (240 MHz) |
| Boot | The production-silicon (v1.x) mask ROM, `esp32c5_rev100_rom.elf`, and the 2nd-stage bootloader from flash offset 0x2000 |
| Peripherals | ESP32-C5 memory map (384 KB HP SRAM, 16 KB LP SRAM kept in deep sleep), 8 or 16 MB flash and 8 MB quad PSRAM (AP Memory; probed on SPI1's CS1 by builds with `CONFIG_SPIRAM`) through SPI_MEM and the 512-entry cache MMU (with the boot-time MSPI timing tuning), PARLIO TX, interrupt matrix + CLIC, PCR clocks and resets, the low-power domain (PMU wake causes, LP_CLKRST reset causes, LP_TIMER, LP_AON), SYSTIMER, TIMG calibration, GPIO/IO_MUX (29 pins), regi2c analog registers, UART0 and the USB serial/JTAG console (merged: IDF logs to both, a line is shown once), I2C, GPSPI2, eFuse (MAC, chip v1.0, block v0.2), RNG |
| Crypto | SHA (incl. SHA-384/512), AES and AES-GCM over the AHB DMA, RSA/MPI, and the ECC (point multiplication and verification, Jacobian and modular modes; P-192/256/384) and ECDSA (signature verification) accelerators, so TLS runs on them as on the chip |
| WiFi | Dual band: besides **TRMNL-Sim** and **Neighbors WiFi**, **TRMNL-Sim-5G** (channel 36, −48 dBm, any password) joins on the C5's own radio (`WiFi-Band: 5`) |
| Battery | BQ27427 fuel gauge on I2C (SDA 23 / SCL 10; BWRY: 11 / 12); the firmware reads its voltage |
| USB power | BQ25616 charger: PG (GPIO 25) and STAT (GPIO 24), open drain, follow the dock switch (USB plugged in; charging below 4.15 V), reported as `USB-Connected` / `Battery-Charging` |
| Firmware | IDF 5.5 built from source with Arduino 3.3 (`framework = arduino, espidf`), `DEV_FIRMWARE` logging |

**Seeed reTerminal E1002** (`seeed_reTerminal_E1002`; an ESP32-S3 build that drives an SPI
panel through bb_epaper): the ESP32-S3 below with the SPI e-paper board above, wired as the
firmware's `reterminal_e1002` row (SCK 7, MOSI 9, CS 10, RST 12, DC 11, BUSY 13).

| Part | How |
|---|---|
| Display | The UC81xx model with one 4 bit/pixel image (`DTM1`, codes 0/1/2/3/5/6) and a ~19 s built-in refresh: flashes through the six colors (every 100 ms for 12 s), then the image in exact black/white/yellow/red/blue/green. **Refresh flashing** turns the flashes off |
| Button | GPIO3 with pull-up, as on the OG |
| Battery | ADC on GPIO1 behind the ½ divider, connected only while the firmware drives GPIO21 high |
| Firmware | Arduino 2.0.17 on prebuilt ESP-IDF 4.4 libraries (unlike the X's IDF 5.5): PSRAM, the WiFi glue's zero-copy transmit and the setup portal all work |

**BYOD boards.** SPI-panel boards are one data-driven board (`board/spi_epd.rs`), a row
per firmware `device_list[]` entry: pins (from `device_list[]`, or bb_epaper's `begin()` for
boards whose wiring is built into it), battery (ADC divider with optional enable pin,
BQ27220 fuel gauge, AXP2101 PMIC, or none), panel supply switching (a GPIO, the M5Paper
Color's PY32, the M5Paper Mono's M5IOE1 expander: an unpowered panel ignores its inputs and
loses its RAM) and the panel:

| Controller | How |
|---|---|
| UC81xx | The OG's UC8179 model at any size (648×480, 792×528, 400×600, 600×1600 halves), with a per-panel particle response fitted to bb_epaper's 4-gray waveforms; black/white/red panels keep two 1-bit planes (`DTM1` black/white, `DTM2` red) and flash through black/white/red during their ~16 s refresh |
| SSD16xx (SSD1677, SSD1683) | RAM windows and address counters, data entry modes, the new/old image planes, `0x22`/`0x20` update sequences with the built-in full/fast/partial waveforms and custom 4-gray LUTs (`0x32`), differential partial refreshes, deep sleep, BUSY active high |
| Two controllers (E1004) | Each half's UC81xx gets the commands sent while its chip select is low; BUSY while either is busy |
| Parallel (PaperS3, T5 Pro, Sensoria C5) | The X's parallel panel model at 960×540 on an 8-bit LCD_CAM bus: the PaperS3 powers the panel from GPIOs, the T5 Pro through a TCA9535 + TPS65185 like the X. The Sensoria C5's 1280×720 panel gets its rows over the C5's PARLIO (fed by the AHB DMA) and its power and SPV through a PCA9535's port 0 |

Firmware bugs these boards show (each is an expected-failure test):
- `trmnl_steam` reboots forever: its `device_list[]` row is inside `#ifdef CMD_CS1_CS2`,
  which its bb_epaper version doesn't define, so `pDevice` stays NULL.
- A panel-sized flip of an uncompressed BMP overruns the download buffer on panels that
  aren't 800×480 (the X3 and E1004 crash; the M5Paper Color shows garbage).
- `dpList[]` holds bb_epaper *product* ids for the boards brought up with `begin()`, which
  `setPanelType()` reads as panel types: 1-bit images never show on the CrowPanel and the
  M5Paper Mono (their 4-gray images do).
- SSD16xx boards: BMP/Group5 images update only the new-image RAM and then refresh
  differentially against a stale old-image RAM, so the old picture stays up.
- The Waveshare 3.97" picture is one row too high (bb_epaper 2.1.9 starts RAM Y at 0 while
  counting down from 479); the Sticky's 4-gray LUT is overwritten by the built-in one (0x22
  0xD7), so it shows black and white only.
- The BWR DIY kit has no black/white/red image path: 2-bit color PNGs go through the 4-gray
  planes and land in the wrong inks.
- The Xteink X4 reports 0 V (`batt_pin` 0xff although `PIN_BATTERY` is 0).
- The gen-2 BWRY (`trmnl_gen2_4clr`) defines `BOARD_TRMNL_GEN2` but not `BOARD_TRMNL_4CLR`,
  which the 4-color image path is compiled under: images are sent as two 1-bit planes that
  the panel reads as 2 bits per pixel, so only the top half changes, in the wrong inks.
- On 960 px parallel panels the setup screen's instructions overflow the width; the
  CrowPanel's 800×480 layouts don't fit 400×300.
- The Sensoria C5 never sleeps: FastEPD (8dc8c74) enables its PARLIO TX unit and on the way
  to deep sleep deletes it without disabling it; IDF 5.5 refuses (`ESP_ERR_INVALID_STATE`) and
  `ESP_ERROR_CHECK` aborts, so it reboots after every refresh.

**TRMNL X** (`TRMNL_X`):

| Part | How |
|---|---|
| CPU | Two Xtensa LX7 cores (windowed ABI, FPU, MAC16, loops, atomics) in lockstep at the firmware's CPU clock; idle cores sleep until an interrupt |
| Peripherals | ESP32-S3 memory map with 16 MB flash and 8 MB octal PSRAM behind the cache MMU, interrupt matrix, SYSTIMER, GPIO (49 pins), UART0 with a 128-byte RX FIFO and flow control, USB serial/JTAG console, I2C, GPSPI2, LCD_CAM i80 + GDMA, SHA/AES/RSA, eFuse, RTC |
| Display | EPDiy-style 1872×1404 panel: the firmware (FastEPD) clocks rows over the LCD_CAM 16-bit bus and drives SPV/CKV/LE on GPIOs; the TPS65185 PMIC supplies the rails. The panel model applies the per-frame drive to each pixel, so 1-bit and 16-gray images come out as the firmware drew them |
| Touch bar | IQS323 capacitive controller on I2C: left, center and right taps and holds (several fingers at once), with RDY as the deep-sleep (EXT0) wake source. The wake stub's bit-banged I2C read of the touch state works too |
| Dock | Placing the device on its magnetic dock powers USB and the charger: TCA9535 expander inputs (charger power-good and status), the fuel gauge's charging flag, and the `USB-Connected` / `Battery-Charging` request headers. Docking wakes a device in shipment mode |
| Battery | BQ27427 fuel gauge (voltage, state of charge, charging) |
| 5 GHz modem | ESP32-C5 running ESP-AT on UART0 at 5 Mbit/s with RTS/CTS: the factory flash over its ROM loader (`esp-serial-flasher`), then `AT+CWJAP`, SNTP, `AT+HTTPCLIENT` with custom headers. Its HTTP requests are made from the host, so 5 GHz onboarding and image downloads use the same mock servers as the 2.4 GHz path |
## Requirements

- Rust (stable, 1.85+).
- A PlatformIO build of the firmware: `pio run -e trmnl`, `-e trmnl_4clr`, `-e TRMNL_X`,
  `-e seeed_reTerminal_E1002`, `-e trmnl_gen2` and/or `-e trmnl_gen2_4clr` in `trmnl-firmware`. The TRMNL X build also needs its `littlefs.bin` (factory images
  and the modem firmware); its post-build script downloads it into the build dir.
- The chips' mask ROM ELFs. The ESP32-C3 and ESP32-S3 ones come with PlatformIO's
  `tool-esp-rom-elfs` package (usually already installed; else
  `pio pkg install -g -t platformio/tool-esp-rom-elfs`). The ESP32-C5's production silicon
  (v1.x) has a different ROM than the v0.x samples in that package: `esp32c5_rev100_rom.elf`,
  from Espressif's [esp-rom-elfs](https://github.com/espressif/esp-rom-elfs/releases) release
  20260528. `scripts/fetch-rom-elfs.sh` (run by `bin/setup`) downloads and checks it into
  `rom/`. The simulator looks for a ROM ELF in `rom/` of its checkout, `rom/` next to the
  executable, `~/.cache/trmnl-sim/rom-elfs` (`$XDG_CACHE_HOME`), then PlatformIO's
  `tool-esp-rom-elfs` packages; `--rom path/to/rom.elf` (or `TRMNL_SIM_ROM`) overrides it.
- Python 3.9+ for the integration tests (standard library only).

## Quick start

```sh
bin/setup      # install Rust (rustup), a C toolchain, Python 3, the ESP32 ROM ELFs
bin/build      # cargo build --release
bin/dev        # build, then run the OG build from ../trmnl-firmware
bin/dev bwry   # ... the trmnl_4clr build;  bin/dev x  for TRMNL_X, bin/dev gen2 for trmnl_gen2
bin/dev x --erase   # extra arguments go to trmnl-sim; TRMNL_FIRMWARE=<checkout> to use another one
bin/test       # fmt, clippy and unit tests
bin/spec       # integration tests (bin/spec test_trmnl_x for a subset)
```

Or by hand:

```sh
cargo build --release
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl     # TRMNL OG
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl_4clr  # TRMNL BWRY
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/TRMNL_X   # TRMNL X
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/seeed_reTerminal_E1002  # reTerminal E1002
./target/release/trmnl-sim ../trmnl-firmware/.pio/build/trmnl_gen2   # TRMNL OG gen 2 (ESP32-C5)
```

The window shows the device. On the OG, click and hold the button on screen, or hold
**Space**. On the X, tap the touch bar under the screen, or use **←/↓/→** (or
**1/2/3**) for left/center/right; hold them for holds. The side panel has reset,
power-cycle, "wake now", [save points](#save-points), WiFi in/out of range, battery
voltage, turbo, pause and, on the X, the dock. The serial console is at the bottom.

**A fresh device** (`--erase`) boots into WiFi setup, like a new TRMNL. Its captive
portal is forwarded to **http://127.0.0.1:8080/**; open it in a browser, pick
**TRMNL-Sim** (any password is accepted) and choose the server. To use your own
device's account, run with its MAC: `--mac D8:3B:DA:12:34:56`.

### Fault injection

The simulator can inject faults that are hard to produce on hardware. The side panel's
**Faults** section has the common ones: internet down, no internet behind the access
point, a slow (300 ms, 16 kB/s) or lossy (10%) link, failing DNS, a power cut in the
middle of the next NVS write, a missing fuel gauge (X), a stuck panel, and an unresponsive
modem (X). The [control API](#control-api) (`POST /faults`), `--faults JSON` and the Python
client (`sim.set_faults(...)`) have the full set:

| Fault | JSON (`POST /faults`, `--faults`) | |
|---|---|---|
| Latency | `{"net": {"latency_ms": 300}}` | added to every packet towards the device |
| Packet loss | `{"net": {"loss": 0.1}}` | each packet, each direction |
| Bandwidth | `{"net": {"bandwidth_bps": 16000}}` | bytes per second, each direction |
| DNS failure | `{"net": {"dns": "servfail"}}` | or `nxdomain`, `empty` (no addresses), `timeout` (no answer) |
| No internet | `{"net": {"no_internet": true}}` | WiFi and DHCP work; DNS and all connections (even to 10.0.2.2) time out |
| Internet down | `{"net": {"offline": true}}` | only the host is reachable, like `--offline` |
| Cut connections | `{"net": {"tcp_cut": {"after_bytes": 20000, "stall": false, "port": 8080}}}` | TCP connections opened afterwards get a RST (or with `stall`, silently stop) after N bytes towards the device; `port` is optional |
| Power loss | `{"power_loss": {"partition": "nvs", "op": "program", "nth": 3, "cut": "torn"}}` | cut power at the `nth` flash `op` (`any`, `program`, `erase`) in a partition (label, or `ota_0`/`ota_1`/`littlefs`) and/or `"range": [start, end]`, then restore it (like Power-cycle). `cut`: `before` (nothing written), `torn` (half the page program / half the erase lands, as on real NOR flash) or `after`. One-shot |
| I2C device absent | `{"i2c_absent": [85]}` | NACKs everything (X: `0x55` fuel gauge, `0x44` touch bar, `0x20` expander, `0x68` PMIC) |
| Panel stuck | `{"panel_busy_stuck": true}` | OG/BWRY: BUSY held low; X: the PMIC never reports power good |
| Modem unresponsive | `{"modem_unresponsive": true}` | X: the modem ignores AT commands (its ROM loader still works) |
| Modem AT errors | `{"modem_at_errors": ["AT+CWMODE"]}` | X: the modem answers ERROR to AT commands starting with one of these |
| Chip temperature | `{"chip_temp_c": 60}` | the on-chip sensor (ESP32-C3, -S3, -C5) reads this die temperature instead of 25 °C |
| Fuel gauge | `{"gauge_reset": true}` | X: the BQ27427 has a power-on reset (once, when set, as when the battery is disconnected): factory data memory, ITPOR set |
| Touch bar | `{"touch_bar": "reset"}` | X: the IQS323 resets on its own (once, when set; config lost, SHOW_RESET), `"lockup"`: its lock-up check register stops reading 0xEE, `"ati_error"`: ATI_ERROR until the next re-ATI |

`POST /faults` merges into the current faults (`null` clears one, also inside `net`);
`DELETE /faults` clears all. Faults are the environment, not device state: save points
don't record them and they stay in effect across a restore. Network faults apply to the S3/C3's own WiFi (the `vnet`
router) and, on the TRMNL X's 5 GHz path, to the modem's host-side HTTP requests: DNS
failure and no internet fail the request (after the modem's 10 s timeout where it would
wait), latency delays the response, the bandwidth limit throttles the body, and a cut
truncates the body after N bytes (a stall times out after 30 s); packet loss isn't applied
there. HTTP-level faults (500s, malformed JSON, truncated or slow bodies, timeouts) are set
per path on the Python mock server: `mock.set_fault("/api/display", status=500)`.

`[sim] power lost: program #3 at 0xa060 (+0x40) in partition nvs, cut torn` marks a
power cut on the console; `GET /faults` also lists the partition table and counts flash
programs and erases (to pick an `nth`). `RUST_LOG=flash=trace` logs every program/erase.

### Built-in mock server

To drive the device's content yourself, without an account on trmnl.app, use the
simulator's built-in TRMNL server: run with `--mock-server` (port 8090; `--mock-server=0`
picks a free one) or start it from the **🖧 Mock server panel** button in the side panel.
The panel on the left shows the URL the device should use, `http://10.0.2.2:8090`, with a
copy button.

- **Onboarding.** With a fresh device in WiFi setup, **Connect it to this server** fills in
  the setup page for you (TRMNL-Sim and the server URL). **Onboard the device here…** on an
  already onboarded OG holds the button for 6 s so it forgets its WiFi, then does the same
  once the portal is up (the API key is kept). You can also type the URL into the setup
  page's server field yourself. Keep the port fixed: the device remembers the URL.
- **Images.** Drop PNG, JPEG, BMP or GIF files on the window, or use **Add images…**.
  They are resized (contain, cover or stretch) and dithered to what the panel takes: an
  800×480 1-bit BMP on the OG, an 800×480 2-bit black/white/yellow/red palette PNG on the
  BWRY (without dithering, colors are classified like the firmware does), a 1872×1404
  4-bit gray PNG on the X. Click an image to serve it. A built-in default image is served
  until you add your own. Each version of an image gets a server-style
  `plugin-<id>-<timestamp>` filename, so the X's image cache behaves as with trmnl.app.
- **Playlist.** Mark images with ☰; with **Next image on every request** each
  `/api/display` serves the next one. Previous/Next step through it by hand.
- **Display response.** Refresh rate, the double-click action (`special_function`:
  identify shows the device's friendly ID, rewind goes back one playlist entry,
  restart_playlist goes to the first), full refresh (`maximum_compatibility`), the touch
  bar mode on the X, and under Advanced: `/api/setup` registration, the response
  `status` (202, 500) and the friendly ID.
- **Next request only.** **Firmware update** sends `update_firmware` with a URL on this
  server: this build's `firmware.bin`, or a file you choose (booting another build needs
  its ELF, see `--elf`). **Reset device** sends `reset_firmware`.
- **HTTP faults.** The failures of the firmware's `scripts/mock_server.py`, queued per
  route (`/api/display` or image downloads) and used up in order, one request each (a
  count of "always" fails every request until removed). Both routes: any HTTP status,
  `timeout` (send nothing for N s, then close), `reset` (TCP RST), `close` (close without
  a byte), `redirect` (307/308 back to the same path). `/api/display`: `bad-json`,
  `status` (JSON `"status": N`; 500 wipes the device's credentials), `empty-state`.
  Images: `truncate` (the full Content-Length, fewer bytes), `slow` (stall after N bytes),
  `empty`, `too-big`, `garbage`, `no-length`, `wrong-type`. Hover a kind for the firmware
  error it should cause. Turbo runs in real time while a connection is open, so a
  `timeout` or `slow` stall lasts as long for the firmware as it says.
- **Wake the device on changes** ends a deep sleep when you pick an image or queue an
  action, so you see it right away.
- **Requests** lists every request the device made (time in UTC, method, path, status,
  wake source, battery, RSSI, firmware version, and what was served); hover for all
  headers and the body.

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
| `--offline` | Hermetic network: only the host (`10.0.2.2` → `127.0.0.1`) and the built-in NTP server are reachable |
| `--dns NAME=IP` | Answer DNS for NAME locally (repeatable) |
| `--portal-port N` | Host port forwarded to the captive portal (default 8080, 0 = any free port) |
| `--mock-server[=PORT]` | Start the [built-in mock server](#built-in-mock-server) (default port 8090, 0 = any free port); the device URL is `http://10.0.2.2:PORT` |
| `--elf PATH` | Extra firmware ELFs the device may boot after an OTA (repeatable) |
| `--seconds S` | Stop after S seconds of virtual time |
| `--screenshot PNG` | Save the display when the run ends |
| `--panel-rev HEX` | Value returned by the panel's REV command (sent as the `Panel-Rev` header) |
| `--rom PATH` | ROM ELF for the build's chip (or `$TRMNL_SIM_ROM`) |
| `--trace f1,f2` | Log every call to these firmware functions, with arguments and caller |
| `--profile` | Print where the CPU spent its time, and CPU state, on exit |
| `--coverage FILE` | Record which firmware instructions run; write an lcov tracefile on exit (see [Code coverage](#code-coverage)) |
| `--coverage-root DIR` | Write source paths under DIR relative to it (default: the firmware checkout of `<build_dir>`) |
| `--coverage-include P,..` | Only report source files whose path starts with one of these, e.g. `src/,lib/` |
| `--memcheck[=halt]` | Check the firmware's [memory use](#memory-checking): report heap errors and carry on, or (`=halt`) stop at the first one |
| `--memcheck-suppress F,..` | Tolerate known memory bugs: ignore violations with one of these functions in their stacks |
| `--scale Z` | Initial display zoom (0 = fit) |
| `--restore FILE` | Start from a [save point](#save-points) instead of booting. Its flash replaces the `--flash` image (and its MAC, `--mac`) |
| `--board NAME` | The board, as the firmware's `DEVICE_MODEL` (`og`, `xteink_x4`, `m5_papers3`, ... ; `x` for the TRMNL X) or PlatformIO env. Default: the build directory's name, else what the firmware links (OG / BWRY / reTerminal E1002 / X) |
| `--sensor NAME` | Environment sensor on an SPI-panel board's I2C header (repeatable): `scd41` (CO2, 0x62), `aht20` (temperature/humidity, 0x38) |
| `--wifi-networks JSON` | Access points in range of the device's own radio, replacing the defaults, e.g. `'[{"ssid":"TRMNL_QA","rssi":-40},{"ssid":"Home","password":"pw"}]'` (keys: `ssid`, `password` (null: any), `rssi`, `channel` (1-14, or 32-177 for 5 GHz, seen by the ESP32-C5 and the X's modem), `open`, `internet`) |
| `--faults JSON` | Inject [faults](#fault-injection) from the start, e.g. `'{"power_loss":{"partition":"nvs"}}'` (repeatable, merged) |

The simulated WiFi environment has two networks: **TRMNL-Sim** (any password
works) and **Neighbors WiFi** (password `hunter2hunter2`), at −54 and −81 dBm. The
TRMNL X modem also sees **TRMNL-Sim-5G** (channel 36, −48 dBm, any password); joining
it makes the firmware do all its HTTP through the modem. The ESP32-C5's own radio is dual
band: it sees TRMNL-Sim-5G too and joins it directly. Access points on channels 36 and up
are invisible to the 2.4 GHz-only C3 and S3.

### Time

The simulator keeps *virtual time* from executed cycles. By default it is paced to
wall-clock time, so the device behaves in real time. With `--turbo`, idle periods
(FreeRTOS idle, display BUSY waits, light sleep) are fast-forwarded. Turbo still runs
in real time while the host network is being waited on (an open TCP connection, a
DNS lookup, a modem HTTP request) and while the setup portal is up (unless its client
is sent away: `POST /wifi {"portal_client": false}`), so no firmware timeout fires early
because of the simulator. A light sleep with no timer armed
(shipment mode, waiting for the dock) also runs in real time. Deep sleep always lasts its real duration unless
`--fast-sleep` is given; end it early with **Wake** or the button.

### Save points

A save point captures the device so you can jump straight back to e.g. "onboarded, asleep,
image X showing" instead of redoing setup. In the side panel, **Save** keeps one in memory
(listed below it, **⟲** restores it), **Save as…** also writes a `.trmnlsave` file, and
**Open…** restores a file. The same is available as `POST /savepoint` / `POST /restore`,
from Python, and with `--restore FILE` on the command line.

- **Taken in deep sleep** (the useful case), it holds everything that survives deep sleep
  on the device plus what the simulator needs to resume it: flash, RTC memory, the RTC_CNTL
  registers, the S3 cache MMU, virtual time, boot count, the pending wake (timer and GPIO
  mask), the e-paper image and controller RAM (UC8179 image/LUT registers, the parallel
  panel's particle state, BWRY colors), the I2C chips (IQS323 touch configuration for touch
  wake, BQ27427 data memory, TCA9535, TPS65185), the modem's flashed image, battery voltage,
  dock and WiFi availability. Restoring it puts the device back into that sleep with the same
  time left; it wakes by timer, button or touch as it would have.
- **Taken at any other time** (running, light sleep, halted), it holds only what survives
  pulling the battery: flash, the screen and the modem's flash. Restoring it powers the device
  on from there.

Files are zlib-compressed (about 1 MB for an OG, 2.5 MB for an X) and record the firmware
build: restoring onto a different build is refused with an error, since the flash holds that
build's app and the simulator's hooks follow its ELF. Taking one while the display refreshes
is refused too; try again when it is idle.

## Integration testing

```sh
scripts/integration-tests.sh                  # build the sim, run the whole suite
scripts/integration-tests.sh --build-firmware # also `pio run` for the tested envs first
scripts/integration-tests.sh test_refresh_cycle.RefreshCycle.test_button_press_wakes_and_refreshes
scripts/integration-tests.sh test_trmnl_x     # only the TRMNL X tests
```

How much runs:

| Command | Runs |
|---|---|
| `bin/spec` | every device's own tests; the general tests (setup, portal, WiFi, HTTP, images, errors, faults, OTA, save points, special functions...) in full on the TRMNL OG; and on every other device a smoke test per area (`SMOKE` in [devices.py](tests/integration/devices.py)): portal, onboarding, identity, battery, image, timer and button wake, OTA, HTTPS, a server error, an error screen, a save point, a special function |
| `bin/spec --comprehensive` | the same, but the general tests in full on one device per family (`FAMILIES`: devices sharing chip, panel controller and inks, e.g. ESP32-S3 + SSD16xx; `bin/spec --list-envs` marks them with `*`) |
| `bin/spec --exhaustive` | the general tests in full on every device |
| `bin/spec <env>` | everything for one device: its own tests and all the general tests |

`--dry-run` prints the test groups a command would run. Tests marked `@slow` (support.py;
e.g. the screen wiper, 100 full refreshes) are skipped unless `--slow` is given.

To test one device, name its PlatformIO environment (`bin/spec` is the same script):

```sh
bin/spec xteink_x4                    # every test that runs the xteink_x4 build
bin/spec TRMNL_X trmnl_gen2           # several environments (names are case-insensitive)
bin/spec --build-firmware xteink_x4   # pio run -e xteink_x4 first, then its tests
bin/spec --list-envs                  # the environments with tests, test counts, builds present
```

Every test class declares the environment whose build it runs: `ENV = "<env>"` on the
class, or on its module for all of its classes (BYOD board classes already set `ENV`). The
runner refuses to select by environment while any class lacks one, and fails up front if the
requested environment hasn't been built.

The suite ([tests/integration](tests/integration)) runs in about two minutes and needs
no internet. For the TRMNL OG it covers:

- the first-boot setup screen and captive portal, and the portal's 15-minute timeout;
- factory QA near a `TRMNL_QA` network (every build but the X's): pass, fail on an
  overheating chip, stopped by the button (needs the firmware's QA fix; see
  [test_errors.py](tests/integration/test_errors.py));
- onboarding, and WiFi failures (unknown SSID, wrong password);
- `/api/setup` and `/api/display` requests and their headers, including the `Panel-Rev`
  read from the panel;
- HTTPS ([test_https.py](tests/integration/test_https.py)), and for trmnl.app the TLS
  session resumed across deep sleep;
- pixel-exact image rendering;
- sleep duration from `refresh_rate`;
- timer and button wake sources;
- battery reporting;
- persistence across power cycles;
- long-press WiFi reset;
- WiFi out of range;
- a full **OTA update** into the second app slot, and booting it.
- save points: restoring a sleeping device in a new simulator (same screen, timer and
  button wake, no re-onboarding), in-memory slots, power-off save points, and refusing
  other builds and bad files.

For the TRMNL BWRY ([test_trmnl_bwry.py](tests/integration/test_trmnl_bwry.py); skipped if
there is no `trmnl_4clr` build): the device identity (`Model: og_4clr`), a 4-color image
rendered exactly (compared as RGB), the panel's long refresh, and a save point keeping the
color image.

For the Seeed reTerminal E1002 ([test_reterminal_e1002.py](tests/integration/test_reterminal_e1002.py);
skipped if there is no `seeed_reTerminal_E1002` build): the setup screen (the OG's
goldens), onboarding, the device identity (`Model: reterminal_e1002`) and switched battery
divider, every PNG pixel format (1/2/4/8-bit gray and palette, truecolor with and without
alpha) reduced to the six inks exactly as the firmware does, the long refresh, button wake,
and a save point keeping the color image.

For the gen-2 OG and BWRY ([test_og_gen2.py](tests/integration/test_og_gen2.py), a class
each; skipped without a `trmnl_gen2` / `trmnl_gen2_4clr` build in `TRMNL_FIRMWARE_BUILDS`):
the BYOD checks (onboarding through the portal, identity headers, a served image), the
fuel gauge's voltage, `USB-Connected`/`Battery-Charging` from the charger lines, timer and
button wake, deep-sleep and power-off save points, onboarding on 5 GHz with the C5's own
radio (`WiFi-Band`), HTTPS on the crypto accelerators, and memcheck and coverage runs; the
BWRY's colors and the long refresh (its image bug is an expected failure).

For the Sensoria C5 ([test_byod_parallel.py](tests/integration/test_byod_parallel.py)): the
setup screen, onboarding and a 16-gray ramp on its 1280×720 panel, and its reboot on the
way to sleep (an expected failure, see above).

For the BYOD boards ([test_byod_uc8179.py](tests/integration/test_byod_uc8179.py),
[test_byod_uc81xx.py](tests/integration/test_byod_uc81xx.py),
[test_byod_ssd.py](tests/integration/test_byod_ssd.py),
[test_byod_m5.py](tests/integration/test_byod_m5.py),
[test_byod_parallel.py](tests/integration/test_byod_parallel.py); a class per board, skipped
if its env isn't built): every board onboards through the portal and is checked for its
`Model`/`Width`/`Height`/`Battery-Voltage` headers and a served image shown exactly
([support_byod.py](tests/integration/support_byod.py)); plus per board 4-gray and 16-gray
images, partial refreshes, button wake, battery from the gauge or PMIC, colors, each E1004
controller's half, and the firmware bugs listed [above](#what-is-simulated).

For the TRMNL X ([test_trmnl_x.py](tests/integration/test_trmnl_x.py); skipped if there is
no `TRMNL_X` build):

- the factory flow: modem flashing, then shipment mode until docked;
- onboarding on 2.4 GHz (the S3's own radio) and on 5 GHz (through the modem);
- request headers, including `Width`/`Height`/`Model`, the RSSI of the radio in use,
  `USB-Connected`/`Battery-Charging` on and off the dock, and the fuel gauge's readings
  next to the voltage-based estimate;
- an unattended portal timing out back into shipment mode;
- pixel-exact 1-bit PNGs and a 16-level 4-bit gray ramp on the 1872×1404 panel;
- sleep duration;
- a center tap waking the device (`Update-Source: EXT0`);
- a left tap showing the previous cached image without touching the network;
- the touch bar ([test_touchbar_x.py](tests/integration/test_touchbar_x.py)): browsing with
  taps, holds and (slide mode) swipes, the WiFi-reset and power-off confirmations, and
  switching between tap and slide mode;
- a save point restored in a new simulator: identical screen, dock state, and a touch wake
  refreshing over 5 GHz.

`bin/spec TRMNL_X` also runs the general tests on the X (onboarded on 2.4 GHz). Their
factory-fresh device is an *unboxed* X (shipped, then docked once: it restarted into the
setup portal; `support_x.unboxed`), and their button presses are its touch bar gestures
(`support_x.XSim`): a short press is a tap in the middle, a 5 s press the WiFi reset (both
edges, then a middle hold). The OG's double click and 15 s press have no equivalent there,
so those tests are skipped (`@needs("double_click")`, `@needs("soft_reset_press")`), as is
what the X doesn't have (factory QA, sensors, Panel-Rev). Its goldens are in
[golden/TRMNL_X](tests/integration/golden/TRMNL_X) (`support.golden`).

Fault injection ([test_faults.py](tests/integration/test_faults.py) on the device under test,
[test_faults_x.py](tests/integration/test_faults_x.py) on the X): HTTP 500 and malformed
JSON from `/api/display`; truncated, reset and stalled image downloads (also on the X's
modem path); slow, high-latency and lossy links; DNS failure; an access point without
internet; power loss mid-write in NVS (torn pages), in otadata and during an OTA (the old
firmware keeps booting); a stuck panel or failed PMIC; a missing fuel gauge, or one that
loses its configuration (the golden file is rewritten and the current's sign fixed); an
unresponsive modem.

| Env var | |
|---|---|
| `TRMNL_FIRMWARE_BUILD` | TRMNL OG build dir (default `../trmnl-firmware/.pio/build/trmnl`) |
| `TRMNL_BWRY_BUILD` | TRMNL BWRY build dir (default `../trmnl-firmware/.pio/build/trmnl_4clr`) |
| `TRMNL_X_BUILD` | TRMNL X build dir (default `../trmnl-firmware/.pio/build/TRMNL_X`) |
| `TRMNL_E1002_BUILD` | reTerminal E1002 build dir (default `../trmnl-firmware/.pio/build/seeed_reTerminal_E1002`) |
| `TRMNL_FIRMWARE_BUILDS` | Where the BYOD and gen-2 boards' builds are, one directory per env (default `../trmnl-firmware/.pio/build`) |
| `TRMNL_SIM_REALTIME=1` | Run the tests without turbo |
| `TRMNL_SIM_UPDATE_GOLDEN=1` | Rewrite golden screenshots from this run |
| `TRMNL_SIM_ARTIFACTS=DIR` | Save every simulator's log and final screen here |
| `TRMNL_SIM_MEMCHECK=1` | Run every simulator with [`--memcheck=halt`](#memory-checking); a test fails on any memory error |
| `TRMNL_SIM_COVERAGE=DIR` | Record firmware code coverage in every simulator; merge and report it after the run (see [Code coverage](#code-coverage)) |
| `TRMNL_SIM_NETWORK=1` | Also run tests against the real trmnl.app |
| `TRMNL_SIM_BIN` | Simulator binary (default `target/release/trmnl-sim`) |

### Writing tests

Give every new test module (or class) an `ENV` naming the PlatformIO environment it runs,
so `bin/spec <env>` picks it up.

Two standard-library Python modules live in [python/](python):

- **`trmnl_sim.Simulator`** launches a headless simulator with the control API and
  wraps every action.
- **`trmnl_mock.MockTrmnl`** is a fake TRMNL API server. It serves `/api/setup`,
  `/api/display`, `/api/log`, images and firmware files, and records every request.
  It also generates BMP images (OG), 1/2/4/8-bit gray PNGs (`set_png`, X) and 4-color
  palette PNGs (`set_color_png`, BWRY; `set_spectra6_png`, 4-bit, reTerminal E1002; like the TRMNL server, colors are reduced to the
  panel's four first, since the OG-family PNG decoder can't take 800 px truecolor rows), with
  server-style `plugin-<id>-<timestamp>` filenames the X uses for its image cache, and
  returns the PNG you should expect on screen. Request header lookups are
  case-insensitive. `set_fault(path, status=, body=, delay=, hang=, truncate=, rate=,
  close=, times=)` makes a path (or a `prefix*`) misbehave: an HTTP error, a malformed
  body, a timeout, a body cut short, a slow download or a dropped connection;
  `device_host` lets the device reach it by a name (with `--dns NAME=10.0.2.2`).
  `MockTrmnl(tls=True)` serves HTTPS (TLS 1.2, ECDHE-ECDSA with a throwaway P-384
  certificate made with `openssl`); each request's `tls_resumed` says whether its
  connection resumed an earlier TLS session.
- **`sim.mock`** (`trmnl_sim.BuiltinServer`) drives the simulator's
  [built-in server](#built-in-mock-server) instead, so no second server is needed:
  `start()` returns the device URL, `add_image(name, png_or_jpeg_bytes, current=True)`
  converts an image for the panel and `expected(name)` returns the PNG the screen should
  then show; `display(refresh_rate=..., image=..., special_function=..., playlist=...,
  extra={...})`, `queue(update_firmware=True, firmware_url=...)`,
  `faults(display=["503:2"], image=["truncate:1"])` / `clear_faults()`, `set_file(path, bytes)`,
  `requests()`, `count(path)` and `wait_for_request(path, after=, timeout_s=)` work like
  their `MockTrmnl` counterparts. See
  [test_builtin_server.py](tests/integration/test_builtin_server.py).

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
- `sim.save_point(path=None, label=None)` takes a save point (into memory, and to `path`
  if given); `sim.restore(path)` or `sim.restore(id=N)` restores one, `sim.save_points()`
  lists the in-memory ones, and `Simulator(BUILD, restore=path)` starts from a file. See
  [test_savepoints.py](tests/integration/test_savepoints.py).
- `ProvisionedDevice` in `tests/integration/support.py` onboards once, then boots
  copies of that flash. Tests start from a registered device in seconds.
  `tests/integration/support_x.py` does the same for the X: `ShippedX` is a device fresh
  from the factory (QA done, modem flashed, in shipment mode) and `ProvisionedX`
  onboards a copy of it on 5 GHz (or `ssid=SSID_24`).
- `sim.set_faults(net={...}, power_loss={...}, ...)`, `sim.set_net_faults(dns="servfail")`,
  `sim.arm_power_loss("nvs", cut="torn")`, `sim.clear_faults()` and `sim.faults()` inject
  [faults](#fault-injection); `Simulator(..., faults={...})` starts with them.
- `Simulator(..., memcheck="halt")` runs under the [memory checker](#memory-checking);
  `sim.memcheck()` returns its report and `sim.assert_no_memory_errors()` fails on
  violations (also done when the `with` block ends).

### Control API

`--control 127.0.0.1:7878` serves JSON over HTTP; the Python client is a thin wrapper.

| | |
|---|---|
| `GET /status` | state, virtual time, display busy and refresh count, WiFi/IP, portal URL, boot count, … |
| `POST /button {"down": bool}` | hold or release the button |
| `POST /press {"ms": N}` | press for N virtual ms; returns after release (`"count": 2, "gap_ms": 150`: a double click, timed in virtual ms) |
| `POST /touch {"zone": "left"\|"center"\|"right", "ms": N}` | TRMNL X touch bar tap; returns after lift (`"down": true/false` instead: hold / lift a finger, several may be down) |
| `POST /gesture {"gesture": "swipe_next"\|"swipe_back"\|"flick_next"\|"flick_back"}` | TRMNL X slide along the touch bar (reported in slide mode only) |
| `POST /dock {"docked": bool}` | TRMNL X magnetic dock |
| `POST /reset`, `/power-cycle`, `/wake`, `/quit` | |
| `POST /wifi {"available": bool}` | network in or out of range; `{"networks": [...]}` replaces the access points in range (as `--wifi-networks`); `{"portal_client": false}` keeps the host's portal client off the setup access point, so an unattended portal runs ahead of wall-clock time in turbo mode (e.g. to its 15-minute timeout) |
| `POST /battery {"mv": N}` | |
| `POST /turbo {"on": bool}`, `/pause {"on": bool}` | |
| `POST /debug` | dump CPU state (registers, backtrace, current FreeRTOS task) and board diagnostics to the console |
| `GET /console?since=N` | serial lines with absolute indices |
| `POST /wait {...}` | block on conditions (see above), `timeout_s`, `settle_ms`; 408 on timeout, 409 if the CPU halted |
| `GET /screenshot[?x=&y=&w=&h=]` | 8-bit grayscale PNG (0 = ink, 255 = paper); RGB on color panels |
| `POST /screenshot/compare?tolerance=&max_ratio=[&x=&y=&w=&h=]` | PNG body in; `{"match", "diff_pixels", "diff_ratio"}`; on color panels a pixel differs if any channel is off by more than `tolerance` |
| `GET /mock` | built-in mock server state: running, `device_url`, images, current image, playlist, settings, queue, request count |
| `POST /mock/start {"port": N}`, `/mock/stop` | start (0 = any free port; returns `device_url`) or stop it |
| `POST /mock/images?name=&current=1&dither=0&fit=contain\|cover\|stretch&raw=1` | image file body (PNG/JPEG/BMP/GIF), converted for the panel (`raw=1`: a PNG/BMP served as is) |
| `GET /mock/images/NAME/expected`, `DELETE /mock/images/NAME` | the PNG the screen should show for it; remove it |
| `POST /mock/display {...}` | `image`, `refresh_rate`, `special_function`, `playlist`, `auto_advance`, `registered`, `friendly_id`, `api_key`, `extra` (raw `/api/display` fields; `null` removes) |
| `POST /mock/queue {...}`, `DELETE /mock/queue` | raw fields (and `image`) for the next `/api/display` answer only |
| `POST /mock/faults {"display": [...], "image": [...]}`, `DELETE /mock/faults[?route=display\|image]` | append HTTP and connection failures to a route's queue, in `scripts/mock_server.py`'s syntax `KIND[=ARG][:COUNT]` (e.g. `"503:2"`, `"timeout=20"`, `"reset:1"`, `"slow=1024,20"`; no count: until cleared); returns both queues, also in `GET /mock` as `faults` |
| `POST /mock/files?path=/x.bin` | serve the body at that path; returns its device URL |
| `GET /mock/requests?since=N` | recorded device requests: method, path, headers, body, status, summary, `sim_time_s` |
| `POST /savepoint {"path"?: str, "label"?: str}` | take a [save point](#save-points) (also written to `path`, absolute or relative to the simulator's cwd); `{"ok", "savepoint": {"id", "label", "deep_sleep", "sim_time_s", "wake_at_s", "path", "bytes"}}`, 409 if refused |
| `POST /restore {"path": str}` or `{"id": N}` | restore from a file or an in-memory save point; 409 on failure (e.g. another firmware build) |
| `GET /savepoints` | the in-memory save points |
| `POST /coverage {"path": "x.info", "reset": bool}` | with `--coverage`: write the lcov tracefile now (default path: the `--coverage` file), then optionally start over; returns `lines_found`/`lines_hit`/`functions_found`/`functions_hit`/`files` |
| `GET /memcheck` | with `--memcheck`: `violations` and `suppressed` (kind, address, pc, task, backtrace, block with allocation/free stacks, `report` lines), `heap` (allocs, frees, live/peak bytes for internal RAM and PSRAM), `stacks` (per task: size, `min_free`, `low`); `{"enabled": false}` otherwise |
| `GET /faults` | injected faults, `summary`, `power_losses`, flash `programs`/`erases`, the partition table |
| `POST /faults {...}`, `DELETE /faults` | merge [faults](#fault-injection) into the current ones; clear them all |

### Code coverage

`--coverage FILE` records every instruction address the firmware executes (a bitmap
over the app ELF's code sections, kept across resets and deep sleeps) and, when the
run ends, maps it to source lines through the ELF's DWARF line tables and writes an
[lcov](https://github.com/linux-test-project/lcov) tracefile: a line is hit if any
instruction attributed to it ran (inlined code counts for the line it came from), and
listed with count 0 if it has code that never ran. Functions come from the symbol
table (`FN`/`FNDA`, demangled). Counts are 0/1 per run. Paths of the firmware's own
sources (`src/`, `lib/`, `.pio/libdeps/`) are relative to its checkout; framework and
IDF sources keep their absolute paths.

```sh
trmnl-sim ../trmnl-firmware/.pio/build/trmnl --headless --seconds 30 --coverage og.info --coverage-include src/,lib/
```

`POST /coverage` (Python: `sim.write_coverage(path, reset=False)`) writes a tracefile
mid-run, e.g. to see what one step of a test covers. After an OTA to another build
(`--elf`), both builds' lines are reported, merged by file and line.

`TRMNL_SIM_COVERAGE=DIR bin/spec` makes every simulator the tests start write
`DIR/<test>-*.info`. At the end, `run.py` merges them into `DIR/merged.info` and an
HTML report in `DIR/html/`, and prints the coverage of the firmware's `src/` and
`lib/`. [scripts/coverage.py](scripts/coverage.py) (standard library only) does the
merging and reporting on its own:

```sh
scripts/coverage.py DIR --include src/ --include lib/            # per-file table and total
scripts/coverage.py DIR -o all.info --html cov-html --root ../trmnl-firmware
genhtml all.info -o cov-html                                     # lcov's report, if installed
```

Recording costs roughly 5-8% of emulation speed (about 1-3% when off); the report
takes a fraction of a second. Not covered: the 2nd-stage bootloader and mask ROM code (no
line tables for them), and functions replaced by [HLE](#architecture) hooks (their
guest code never runs, so they are left out of the report rather than counted as
missed). A simulator killed rather than quit writes no tracefile.

### Memory checking

`--memcheck` finds memory bugs in the firmware as they happen, with symbolized stacks,
instead of waiting for them to crash it:

- **heap-use-after-free**, **heap-buffer-overflow** (past the end of a block, into its
  successor's header, or before it), accesses to never-allocated heap or allocator
  metadata, and **stack-overflow** below the running task's stack;
- **double-free** and **invalid-free** (a pointer that isn't a live block; also for
  realloc). The bad free is not passed on, so the heap stays consistent;
- **stack high-water marks** of every FreeRTOS task (from the 0xA5 fill pattern), by
  task name across boots, flagged `low` within 256 bytes of the end;
- heap statistics: allocations, frees, live and peak bytes in internal RAM and PSRAM.

```
[sim] memcheck: heap-use-after-free: 1-byte read at 0x3fcaad88 (pc dns_gethostbyname_addrtype+0x8, task "tiT", core 0)
[sim]   backtrace: dns_gethostbyname_addrtype+0x8 <- dns_gethostbyname+0xa <- sntp_request+0x52 <- ...
[sim]   0x3fcaad88 is the start of a 16-byte block at 0x3fcaad88
[sim]   allocated by task "loopTask": _ZN6String12changeBufferEj+0x40 <- ... <- _ZN11Preferences9getStringEPKc6String+0x40
[sim]   freed by task "loopTask": _ZN6String10invalidateEv+0x1c <- _ZN6StringD2Ev+0x8 <- _ZN5Clock14setTimeFromNTPEv+0xec <- ...
```

Each distinct violation (same kind and block, or same pc) is reported once on the
console and counted after that. `--memcheck=halt` stops the machine at the first one
(`sim.wait` then fails with it); plain `--memcheck` carries on. `GET /memcheck`
(Python: `sim.memcheck()`) returns the full report, and a summary with the stack marks
is printed when the run ends. `Simulator(memcheck="halt")` fails the `with` block if
there were violations (`sim.assert_no_memory_errors()` checks explicitly), and
`TRMNL_SIM_MEMCHECK=1 bin/spec` runs the whole suite that way. Known firmware bugs are
listed in `KNOWN_MEMORY_BUGS` in [support.py](tests/integration/support.py), passed as
`--memcheck-suppress` so the rest of each run is still checked, with an expected failure
for each in [test_memcheck.py](tests/integration/test_memcheck.py).

How it works: HLE hooks on the IDF heap's `multi_heap_*` layer, which every allocation
goes through exactly once (`malloc`, `heap_caps_*`, `new`, newlib in ROM; IDF 4.4 and
5.x), see each block with its size, TLSF block size and a short backtrace. A shadow
byte per byte of SRAM (and of the S3's 32 MB PSRAM window) marks user bytes of live
blocks, freed blocks, block headers and the slack after a block, and never-allocated
heap; every CPU load and store is checked against it (DMA and simulator accesses are
not), except by the allocator's own code (including heap poisoning's canaries, which the
ESP32-S3's Arduino 2 libraries write; `multi_heap_get_allocated_size` answers the size asked
for, so a realloc into another heap copies only that). Freed blocks wait in a small quarantine
(16 KB internal, 256 KB PSRAM; blocks over a quarter of that are freed at once) before
the allocator gets them back, and `realloc` always moves the block, so stale pointers
keep pointing at poisoned memory. Off, its cost is within measurement noise (0-3%);
on, it slows emulation by about 5-12%.

Limitations: stacks are exact on the X (windowed ABI) but heuristic on the OG/BWRY
(no frame pointers: return addresses found on the stack, so a frame may be stale).
Checks are byte-exact, except that an aligned load starting inside a block may run past
its end (word-at-a-time `strlen`/`memcpy` do that); an overflow that lands inside
another live block isn't seen. Static and global buffers
aren't checked, nor task stacks outside the heap. The quarantine and the moving
realloc make the heap look a little fuller than it is.

### CI

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs format, clippy and unit
tests, then builds the firmware with PlatformIO and runs the integration suite. On
failure it uploads simulator logs, final screens and screen diffs. It also uploads
the suite's firmware code coverage (merged lcov and HTML) as an artifact. It can also be
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
├─ mock-trmnl/         built-in mock TRMNL server and image conversion (GUI panel, /mock API)
└─ vnet/               user-mode router/NAT (smoltcp) + soft-AP client
```

**Chips and boards.** The build's image header names the chip (C3, S3 or C5); the board comes
from `--board` or the build directory's name, else from the chip and the firmware's symbols. A core implements `GuestCpu`; the C3 and C5 share
the RISC-V core (the C5 switches on its CLIC mode), and the Xtensa windowed ABI
lives behind `arg`, `return_from_hook`, `alloc_scratch` and `begin_call`, so the
IDF-level HLE (WiFi, sleep, ADC) is the same code on every chip. The WiFi model
handles the struct layouts of IDF 4.4 and 5.5 (and 5.5's dual-band variant on the C5). A new board is a `Board`
implementation plus its devices; a new chip is a `soc/` module.

**HLE and OTA.** Hooks are bound to addresses from `firmware.elf`. On every boot the
simulator reads the app descriptor of the slot the bootloader will start, and
matches its ELF SHA-256 against known ELFs. An OTA to a different build therefore
needs that build's ELF via `--elf`; otherwise the run halts with a clear message.

## Limitations

- ESP32-C3, ESP32-S3 and ESP32-C5 boards only (no classic ESP32). BYOD boards model what the
  firmware uses: no SD cards, touch panels or power-hold latches; charging only where the
  firmware reads a charger (TRMNL X, gen-2 OG).
- ESP32-C5: no ADC (the SAR ADC is HLE'd through Arduino's `analogRead*`), LP core or PARLIO
  RX models; PARLIO TX sends whole DMA chains at once. The ECDSA accelerator verifies
  signatures but can't sign or export keys (those use eFuse keys). WiFi 6 / 802.11ax details
  and BLE aren't modelled.
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
- Save points: only deep-sleep ones keep RTC and chip state. A modem that was powered at
  the time comes back powered off and boots afresh (its ESP-AT session isn't saved). The
  modem's flashed image itself isn't kept between simulator runs except in save points.

## Troubleshooting

- **The run halts with "CPU exception at …"**: the message has symbolized registers
  and a stack scan. `--trace fn` logs calls into a suspect function.
- **The firmware seems stuck**: `--headless --seconds N --profile` shows which
  functions the CPU spends its time in, often a polling loop on an unmodelled
  register.
- **A peripheral register isn't modelled**: run with `RUST_LOG=trmnl_sim=trace` to log
  first accesses to unmodelled registers.
- **Memory corruption or a crash in freed memory**: run with `--memcheck` (see
  [Memory checking](#memory-checking)) to catch the bad access where it happens.
- **Something hangs or crashes on the TRMNL X**: `POST /debug` (Python: `sim.debug()`)
  prints both cores' registers, a windowed-ABI backtrace (through HLE calls) and the
  running FreeRTOS task. `SIM_PEEK=addr,addr` adds memory words to that dump.
  `RUST_LOG=modem=debug` logs every AT command and modem reply.
