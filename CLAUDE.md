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
| `rake spec ...` | integration tests (below; `rake -T` lists the tasks) |

Style: `cargo fmt` (max_width 120), clippy clean. Match the surrounding code's comment
density and naming; comments explain hardware behaviour and why, with firmware file:line
references where relevant.

## Integration tests (tests/integration, RSpec)

- Rake tasks (tests/integration/Rakefile, imported by the root Rakefile); a task's argument is
  one shell-style string. `rake "spec[<file>]"`: a spec file by short name (`portal` =
  spec/portal_spec.rb), a path, `path:line` or `path[id]`; rspec options pass through
  (`rake "spec[portal -e 'wrong password']"`). `rake "spec[<env>]"`: everything for one
  PlatformIO environment. `rake spec` (default tier): every device's own specs, the general
  ones in full on the OG and the `:smoke` examples on the rest (~9 min).
  `rake spec:comprehensive` (~24 min), `spec:exhaustive`, `spec:plan[...]` (dry run),
  `spec:envs`; `--slow`, `-j N`, `--no-cache` go in the argument. The runner is
  tests/integration/runner/runner.rb. `rake firmware[...]` runs pio: only when asked.
- While iterating, run ONE group or example at a time under a hard limit and clean up:
  `perl -e 'alarm 90; exec @ARGV' rake "spec[portal -e FailedJoin]"; pkill -f target/release/trmnl-sim`
  (or `cd tests/integration && bundle exec rspec spec/portal_spec.rb:42`, with
  `TRMNL_SIM_DEVICE=<env>` for another device under test). No full-suite runs for debugging;
  a full run is for a final regression check (run it in the background).
- `spec/support/devices.rb` has a `Device` profile per environment (size, inks, chip, battery,
  button, ...) plus FAMILIES / REPRESENTATIVES. Every group declares the environment it runs:
  `env: "<env>"`, or `env: :any` for general specs, which run on the device under test
  (`TRMNL_SIM_DEVICE`, default `trmnl`) and must adapt to its `Device` (use `device_image`,
  `needs:` / `only_on:` metadata, `match_golden`). Metadata is documented in
  `spec/support/metadata.rb`; the client library (`TrmnlSim::Simulator`, `MockTrmnl`) is in
  `tests/integration/lib`. Everything the suite needs lives under tests/integration (it is
  meant to move to a repository of its own): keep it self-contained, reaching this checkout
  only through `TrmnlSim::REPO` / `Builds::ROOT` (`TRMNL_SIM_REPO`).
- A test that fails because the firmware is wrong stays in and is marked:
  `known_failure: { "<env>" => "what the firmware does wrong, file:line" }` on the example
  (or group), or `pending: "..."` with a comment for device-specific specs. Verify the root
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
`tests/integration/spec/support/devices.rb` (and its family in `FAMILIES`), and a board
group using the BYOD support (`spec/support/byod.rb`); then run `rake "spec[<env>]"` and
classify every failure as above. Update the README's supported-device tables.
