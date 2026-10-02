// Index loops over parallel per-core / per-timer arrays read clearer than zipped iterators.
#![allow(clippy::needless_range_loop)]

mod arch;
mod board;
mod coverage;
mod devices;
mod faults;
mod firmware;
mod hle;
mod memcheck;
mod nvs;
mod periph;
mod runner;
mod savepoint;
mod soc;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use devices::spi_flash::SpiFlash;

/// Run TRMNL firmware builds in a simulated device.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The PlatformIO environment the firmware was built with (e.g. trmnl, TRMNL_X,
    /// seeed_reTerminal_E1001); it picks the board. Give it and the firmware, or neither: then
    /// the window asks for both.
    #[arg(requires = "firmware")]
    env: Option<String>,
    /// Merged flash image (e.g. merged_firmware.bin); its ELF is the same path with the .elf
    /// extension.
    firmware: Option<PathBuf>,
    /// Persistent flash image (NVS/SPIFFS survive restarts). Default: sim-flash.bin next to
    /// the firmware.
    #[arg(long)]
    flash: Option<PathBuf>,
    /// Erase flash (factory reset) before flashing the firmware.
    #[arg(long)]
    erase: bool,
    /// Run without a window; serial output goes to stdout.
    #[arg(long)]
    headless: bool,
    /// Stop after this many seconds of virtual time (headless).
    #[arg(long)]
    seconds: Option<f64>,
    /// Don't pace to wall-clock time.
    #[arg(long)]
    turbo: bool,
    /// Fast-forward deep sleeps (otherwise they last their real duration; end them early with Wake/button).
    #[arg(long)]
    fast_sleep: bool,
    /// Print a CPU profile and state on exit.
    #[arg(long)]
    profile: bool,
    /// WiFi MAC address burned into eFuse (the device's identity on the server), e.g. 7C:DF:A1:12:34:56
    #[arg(long, value_parser = parse_mac)]
    mac: Option<[u8; 6]>,
    /// Serve the HTTP control API for integration tests on this address (e.g. 127.0.0.1:7878).
    #[arg(long)]
    control: Option<std::net::SocketAddr>,
    /// Start the built-in mock TRMNL server on this port (default 8090; 0 = any free port).
    /// The device reaches it at http://10.0.2.2:PORT. It can also be started from the window
    /// or the control API.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "8090")]
    mock_server: Option<u16>,
    /// Host port forwarded to the device's captive portal while it's in setup mode (0 = pick a free one).
    #[arg(long, default_value_t = 8080)]
    portal_port: u16,
    /// Hermetic networking: only the host (10.0.2.2 = 127.0.0.1) is reachable from the device.
    #[arg(long)]
    offline: bool,
    /// Answer DNS for NAME with IP, e.g. --dns api.example.com=10.0.2.2 (repeatable).
    #[arg(long, value_parser = parse_dns)]
    dns: Vec<(String, std::net::Ipv4Addr)>,
    /// Send the device's connections to 10.0.2.2:GUEST to host port HOST instead, e.g.
    /// --host-port 8090=51234 for a device onboarded against a server that has since moved
    /// (repeatable).
    #[arg(long, value_name = "GUEST=HOST", value_parser = parse_host_port)]
    host_port: Vec<(u16, u16)>,
    /// ROM ELF for the build's chip (default: $TRMNL_SIM_ROM, else the simulator's rom/ directory,
    /// ~/.cache/trmnl-sim/rom-elfs or PlatformIO's tool-esp-rom-elfs).
    #[arg(long, env = "TRMNL_SIM_ROM")]
    rom: Option<PathBuf>,
    /// Save the e-paper contents as PNG when the run ends.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// Initial display zoom (0 = fit to window).
    #[arg(long, default_value_t = 0.0)]
    scale: f32,
    /// App image (firmware.bin) the built-in server offers for OTA updates, at
    /// /firmware.bin; its ELF (the same path with .elf) is loaded so the device can boot it.
    #[arg(long, value_name = "PATH")]
    ota_firmware: Option<PathBuf>,
    /// Additional firmware ELFs the device may boot after an OTA update (repeatable).
    #[arg(long)]
    elf: Vec<PathBuf>,
    /// Log calls to these firmware functions (comma separated symbol names).
    #[arg(long, value_delimiter = ',')]
    trace: Vec<String>,
    /// Record which firmware instructions execute and write an lcov tracefile here when
    /// the run ends (or on POST /coverage).
    #[arg(long, value_name = "FILE")]
    coverage: Option<PathBuf>,
    /// Write coverage paths under this directory relative to it, e.g. the firmware checkout
    /// (default: absolute paths).
    #[arg(long, value_name = "DIR")]
    coverage_root: Option<PathBuf>,
    /// Only report source files whose (relative) path starts with one of these, e.g.
    /// src/,lib/ (repeatable).
    #[arg(long, value_name = "PREFIX", value_delimiter = ',')]
    coverage_include: Vec<String>,
    /// Check the firmware's memory use: heap use-after-free, overflows, double and invalid
    /// frees, stack high-water marks. `--memcheck` reports and carries on, `--memcheck=halt`
    /// stops at the first violation.
    #[arg(long, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "log",
          value_parser = ["log", "halt"])]
    memcheck: Option<String>,
    /// Tolerate known memory bugs: a violation is ignored if one of these functions is in
    /// its backtrace or in its block's allocation or free stack (comma separated).
    #[arg(long, value_name = "FUNCTIONS", value_delimiter = ',')]
    memcheck_suppress: Vec<String>,
    /// Panel revision returned by the UC8179 REV command.
    #[arg(long, default_value = "0x0a0c1b2c", value_parser = parse_u32)]
    panel_rev: u32,
    /// Start from a save point file (taken with this firmware build) instead of booting.
    /// Its flash replaces the --flash image.
    #[arg(long, value_name = "FILE")]
    restore: Option<PathBuf>,
    /// Environment sensor on the I2C header of an SPI-panel board (repeatable): scd41, aht20.
    #[arg(long, value_enum)]
    sensor: Vec<board::spi_epd::Sensor>,
    /// Access points in range of the device's own radio, replacing the defaults: a JSON
    /// array, e.g. '[{"ssid":"TRMNL_QA","rssi":-40},{"ssid":"Home","password":"pw"}]'
    /// (keys: ssid, password, rssi, channel, open, internet).
    #[arg(long, value_name = "JSON")]
    wifi_networks: Option<String>,
    /// Inject faults from the start: JSON as for POST /faults, e.g. '{"net":{"dns":"servfail"}}'
    /// (repeatable; later ones are merged into earlier ones).
    #[arg(long, value_name = "JSON")]
    faults: Vec<String>,
}

