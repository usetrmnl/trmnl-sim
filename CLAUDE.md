# trmnl-sim

Emulator that runs unmodified TRMNL firmware builds (PlatformIO output from
`../trmnl-firmware/.pio/build/<env>`) on simulated ESP32-C3, ESP32-S3 and ESP32-C5 SoCs, with
board models, an HLE WiFi driver, a mock TRMNL server, an egui window and an HTTP control
API. README.md is the full reference (architecture, supported devices, control API, test
suite); read the relevant section before changing a subsystem.

## Repository rules

- Local git only: never push, never add a remote.
- Commit after each coherent improvement. Commit messages: a subject line, a body saying
  why, and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Never run `pio` unless asked; the user builds firmware. Never edit `../trmnl-firmware`
  from here (read it freely: `src/`, `lib/`, `.pio/libdeps/<env>/`, and the IDF/Arduino
  sources under `~/.platformio/packages` are the reference for what the hardware must do).
- Never run `pio run -c <other.ini>` in the firmware checkout: a different project config
  makes PlatformIO wipe every env's `.pio/build`. Use a separate `[platformio] build_dir`.

## Commands

| | |
|---|---|
| `bin/setup` | toolchains and ROM ELFs (`scripts/fetch-rom-elfs.sh` → gitignored `rom/`) |
| `bin/build` | release build of the simulator |
| `bin/test` | fmt, clippy (with and without the GUI), unit tests; run before committing |
| `bin/sim [<FW…-env.bin>] [--env ENV]` | run a firmware image (its `.elf` next to it) in the window; the board comes from `-<env>` in the name or `--env`; without arguments the window asks |

Style: `cargo fmt` (max_width 120), clippy clean. Match the surrounding code's comment
density and naming; comments explain hardware behaviour and why, with firmware file:line
references where relevant.

## Integration tests (../trmnl-spec)

- The integration tests live in their own repository, `../trmnl-spec` (RSpec; its CLAUDE.md
  and README say how to run and write them): they run firmware builds in this simulator's
  release build (`target/release/trmnl-sim`, via the control API). After changing the
  simulator, `cargo build --release`, then run the affected specs there, ONE group or example
  at a time under a hard limit:
  `cd ../trmnl-spec && perl -e 'alarm 90; exec @ARGV' bundle exec rspec spec/general/setup/portal_spec.rb:42; pkill -f target/release/trmnl-sim`
  (`ENVS=<env>:full` for one device). A full run (`rake spec` there) is a final regression
  check, run in the background.
- A test that fails because the simulator is wrong gets the simulator fixed here: general,
  minimal changes that keep the other chips and boards behaving identically. A test that
  fails because the firmware is wrong is marked in trmnl-spec (`known_failure:`), never
  worked around in the simulator.
- A change to the control API or the built-in behaviour the tests rely on needs the matching
  change in trmnl-spec's client library (`lib/trmnl_sim/`); commit each repository on its own.

## Where things are

- `src/soc/{esp32c3,esp32s3,esp32c5}/`: memory maps, peripherals, boot; `src/arch/`: RISC-V
  (with CLIC mode for the C5) and Xtensa cores; `src/hle/`: ESP-IDF function replacements
  (WiFi, sleep, ADC) bound by ELF symbol.
- `src/board/`: `spi_epd.rs` (data-driven SPI e-paper boards, one `BoardSpec` per firmware
  `device_list[]` row, selected by `-<env>` in the image's name or `--env`), `trmnl_x.rs`,
  `parallel_byod.rs`; `src/devices/`: panel controllers (UC81xx, SSD16xx, dual-CS, parallel),
  I2C chips, the ESP-AT modem, SPI flash.
- `crates/`: `sim-api` (emulator/front-end contract), `sim-ui` (egui), `sim-control` (HTTP
  API), `mock-trmnl` (built-in server, image conversion), `vnet` (user-mode network).

## Adding a board

Add a `BoardSpec` row (or a board module for non-SPI panels) and update the README's
supported-device tables; then add the board's tests in ../trmnl-spec (a `Device` and a
board group, see its CLAUDE.md), run them there and classify every failure as above.
