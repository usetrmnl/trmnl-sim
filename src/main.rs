// Index loops over parallel per-core / per-timer arrays read clearer than zipped iterators.
#![allow(clippy::needless_range_loop)]

mod arch;
mod board;
mod coverage;
mod devices;
mod faults;
mod firmware;
mod hle;
mod periph;
mod runner;
mod savepoint;
mod soc;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use devices::spi_flash::SpiFlash;
use devices::uc8179::Uc8179;

/// Run TRMNL firmware builds in a simulated device.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// PlatformIO build directory, e.g. ../trmnl-firmware/.pio/build/trmnl
    build_dir: PathBuf,
    /// Persistent flash image (NVS/SPIFFS survive restarts). Default: <build_dir>/sim-flash.bin
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
    /// ESP32-C3 ROM ELF (default: $TRMNL_SIM_ROM or PlatformIO's tool-esp-rom-elfs).
    #[arg(long, env = "TRMNL_SIM_ROM")]
    rom: Option<PathBuf>,
    /// Save the e-paper contents as PNG when the run ends.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// Initial display zoom (0 = fit to window).
    #[arg(long, default_value_t = 0.0)]
    scale: f32,
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
    /// Write coverage paths under this directory relative to it (default: the firmware
    /// checkout the build dir is in).
    #[arg(long, value_name = "DIR")]
    coverage_root: Option<PathBuf>,
    /// Only report source files whose (relative) path starts with one of these, e.g.
    /// src/,lib/ (repeatable).
    #[arg(long, value_name = "PREFIX", value_delimiter = ',')]
    coverage_include: Vec<String>,
    /// Panel revision returned by the UC8179 REV command.
    #[arg(long, default_value = "0x0a0c1b2c", value_parser = parse_u32)]
    panel_rev: u32,
    /// Start from a save point file (taken with this firmware build) instead of booting.
    /// Its flash replaces the --flash image.
    #[arg(long, value_name = "FILE")]
    restore: Option<PathBuf>,
    /// Inject faults from the start: JSON as for POST /faults, e.g. '{"net":{"dns":"servfail"}}'
    /// (repeatable; later ones are merged into earlier ones).
    #[arg(long, value_name = "JSON")]
    faults: Vec<String>,
}

fn parse_dns(s: &str) -> Result<(String, std::net::Ipv4Addr), String> {
    let (name, ip) = s.split_once('=').ok_or("expected NAME=IP")?;
    Ok((name.to_string(), ip.parse().map_err(|e| format!("{e}"))?))
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
    let cli = Cli::parse();

    let fw = firmware::Firmware::from_build_dir(&cli.build_dir)?;
    let restore = cli.restore.as_deref().map(savepoint::SavePoint::load).transpose()?;
    let flash_path = cli.flash.clone().unwrap_or_else(|| cli.build_dir.join("sim-flash.bin"));
    let flash_data = firmware::prepare_flash(&flash_path, fw.flash_size, &fw, cli.erase)?;
    let flash = SpiFlash::new(flash_data, Some(flash_path.clone()));

    let rom_name = match fw.chip_id {
        firmware::CHIP_ESP32C3 => soc::esp32c3::ROM_ELF,
        firmware::CHIP_ESP32S3 => soc::esp32s3::ROM_ELF,
        other => anyhow::bail!("unsupported chip id {other} in the firmware image (supported: ESP32-C3, ESP32-S3)"),
    };
    let rom_path = match &cli.rom {
        Some(p) => p.clone(),
        None => firmware::find_rom_elf(rom_name).with_context(|| {
            format!(
                "{rom_name} not found: pass --rom, set TRMNL_SIM_ROM, or run \
                 `pio pkg install -g -t platformio/tool-esp-rom-elfs`"
            )
        })?,
    };
    let rom = std::fs::read(&rom_path)?;

    let mut apps = vec![(fw.elf_sha256, fw.symbols.clone(), fw.name.clone())];
    for p in &cli.elf {
        let a = firmware::ExtraApp::from_elf(p)?;
        apps.push((a.elf_sha256, a.symbols, a.name));
    }
    let net = vnet::NetConfig { offline: cli.offline, dns_overrides: cli.dns.clone(), ..Default::default() };

    let mut panel = mock_trmnl::Panel::Og;
    let (mut machine, frame): (Box<dyn soc::Machine>, sim_api::SharedFrame) = match fw.chip_id {
        firmware::CHIP_ESP32S3 => {
            let modem_mac = cli.mac.map(|mut m| {
                m[5] = m[5].wrapping_add(2);
                m
            });
            let board = board::trmnl_x::TrmnlX::new(
                modem_mac.unwrap_or([0x7c, 0xdf, 0xa1, 0x5e, 0x1a, 0x2d]),
                cli.offline,
                cli.dns.clone(),
            );
            let frame = board.panel.frame();
            panel = mock_trmnl::Panel::X;
            let mut m = soc::esp32s3::Esp32s3::new(&rom, flash, Box::new(board), apps, &cli.trace)?;
            if let Some(mac) = cli.mac {
                m.set_mac(mac);
            }
            m.set_portal_port(cli.portal_port);
            m.set_net_config(net);
            (Box::new(m), frame)
        }
        _ => {
            // trmnl_4clr (TRMNL BWRY) is the OG board with a 4-color panel.
            let bwry = fw.symbols.has_prefix("_Z13png_draw_4clr");
            let epd = if bwry { Uc8179::new_bwry(cli.panel_rev) } else { Uc8179::new(cli.panel_rev) };
            if bwry {
                panel = mock_trmnl::Panel::Bwry;
            }
            let frame = epd.frame.clone();
            let board = Box::new(board::trmnl_og::TrmnlOg::new(epd));
            let mut m = soc::esp32c3::Esp32c3::new(&rom, flash, board, apps, &cli.trace)?;
            if let Some(mac) = cli.mac {
                m.set_mac(mac);
            }
            m.set_portal_port(cli.portal_port);
            m.set_net_config(net);
            (Box::new(m), frame)
        }
    };

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
                std::iter::once(cli.build_dir.join("firmware.elf")).chain(cli.elf.clone()).collect();
            let data = elfs.iter().map(std::fs::read).collect::<std::io::Result<Vec<_>>>()?;
            machine.set_coverage(coverage::Coverage::new(&data.iter().map(Vec::as_slice).collect::<Vec<_>>())?);
            let filter = coverage::PathFilter {
                root: cli.coverage_root.clone().or_else(|| coverage::checkout_of(&cli.build_dir)),
                include: cli.coverage_include.clone(),
            };
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
    // This build's firmware, for OTA updates from the built-in server.
    let fw_bin = cli.build_dir.join("firmware.bin");
    if fw_bin.exists() {
        mock.set_file("/firmware.bin", mock_trmnl::FileSource::Path(fw_bin));
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