/// Every PlatformIO environment the simulator has a board for, with the board's name.
fn board_envs() -> Vec<(&'static str, &'static str)> {
    let mut v = vec![(board::trmnl_x::ENV, "TRMNL X")];
    v.extend(board::spi_epd::SPECS.iter().flat_map(|s| s.envs.iter().map(|e| (*e, s.name))));
    v.extend(board::parallel_byod::SPECS.iter().flat_map(|s| s.envs.iter().map(|e| (*e, s.name))));
    v
}

fn parse_dns(s: &str) -> Result<(String, std::net::Ipv4Addr), String> {
    let (name, ip) = s.split_once('=').ok_or("expected NAME=IP")?;
    Ok((name.to_string(), ip.parse().map_err(|e| format!("{e}"))?))
}

fn parse_host_port(s: &str) -> Result<(u16, u16), String> {
    let (guest, host) = s.split_once('=').ok_or("expected GUEST=HOST")?;
    Ok((guest.parse().map_err(|e| format!("{e}"))?, host.parse().map_err(|e| format!("{e}"))?))
}

fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let parts: Vec<u8> =
        s.split([':', '-']).map(|p| u8::from_str_radix(p, 16).map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
    parts.try_into().map_err(|_| "expected 6 bytes like 7C:DF:A1:12:34:56".to_string())
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim_start_matches("0x");
    u32::from_str_radix(s, 16).map_err(|e| e.to_string())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // The launcher (with the GUI) may fill in options.
    #[cfg_attr(not(feature = "gui"), allow(unused_mut))]
    let mut cli = Cli::parse();
    let (env, firmware) = match (&cli.env, &cli.firmware) {
        (Some(e), Some(f)) => (e.clone(), f.clone()),
        _ if cli.headless => anyhow::bail!("--headless needs the environment and the firmware image"),
        #[cfg(feature = "gui")]
        _ => {
            let boards = board_envs()
                .into_iter()
                .map(|(env, name)| sim_ui::BoardChoice { env: env.into(), name: name.into() })
                .collect();
            let Some(l) = sim_ui::launch(boards)? else { return Ok(()) };
            // What the launcher sets; the rest stays as given on the command line.
            cli.mac = l.mac.or(cli.mac);
            cli.erase |= l.erase;
            (l.env, l.firmware)
        }
        #[cfg(not(feature = "gui"))]
        _ => anyhow::bail!("built without the `gui` feature: pass the environment and the firmware image"),
    };
    let fw = firmware::Firmware::from_merged(&firmware)?;
    let fw_dir = firmware.parent().map(PathBuf::from).unwrap_or_default();
    let restore = cli.restore.as_deref().map(savepoint::SavePoint::load).transpose()?;
    let flash_path = cli.flash.clone().unwrap_or_else(|| fw_dir.join("sim-flash.bin"));
    let flash_data = firmware::prepare_flash(&flash_path, fw.flash_size, &fw, cli.erase)?;
    let flash = SpiFlash::new(flash_data, Some(flash_path.clone()));

    let rom_name = match fw.chip_id {
        firmware::CHIP_ESP32C3 => soc::esp32c3::ROM_ELF,
        firmware::CHIP_ESP32S3 => soc::esp32s3::ROM_ELF,
        firmware::CHIP_ESP32C5 => soc::esp32c5::ROM_ELF,
        other => {
            anyhow::bail!("unsupported chip id {other} in the firmware image (supported: ESP32-C3, ESP32-S3, ESP32-C5)")
        }
    };
    let rom_path = match &cli.rom {
        Some(p) => p.clone(),
        None => firmware::find_rom_elf(rom_name).with_context(|| {
            format!(
                "{rom_name} not found: pass --rom, set TRMNL_SIM_ROM, run scripts/fetch-rom-elfs.sh, or \
                 `pio pkg install -g -t platformio/tool-esp-rom-elfs`"
            )
        })?,
    };
    let rom = std::fs::read(&rom_path)?;

    let ota_elf = cli.ota_firmware.as_ref().map(|p| p.with_extension("elf"));
    let extra_elfs: Vec<PathBuf> = cli.elf.iter().cloned().chain(ota_elf).collect();
    let mut apps = vec![(fw.elf_sha256, fw.symbols.clone(), fw.name.clone())];
    for p in &extra_elfs {
        let a = firmware::ExtraApp::from_elf(p)?;
        apps.push((a.elf_sha256, a.symbols, a.name));
    }
    let memcheck_mode = cli.memcheck.as_deref().map(|m| match m {
        "halt" => memcheck::Mode::Halt,
        _ => memcheck::Mode::Log,
    });
    let net = vnet::NetConfig {
        offline: cli.offline,
        dns_overrides: cli.dns.clone(),
        host_ports: cli.host_port.clone(),
        ..Default::default()
    };

    let parallel_spec = board::parallel_byod::find(&env);
    let spi_spec = board::spi_epd::find(&env);
    if parallel_spec.is_none() && spi_spec.is_none() && env != board::trmnl_x::ENV {
        let envs: Vec<&str> = board_envs().into_iter().map(|(e, _)| e).collect();
        anyhow::bail!("unknown PlatformIO environment {env:?} (known: {})", envs.join(", "));
    }
    let (board, frame, panel): (Box<dyn board::Board>, sim_api::SharedFrame, mock_trmnl::Panel) = match spi_spec {
        Some(spec) => {
            let chip = match spec.chip {
                board::spi_epd::Chip::Esp32c3 => firmware::CHIP_ESP32C3,
                board::spi_epd::Chip::Esp32s3 => firmware::CHIP_ESP32S3,
                board::spi_epd::Chip::Esp32c5 => firmware::CHIP_ESP32C5,
            };
            if chip != fw.chip_id {
                anyhow::bail!("the {} is a {:?} board, but the firmware is for another chip", spec.name, spec.chip);
            }
            let b = board::spi_epd::SpiEpdBoard::new(spec, cli.panel_rev, &cli.sensor);
            let frame = b.panel.frame();
            (Box::new(b), frame, spec.panel.mock_panel())
        }
        None if parallel_spec.is_some() => {
            let spec = parallel_spec.unwrap();
            let chip = match spec.chip {
                board::spi_epd::Chip::Esp32c3 => firmware::CHIP_ESP32C3,
                board::spi_epd::Chip::Esp32s3 => firmware::CHIP_ESP32S3,
                board::spi_epd::Chip::Esp32c5 => firmware::CHIP_ESP32C5,
            };
            if chip != fw.chip_id {
                anyhow::bail!("the {} is a {:?} board, but the firmware is for another chip", spec.name, spec.chip);
            }
            let b = board::parallel_byod::ParallelByodBoard::new(spec);
            let frame = b.panel.frame();
            let (w, h) = {
                let f = frame.lock();
                (f.width, f.height)
            };
            (Box::new(b), frame, mock_trmnl::Panel::new(mock_trmnl::Inks::Gray16, w as u32, h as u32))
        }
        None => {
            if fw.chip_id != firmware::CHIP_ESP32S3 {
                anyhow::bail!("the TRMNL X is an ESP32-S3 board, but the firmware is for another chip");
            }
            let modem_mac = cli.mac.map(|mut m| {
                m[5] = m[5].wrapping_add(2);
                m
            });
            let b = board::trmnl_x::TrmnlX::new(modem_mac.unwrap_or([0x7c, 0xdf, 0xa1, 0x5e, 0x1a, 0x2d]), &net);
            let frame = b.panel.frame();
            (Box::new(b), frame, mock_trmnl::Panel::X)
        }
    };
    let mut machine: Box<dyn soc::Machine> = match fw.chip_id {
        firmware::CHIP_ESP32S3 => {
            let mut m = soc::esp32s3::Esp32s3::new(&rom, flash, board, apps, &cli.trace)?;
            if let Some(mac) = cli.mac {
                m.set_mac(mac);
            }
            m.set_portal_port(cli.portal_port);
            m.set_net_config(net);
            if let Some(mode) = memcheck_mode {
                m.enable_memcheck(mode, cli.memcheck_suppress.clone());
            }
            Box::new(m)
        }
        firmware::CHIP_ESP32C5 => {
            let mut m = soc::esp32c5::Esp32c5::new(&rom, flash, board, apps, &cli.trace)?;
            if let Some(mac) = cli.mac {
                m.set_mac(mac);
            }
            m.set_portal_port(cli.portal_port);
            m.set_net_config(net);
            if let Some(mode) = memcheck_mode {
                m.enable_memcheck(mode, cli.memcheck_suppress.clone());
            }
            Box::new(m)
        }
        _ => {
            let mut m = soc::esp32c3::Esp32c3::new(&rom, flash, board, apps, &cli.trace)?;
            if let Some(mac) = cli.mac {
                m.set_mac(mac);
            }
            m.set_portal_port(cli.portal_port);
            m.set_net_config(net);
            if let Some(mode) = memcheck_mode {
                m.enable_memcheck(mode, cli.memcheck_suppress.clone());
            }
            Box::new(m)
        }
    };

    if let Some(json) = &cli.wifi_networks {
        let nets = sim_control::wifi::parse_networks_str(json).map_err(|e| anyhow::anyhow!("--wifi-networks: {e}"))?;
        machine.set_wifi_networks(&nets);
    }

    let fw_id = savepoint::FirmwareId { name: fw.name.clone(), elf_sha256: fw.elf_sha256 };
    if let Some(sp) = &restore {
        sp.check_compatible(&fw_id, &machine.board().info().name)?;
        if cli.mac.is_some_and(|m| m != sp.soc.mac) {
            eprintln!("trmnl-sim: the save point's MAC replaces --mac");
        }
    }
    let coverage = match &cli.coverage {
        Some(path) => {
            let elfs: Vec<PathBuf> =
                std::iter::once(firmware.with_extension("elf")).chain(extra_elfs.clone()).collect();
            let data = elfs.iter().map(std::fs::read).collect::<std::io::Result<Vec<_>>>()?;
            machine.set_coverage(coverage::Coverage::new(&data.iter().map(Vec::as_slice).collect::<Vec<_>>())?);
            // DWARF paths are absolute: so is the root.
            let root = cli.coverage_root.as_ref().map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()));
            let filter = coverage::PathFilter { root, include: cli.coverage_include.clone() };
            Some(coverage::Reporter::new(elfs, filter, path.clone()))
        }
        None => None,
    };
    let mut faults = sim_api::Faults::default();
    for f in &cli.faults {
        faults = sim_control::faults::merge_faults_str(&faults, f).map_err(|e| anyhow::anyhow!("--faults: {e}"))?;
    }

    let frame_for_shot = frame.clone();
    let (handle, ports) = sim_api::channel(frame);
    let opts = runner::RunnerOptions {
        max_virtual_ns: cli.seconds.map(|s| (s * 1e9) as u64),
        turbo: cli.turbo,
        fast_sleep: cli.fast_sleep,
        echo_console: true,
        profile: cli.profile,
        firmware_name: fw.name.clone(),
        exit_on_halt: cli.headless && cli.control.is_none(),
        firmware: fw_id,
        restore,
        coverage,
        faults,
    };
    let mock = mock_trmnl::MockServer::new(panel);
    let status = handle.status.clone();
    mock.set_clock(move || status.lock().sim_time_ns);
    if let Some(p) = &cli.ota_firmware {
        anyhow::ensure!(p.is_file(), "--ota-firmware: {} not found", p.display());
        mock.set_file(mock_trmnl::OTA_FIRMWARE_PATH, mock_trmnl::FileSource::Path(p.clone()));
    }
    if let Some(port) = cli.mock_server {
        let bound = mock.start(port).with_context(|| format!("starting the mock server on port {port}"))?;
        eprintln!("trmnl-sim: mock server on http://{bound}/ (device URL http://10.0.2.2:{})", bound.port());
    }
    if let Some(addr) = cli.control {
        let (bound, _t) = sim_control::serve_with_mock(handle.clone(), addr, Some(mock.clone()))?;
        eprintln!("trmnl-sim: control API on http://{bound}/");
    }
    eprintln!("trmnl-sim: {} ({} symbols), flash {}", fw.name, fw.symbols.len(), flash_path.display());

    let emu = std::thread::Builder::new()
        .name("emulator".into())
        .stack_size(16 << 20)
        .spawn(move || runner::run(machine, ports, opts))?;

    let halted = if cli.headless {
        emu.join().ok().and_then(|o| o.halted)
    } else {
        #[cfg(feature = "gui")]
        sim_ui::run(
            handle.clone(),
            sim_ui::UiOptions {
                title: format!("TRMNL Simulator — {}", fw.name),
                scale: cli.scale,
                mock: Some(mock.clone()),
                ota_firmware: cli.ota_firmware.clone(),
            },
        )?;
        #[cfg(not(feature = "gui"))]
        anyhow::bail!("built without the `gui` feature: run with --headless");
        #[allow(unreachable_code)]
        handle.send(sim_api::Command::Quit);
        emu.join().ok().and_then(|o| o.halted)
    };
    if let Some(path) = &cli.screenshot {
        save_png(&frame_for_shot, path)?;
        eprintln!("screenshot saved to {}", path.display());
    }
    if let Some(msg) = halted {
        eprintln!("trmnl-sim: halted: {msg}");
        std::process::exit(2);
    }
    Ok(())
}

/// Write the panel as seen by a viewer (gray: 0 = black ink, 255 = paper; or RGB).
fn save_png(frame: &sim_api::SharedFrame, path: &std::path::Path) -> Result<()> {
    let (w, h, ch, px) = frame.lock().viewer(None);
    std::fs::write(path, sim_control::encode_png_channels(w, h, ch, &px))?;
    Ok(())
}
