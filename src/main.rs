mod arch;
mod board;
mod devices;
mod firmware;
mod hle;
mod runner;
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
    /// Panel revision returned by the UC8179 REV command.
    #[arg(long, default_value = "0x0a0c1b2c", value_parser = parse_u32)]
    panel_rev: u32,
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
    let flash_path = cli.flash.clone().unwrap_or_else(|| cli.build_dir.join("sim-flash.bin"));
    let flash_data = firmware::prepare_flash(&flash_path, 4 << 20, &fw, cli.erase)?;
    let flash = SpiFlash::new(flash_data, Some(flash_path.clone()));

    let rom_path = match &cli.rom {
        Some(p) => p.clone(),
        None => firmware::find_rom_elf(soc::esp32c3::ROM_ELF).context(
            "ESP32-C3 ROM ELF not found: pass --rom, set TRMNL_SIM_ROM, or run \
             `pio pkg install -g -t platformio/tool-esp-rom-elfs`",
        )?,
    };
    let rom = std::fs::read(&rom_path)?;

    let panel = Uc8179::new(cli.panel_rev);
    let frame = panel.frame.clone();
    let board = Box::new(board::trmnl_og::TrmnlOg::new(panel));
    let mut apps = vec![(fw.elf_sha256, fw.symbols.clone(), fw.name.clone())];
    for p in &cli.elf {
        let a = firmware::ExtraApp::from_elf(p)?;
        apps.push((a.elf_sha256, a.symbols, a.name));
    }
    let mut machine = soc::esp32c3::Esp32c3::new(&rom, flash, board, apps, &cli.trace)?;
    if let Some(mac) = cli.mac {
        machine.set_mac(mac);
    }
    machine.set_portal_port(cli.portal_port);
    machine.set_net_config(vnet::NetConfig {
        offline: cli.offline,
        dns_overrides: cli.dns.clone(),
        ..Default::default()
    });

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
    };
    if let Some(addr) = cli.control {
        let (bound, _t) = sim_control::serve(handle.clone(), addr)?;
        eprintln!("trmnl-sim: control API on http://{bound}/");
    }
    eprintln!("trmnl-sim: {} ({} symbols), flash {}", fw.name, fw.symbols.len(), flash_path.display());

    let emu = std::thread::Builder::new()
        .name("emulator".into())
        .stack_size(16 << 20)
        .spawn(move || runner::run(Box::new(machine), ports, opts))?;

    let halted = if cli.headless {
        emu.join().ok().and_then(|o| o.halted)
    } else {
        #[cfg(feature = "gui")]
        sim_ui::run(
            handle.clone(),
            sim_ui::UiOptions { title: format!("TRMNL Simulator — {}", fw.name), scale: cli.scale },
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

/// Write the panel as seen by a viewer (0 = black ink, 255 = paper).
fn save_png(frame: &sim_api::SharedFrame, path: &std::path::Path) -> Result<()> {
    let f = frame.lock();
    let pixels: Vec<u8> = f.pixels.iter().map(|d| 255 - d).collect();
    std::fs::write(path, sim_control::encode_png(f.width, f.height, &pixels))?;
    Ok(())
}
