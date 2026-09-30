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
| `bin/test` | fmt, clippy (with and without the GUI), unit tests, `rake check`; run before committing |
| `bin/dev [og\|bwry\|x\|e1002\|gen2\|gen2bwry\|<build dir>]` | run a firmware build in the window |
| `rake spec` | integration tests in parallel (below; `rake -T` lists the tasks) |

Style: `cargo fmt` (max_width 120), clippy clean. Match the surrounding code's comment
density and naming; comments explain hardware behaviour and why, with firmware file:line
references where relevant.

## Integration tests (tests/integration, RSpec)

- Plain RSpec, run in tests/integration: `bundle exec rspec [path[:line]] [-e ...]`, or
  `bundle exec parallel_rspec` (`rake spec` builds the simulator first, then runs it).
  `ENVS` (spec/support/selection.rb) lists the devices a run covers: PlatformIO environments
  or `core` / `byod` / `all`, each optionally `:full`. A listed device runs its own specs and
  the general `:smoke` examples; `:full` runs every general example on it. Default
  `trmnl:full TRMNL_X trmnl_4clr trmnl_gen2 trmnl_gen2_4clr`; unlisted devices and missing
  builds are left out. `TRMNL_SIM_SLOW=1`, `TRMNL_SPEC_NO_CACHE=1` as needed. `rake
  firmware[...]` runs pio: only when asked.
- While iterating, run ONE group or example at a time under a hard limit and clean up:
  `cd tests/integration && perl -e 'alarm 90; exec @ARGV' bundle exec rspec spec/general/setup/portal_spec.rb:42; pkill -f target/release/trmnl-sim`
  (`ENVS=<env>` or `ENVS=<env>:full` for one device). No full-suite runs for debugging; a
  full run is for a final regression check (run it in the background).
- `spec/support/devices.rb` has a `Device` profile per environment (size, inks, chip, battery,
  button, core or BYOD, ...). Every group declares the environment it runs: `env: "<env>"`
  (device specs: spec/core/, spec/byod/), or `General.describe` for general specs
  (spec/general/), which is defined once per listed device and must adapt to its `Device`
  (`device` / `build` in the group; use `device_image`, `needs:` / `only_on:` metadata,
  `match_golden`). Metadata is documented in `spec/support/metadata.rb`; the client library
  (`TrmnlSim::Simulator`, `MockTrmnl`) is in `tests/integration/lib`. Everything the suite
  needs lives under tests/integration (it is meant to move to a repository of its own): keep
  it self-contained, reaching this checkout only through `TrmnlSim::REPO` / `Builds::ROOT`
  (`TRMNL_SIM_REPO`).
- A test that fails because the firmware is wrong stays in and is marked:
  `known_failure: { "<env>" => "what the firmware does wrong, file:line" }` on the example
  (or group), or `pending: "..."` with a comment. Verify the root
  cause in the firmware source first. Never weaken an assertion or work around a firmware
  bug in the simulator.
- A test that fails because the simulator is wrong gets the simulator fixed: general,
  minimal changes that keep the other chips and boards behaving identically.
- Golden screenshots: per-device ones live in `golden/<env>/`. Look at every new or
  rewritten golden (open the PNG) before committing it; wrong-looking output is a firmware
  bug to record, not a golden.
- Onboarded devices are cached in `target/spec-cache/` keyed by firmware, simulator and
  test support code, so the first run after a change is slower.

## Where things are

- `src/soc/{esp32c3,esp32s3,esp32c5}/`: memory maps, peripherals, boot; `src/arch/`: RISC-V
  (with CLIC mode for the C5) and Xtensa cores; `src/hle/`: ESP-IDF function replacements
  (WiFi, sleep, ADC) bound by ELF symbol.
- `src/board/`: `spi_epd.rs` (data-driven SPI e-paper boards, one `BoardSpec` per firmware
  `device_list[]` row, selected by `--board` or the build directory name), `trmnl_x.rs`,
  `parallel_byod.rs`; `src/devices/`: panel controllers (UC81xx, SSD16xx, dual-CS, parallel),
  I2C chips, the ESP-AT modem, SPI flash.
- `crates/`: `sim-api` (emulator/front-end contract), `sim-ui` (egui), `sim-control` (HTTP
  API), `mock-trmnl` (built-in server, image conversion), `vnet` (user-mode network).
- `tests/integration/lib/trmnl_sim/`: `TrmnlSim::Simulator` (control API client), `MockTrmnl`, `Images`.

## Adding a board

Add a `BoardSpec` row (or a board module for non-SPI panels), a `Device` in
`tests/integration/spec/support/devices.rb`, and a board group in spec/byod/ using the BYOD
support (`spec/support/byod.rb`); then run `ENVS=<env>:full bundle exec parallel_rspec` and
classify every failure as above. Update the README's supported-device tables.
