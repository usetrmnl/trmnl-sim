//! ESP32-S3: two Xtensa LX7 cores at up to 240 MHz, 512 KB SRAM, octal PSRAM.
//!
//! Both cores run in lockstep quanta of `QUANTUM` cycles on one shared clock:
//! each core executes up to `QUANTUM` instructions (stopping early in WAITI),
//! then time advances. That keeps FreeRTOS SMP's view of time consistent.

pub mod bus;
pub mod periph;

use std::collections::HashMap;

use crate::arch::GuestCpu;
use crate::arch::xtensa::{Step, Trap, Xtensa, cause};
use crate::board::Board;
use crate::coverage::Coverage;
use crate::devices::spi_flash::SpiFlash;
use crate::firmware::{self, Symbols};
use crate::hle::{self, GuestMem, HleCtx, HleEnv, HleState, Hooks, MAGIC_BASE, MachineRequest};
use crate::memcheck::{self, Memcheck};
use crate::savepoint::SocState;

use super::{Machine, NetStatus, Output, ResetKind, SliceExit};
use bus::S3Bus;
use periph::{ResetReason, ResetRequest};

pub const ROM_ELF: &str = "esp32s3_rev0_rom.elf";
/// Tops of the ROM's boot stacks (ends of .stack_pro / .stack_app).
const ROM_STACK_PRO: u32 = 0x3FCE_B710;
const ROM_STACK_APP: u32 = 0x3FCE_D710;
const SERVICE_NS: u64 = 500_000;
const QUANTUM: u64 = 64;
/// Return address for calls the simulator makes that must never return
/// (boot entry points) or that return to the ROM (the wake stub).
const MAGIC_BOOT_RETURN: u32 = MAGIC_BASE + 0x40;
/// PRID values of the PRO and APP cores.
const PRID: [u32; 2] = [0xCDCD, 0xABAB];

/// Start `core` at a windowed function as if the ROM had executed `CALL4 pc`:
/// fresh register window, SP in a1, return address (with call increment 1) in a4.
fn call_entry(c: &mut Xtensa, prid: u32, pc: u32, sp: u32, ret: u32) {
    c.reset();
    c.prid = prid;
    c.set_window(0, 1);
    c.set_a(1, sp);
    c.set_a(4, 1 << 30 | (ret & 0x3FFF_FFFF));
    // PS: WOE (bit 18), CALLINC = 1 (bits 16-17), INTLEVEL 0, EXCM clear
    c.set_ps(1 << 18 | 1 << 16);
    c.set_pc(pc);
}

fn new_core(prid: u32) -> Xtensa {
    let mut c = Xtensa::new();
    c.prid = prid;
    c
}

pub struct Esp32s3 {
    pub cores: [Xtensa; 2],
    core_running: [bool; 2],
    /// Core executed WAITI and sleeps until an interrupt is deliverable.
    parked: [bool; 2],
    pub bus: S3Bus,
    rom_sections: Vec<(u32, Vec<u8>)>,
    rom_syms: Symbols,
    apps: Vec<([u8; 32], Symbols, String)>,
    /// Where to look for the ELF of an app booted without one.
    elf_search: firmware::ElfSearch,
    active_app: Option<usize>,
    /// Flash offset of the app booted last (to report a boot from the other OTA slot).
    boot_offset: Option<u32>,
    trace: Vec<String>,
    pending_halt: Option<String>,
    recent_resets: Vec<u64>,
    syms: Symbols,
    hooks: Hooks,
    requests: Vec<MachineRequest>,
    sim_msgs: Vec<(usize, String)>,
    samples: HashMap<u32, u64>,
    next_sample: u64,
    boots: u32,
    hle: HleState,
    /// Light sleep in progress: (wake time, RTC_*_TRIG_EN wake sources).
    light_sleep: Option<(Option<u64>, u32)>,
    /// Where the APP core starts (from `ets_set_appcpu_boot_addr`).
    appcpu_boot_addr: Option<u32>,
    /// Waiting for the deep-sleep wake stub to return before loading the bootloader.
    in_wake_stub: bool,
    /// Instructions retired (both cores).
    retired: u64,
    /// Executed instructions (both cores), when recording code coverage.
    coverage: Option<Box<Coverage>>,
}

struct NullBoard;
impl Board for NullBoard {
    fn gpio_out(&mut self, _: u64, _: u64, _: u64) {}
    fn gpio_in(&mut self, _: u64) -> (u64, u64) {
        (0, 0)
    }
    fn spi_transfer(&mut self, _: u64, _: u8, _: &[u8], n: usize) -> Vec<u8> {
        vec![0; n]
    }
    fn adc_millivolts(&mut self, _: u8) -> u32 {
        0
    }
    fn next_event_ns(&self, _: u64) -> Option<u64> {
        None
    }
    fn update(&mut self, _: u64) {}
    fn info(&self) -> sim_api::BoardInfo {
        Default::default()
    }
    fn set_button(&mut self, _: bool) {}
    fn set_battery_mv(&mut self, _: u32) {}
    fn display_status(&self, _: u64) -> (bool, u64) {
        (false, 0)
    }
}

impl GuestMem for S3Bus {
    fn read_bytes(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
        self.peek_bytes(addr, len)
    }
    fn write_bytes(&mut self, addr: u32, data: &[u8]) -> bool {
        self.load_bytes(addr, data)
    }
    fn read_u32(&self, addr: u32) -> Option<u32> {
        self.peek32(addr)
    }
    fn memcheck(&mut self) -> Option<&mut Memcheck> {
        self.mc.as_deref_mut()
    }
}

struct Env<'a> {
    now: u64,
    board: &'a mut dyn Board,
    requests: &'a mut Vec<MachineRequest>,
    msgs: &'a mut Vec<(usize, String)>,
    uart_pos: usize,
}

impl HleEnv for Env<'_> {
    fn now_ns(&self) -> u64 {
        self.now
    }
    fn adc_millivolts(&mut self, gpio: u8) -> u32 {
        self.board.adc_millivolts(gpio)
    }
    fn console(&mut self, msg: &str) {
        self.msgs.push((self.uart_pos, msg.to_string()));
    }
    fn request(&mut self, r: MachineRequest) {
        self.requests.push(r);
    }
}

impl Esp32s3 {
    /// FreeRTOS: name of the task running on `core` (pcTaskName at TCB+0x34 in IDF 5).
    fn current_task(&self, core: usize) -> Option<String> {
        let tcb = self.bus.peek32(self.syms.addr("pxCurrentTCBs")? + 4 * core as u32)?;
        let name: String = (0..16)
            .map_while(|k| self.bus.peek_bytes(tcb + 0x34 + k, 1).map(|b| b[0]).filter(|&b| b != 0))
            .map(|b| b as char)
            .collect();
        Some(format!("{name:?} (tcb {tcb:#x})"))
    }

    /// Windowed-ABI call chain: live frames from the register file, spilled ones from the stack.
    fn backtrace(&self, c: &Xtensa) -> String {
        let mut out = vec![self.syms.describe(c.pc())];
        let (mut pc, mut a0, mut sp) = (c.pc(), c.a(0), c.a(1));
        let mut wb = c.windowbase();
        let mut live = true;
        for _ in 0..24 {
            let inc = a0 >> 30;
            if inc == 0 || a0 == 0 {
                break;
            }
            pc = (a0 & 0x3fff_ffff) | (pc & 0xc000_0000);
            if pc & 0x3fff_ffff == crate::arch::xtensa::SYNTH_RET_PC & 0x3fff_ffff {
                // An HLE guest call: its frame records where the hook resumes.
                let caller_sp = if live && c.windowstart() >> ((wb + 16 - inc) & 15) & 1 != 0 {
                    c.ar[(((wb + 16 - inc) & 15) * 4 + 1) as usize]
                } else {
                    self.bus.peek32(sp.wrapping_sub(12)).unwrap_or(0)
                };
                let ret = self.bus.peek32(caller_sp + 32).unwrap_or(0);
                out.push(format!("<hle call from {} sp={caller_sp:#x}>", self.syms.describe(ret)));
                break;
            }
            out.push(self.syms.describe(pc));
            let cwb = (wb + 16 - inc) & 15;
            if live && c.windowstart() >> cwb & 1 != 0 && cwb != c.windowbase() {
                a0 = c.ar[(cwb * 4) as usize];
                sp = c.ar[(cwb * 4 + 1) as usize];
                wb = cwb;
            } else {
                live = false;
                match (self.bus.peek32(sp.wrapping_sub(16)), self.bus.peek32(sp.wrapping_sub(12))) {
                    (Some(r), Some(s)) => (a0, sp) = (r, s),
                    _ => break,
                }
            }
        }
        out.join(" <- ")
    }

    pub fn new(
        rom_elf: &[u8],
        flash: SpiFlash,
        board: Box<dyn Board>,
        apps: Vec<([u8; 32], Symbols, String)>,
        elf_search: firmware::ElfSearch,
        trace: &[String],
    ) -> anyhow::Result<Self> {
        let rom_sections = firmware::rom_sections(rom_elf)?;
        let rom_syms = Symbols::from_elf(rom_elf)?;
        let mut rom = vec![0u8; bus::ROM_SIZE].into_boxed_slice();
        for (addr, data) in &rom_sections {
            let off = match *addr {
                a if (bus::ROM_BASE..bus::ROM_BASE + bus::ROM_SIZE as u32).contains(&a) => (a - bus::ROM_BASE) as usize,
                a if (bus::DROM_ROM_BASE..bus::DROM_ROM_BASE + 0x2_0000).contains(&a) => {
                    (a - bus::DROM_ROM_BASE) as usize + 0x4_0000
                }
                _ => continue,
            };
            let end = (off + data.len()).min(rom.len());
            rom[off..end].copy_from_slice(&data[..end - off]);
        }
        let mut m = Esp32s3 {
            cores: [new_core(PRID[0]), new_core(PRID[1])],
            core_running: [false; 2],
            parked: [false; 2],
            bus: S3Bus::new(rom, flash, board),
            rom_sections,
            syms: rom_syms.clone(),
            rom_syms,
            apps,
            elf_search,
            active_app: None,
            boot_offset: None,
            trace: trace.to_vec(),
            pending_halt: None,
            recent_resets: Vec::new(),
            hooks: Hooks::default(),
            requests: Vec::new(),
            sim_msgs: Vec::new(),
            samples: HashMap::new(),
            next_sample: 0,
            boots: 0,
            hle: HleState::new(periph::Periph::new().mac),
            light_sleep: None,
            appcpu_boot_addr: None,
            in_wake_stub: false,
            retired: 0,
            coverage: None,
        };
        m.reset(ResetKind::PowerOn);
        Ok(m)
    }

    pub fn set_mac(&mut self, mac: [u8; 6]) {
        self.bus.p.mac = mac;
        let (cfg, portal) = (self.hle.wifi.net_config.clone(), self.hle.wifi.portal_forward);
        self.hle = HleState::new(mac);
        self.hle.wifi.set_net_config(cfg);
        self.hle.wifi.portal_forward = portal;
        self.reset(ResetKind::PowerOn);
        self.boots = 1;
    }

    /// Turn on `--memcheck` and restart from power-on (so the heap is followed from the start).
    /// The shadow covers SRAM (both bus aliases) and the 32 MB data-bus window PSRAM is
    /// mapped into.
    pub fn enable_memcheck(&mut self, mode: memcheck::Mode, suppressions: Vec<String>) {
        let sram = bus::SRAM_SIZE as u32;
        self.bus.mc = Some(Box::new(Memcheck::new(
            mode,
            &[
                (bus::IRAM_BASE, sram, None),
                (bus::DRAM_BASE, bus::DRAM_END - bus::DRAM_BASE, Some(bus::IRAM_BASE + 0x8000)),
                (0x3C00_0000, 0x200_0000, None),
            ],
        )));
        if let Some(mc) = self.bus.mc.as_deref_mut() {
            mc.suppressions = suppressions;
        }
        self.active_app = None;
        self.boot_offset = None;
        self.bus.p.console_out.clear();
        self.reset(ResetKind::PowerOn);
        self.boots = 1;
    }

    /// A load or store of `core` hit poisoned memory: report it, unless the allocator did it.
    #[cold]
    fn memcheck_poll(&mut self, core: usize, pc: u32) -> Option<SliceExit> {
        let mut mc = self.bus.mc.take()?;
        let mut exit = None;
        if let Some(a) = mc.pending.take()
            && !mc.is_exempt(pc, &a)
        {
            let c = &self.cores[core];
            let mut frames = c.call_chain(&|a| self.bus.peek32(a), memcheck::FRAMES, false);
            frames[0] = pc;
            let tcb = memcheck::current_tcb_addr(&self.syms, core).and_then(|a| self.bus.peek32(a));
            let site = memcheck::Site { frames, tcb, core };
            let v = mc.access_violation(&a, &site, &self.syms, self.bus.now_ns());
            let summary = v.summary();
            if let Some((lines, counts)) = mc.record(v) {
                for l in lines {
                    self.msg(l);
                }
                if counts && mc.mode == memcheck::Mode::Halt {
                    exit = Some(SliceExit::Halted(format!("memcheck: {summary}")));
                }
            }
        }
        self.bus.mc = Some(mc);
        exit
    }

    /// Refresh the tasks' stack high-water marks.
    fn memcheck_scan_stacks(&mut self) {
        if let Some(mut mc) = self.bus.mc.take() {
            mc.scan_stacks(|a, n| self.bus.peek_bytes(a, n));
            self.bus.mc = Some(mc);
        }
    }

    pub fn set_net_config(&mut self, cfg: vnet::NetConfig) {
        self.hle.wifi.set_net_config(cfg);
    }

    pub fn set_portal_port(&mut self, port: u16) {
        self.hle.wifi.portal_forward = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    }

    fn msg(&mut self, s: String) {
        log::info!("{s}");
        self.sim_msgs.push((self.bus.p.console_out.len(), s));
    }

    fn select_app(&mut self) {
        let Some(app) = firmware::booting_app(&self.bus.flash.data) else {
            self.pending_halt = Some("no bootable app image in flash".into());
            return;
        };
        if self.boot_offset.is_some_and(|o| o != app.offset) {
            // the bootloader's own "Loaded app from partition" line isn't printed on every chip
            self.msg(format!("booting the app in the other slot, at {:#x}", app.offset));
        }
        self.boot_offset = Some(app.offset);
        self.hle.wifi.set_abi(hle::wifi::WifiAbi::for_idf(&app.idf_version));
        let sha = app.elf_sha256;
        let i = match self.apps.iter().position(|a| a.0 == sha) {
            Some(i) => i,
            None => match self.elf_search.find(&sha) {
                Some(a) => {
                    self.msg(format!("found the ELF of the app at {:#x}: {}", app.offset, a.name));
                    self.apps.push((a.elf_sha256, a.symbols, a.name));
                    self.apps.len() - 1
                }
                None => {
                    self.pending_halt = Some(self.elf_search.not_found(app.offset, &app.version, &sha));
                    return;
                }
            },
        };
        if self.active_app == Some(i) {
            self.cover_app();
            return;
        }
        let mut syms = self.rom_syms.clone();
        syms.merge(&self.apps[i].1);
        let mut hooks = Hooks::default();
        hle::idf::install(&mut hooks, &syms);
        if let Some(mc) = self.bus.mc.as_deref_mut() {
            hle::memcheck::install(&mut hooks, &syms, mc);
        }
        hooks.install(&syms, "ets_set_appcpu_boot_addr", hook_appcpu_boot_addr);
        hooks.install(&syms, "s_test_psram", hook_skip_psram_test);
        hooks.trampoline(MAGIC_BOOT_RETURN, "boot/wake-stub return", trampoline_boot_return);
        for name in &self.trace {
            if !hooks.trace(&syms, name) {
                log::warn!("--trace: no symbol named {name}");
            }
        }
        if self.active_app.is_some() {
            let name = self.apps[i].2.clone();
            self.msg(format!("booting a different app at {:#x}: {} ({name})", app.offset, app.version));
        }
        self.syms = syms;
        self.hooks = hooks;
        self.active_app = Some(i);
        self.cover_app();
    }

    /// Point coverage recording at the app about to run.
    fn cover_app(&mut self) {
        if let (Some(cov), Some(i)) = (self.coverage.as_mut(), self.active_app) {
            cov.activate(i, self.hooks.replaced());
        }
    }

    /// ROM behaviour after reset on the PRO core: run the deep-sleep wake stub
    /// if one is registered, then load and start the 2nd-stage bootloader.
    fn rom_boot(&mut self) -> anyhow::Result<()> {
        for (addr, data) in &self.rom_sections {
            if (bus::DRAM_BASE..bus::DRAM_END).contains(addr) {
                self.bus.load_bytes(*addr, data);
            }
        }
        self.cores = [new_core(PRID[0]), new_core(PRID[1])];
        self.core_running = [true, false];
        self.parked = [false; 2];
        let stub = self.bus.p.store_get(0x6000_8000 + 0xC8); // RTC_CNTL_STORE6: wake stub entry
        if self.bus.p.reset_reason == ResetReason::DeepSleep && stub != 0 && self.bus.peek32(stub).is_some() {
            self.in_wake_stub = true;
            call_entry(&mut self.cores[0], PRID[0], stub, ROM_STACK_PRO, MAGIC_BOOT_RETURN);
            return Ok(());
        }
        self.start_bootloader()
    }

    fn start_bootloader(&mut self) -> anyhow::Result<()> {
        self.in_wake_stub = false;
        let (entry, segs) = firmware::parse_image(&self.bus.flash.data, 0)?;
        let rst = self.bus.p.reset_reason;
        let mut banner = format!(
            "ESP-ROM:esp32s3-20210327\r\nBuild:Mar 27 2021\r\nrst:{:#x} ({}),boot:0x8 (SPI_FAST_FLASH_BOOT)\r\n",
            rst as u32,
            match rst {
                ResetReason::PowerOn => "POWERON",
                ResetReason::DeepSleep => "DSLEEP",
                ResetReason::SwSys => "RTC_SW_SYS_RST",
                ResetReason::SwCpu => "RTC_SW_CPU_RST",
            }
        );
        for (addr, data) in &segs {
            if !self.bus.load_bytes(*addr, data) {
                anyhow::bail!("bootloader segment at {addr:#x} is not in RAM");
            }
            banner += &format!("load:{:#010x},len:{:#x}\r\n", addr, data.len());
        }
        banner += &format!("entry {entry:#010x}\r\n");
        // The ROM prints on UART0 (on the TRMNL X that line goes to the modem).
        let now = self.bus.now_ns();
        self.bus.board.uart_tx(now, 0, banner.as_bytes());
        self.bus.p.console_out.extend(banner.bytes());
        call_entry(&mut self.cores[0], PRID[0], entry, ROM_STACK_PRO, MAGIC_BOOT_RETURN);
        Ok(())
    }

    fn start_app_cpu(&mut self) {
        let Some(addr) = self.appcpu_boot_addr else {
            return;
        };
        call_entry(&mut self.cores[1], PRID[1], addr, ROM_STACK_APP, MAGIC_BOOT_RETURN);
        self.core_running[1] = true;
        self.parked[1] = false;
        log::info!("APP CPU started at {addr:#x}");
    }

    fn service(&mut self) {
        let now = self.bus.now_ns();
        self.bus.systimer_update(now);
        self.bus.board.update(now);
        self.bus.gpio_sample(now);
        self.bus.uart_poll(now);
        self.bus.usb_sof(now);
        let mut next = now + SERVICE_NS;
        if let Some(t) = self.bus.p.systimer.next_ns(now) {
            next = next.min(t);
        }
        if let Some(t) = self.bus.board.next_event_ns(now) {
            next = next.min(t.max(now + 1));
        }
        self.bus.next_event = self.bus.clock.cycles_at(next);
        self.bus.irq_dirty = true;
        if self.bus.clock.cycles >= self.next_sample {
            *self.samples.entry(self.cores[0].pc()).or_default() += 1;
            if self.core_running[1] {
                *self.samples.entry(self.cores[1].pc()).or_default() += 1;
            }
            self.next_sample = self.bus.clock.cycles + 200_000;
        }
        if self.bus.mc.as_deref_mut().is_some_and(|mc| mc.stack_scan_due(now)) {
            self.memcheck_scan_stacks();
        }
    }

    fn update_irq(&mut self) {
        self.bus.irq_dirty = false;
        self.bus.refresh_sources();
        for core in 0..2 {
            let lines = self.bus.p.intc.lines(core);
            self.cores[core].set_irq_lines(lines);
        }
    }

    fn run_hook(&mut self, core: usize) -> bool {
        let mut board = std::mem::replace(&mut self.bus.board, Box::new(NullBoard));
        let now = self.bus.now_ns();
        let uart_pos = self.bus.p.console_out.len();
        let redirected = {
            let mut env =
                Env { now, board: board.as_mut(), requests: &mut self.requests, msgs: &mut self.sim_msgs, uart_pos };
            let mut ctx = HleCtx {
                cpu: &mut self.cores[core],
                mem: &mut self.bus,
                env: &mut env,
                syms: &self.syms,
                state: &mut self.hle,
            };
            self.hooks.dispatch(&mut ctx)
        };
        self.bus.board = board;
        redirected
    }

    fn handle_reset_request(&mut self, r: ResetRequest) {
        match r {
            ResetRequest::AppCpu => {
                // Out of reset the APP core waits in ROM for a new boot address. Without
                // this it would restart at the old one while esp_restart_noos (running on
                // core 0) disables the caches under it.
                self.core_running[1] = false;
                self.appcpu_boot_addr = None;
                return;
            }
            ResetRequest::System | ResetRequest::ProCpu => {}
        }
        self.msg(format!("software reset ({r:?})"));
        let now = self.bus.now_ns();
        self.recent_resets.retain(|t| now - *t < 2_000_000_000);
        self.recent_resets.push(now);
        if self.recent_resets.len() > 10 {
            self.pending_halt = Some("reset loop: more than 10 software resets within 2 s".into());
        }
        self.bus.p.reset_reason = if r == ResetRequest::System { ResetReason::SwSys } else { ResetReason::SwCpu };
        self.reset_internal(false);
    }

    fn reset_internal(&mut self, power_on: bool) {
        self.bus.flash.flush().ok();
        self.bus.flash.restore_power();
        let reason = self.bus.p.reset_reason;
        let wake = self.bus.p.wakeup_cause;
        self.bus.p.chip_reset(!power_on);
        self.bus.p.reset_reason = reason;
        self.bus.p.wakeup_cause = wake;
        let now = self.bus.now_ns();
        self.bus.p.rtc_ticks_base = self.bus.rtc_ticks(now);
        let cycles = self.bus.clock.cycles;
        self.bus.clock = crate::soc::esp32c3::bus::Clock::new(40_000_000);
        self.bus.clock.cycles = cycles;
        self.bus.clock.set_base(now);
        self.bus.p.systimer.reset_counters(now);
        self.bus.sram.fill(0);
        if power_on {
            self.bus.rtc_slow.fill(0);
            self.bus.rtc_fast.fill(0);
        }
        // The cache MMU keeps its mappings through deep sleep: the TRMNL X wake stub reads
        // a pin-mux table from flash .rodata before the bootloader remaps anything.
        if reason != ResetReason::DeepSleep {
            self.bus.mmu.fill(bus::MMU_INVALID);
        }
        self.hooks.clear_pending();
        self.hle.chip_reset();
        if let Some(mc) = self.bus.mc.as_deref_mut() {
            mc.chip_reset();
        }
        self.light_sleep = None;
        self.appcpu_boot_addr = None;
        self.boots += 1;
        self.select_app();
        if let Err(e) = self.rom_boot() {
            self.msg(format!("boot failed: {e:#}"));
        }
        self.service();
    }

    /// Run one core for up to `n` instructions. Returns the number executed and
    /// whether it is still busy (false once it waits for an interrupt).
    fn run_core(&mut self, core: usize, n: u64) -> Result<(u64, bool), SliceExit> {
        self.bus.core = core;
        let mut done = 0;
        while done < n {
            let pc = self.cores[core].pc();
            if self.hooks.maybe(pc) && self.run_hook(core) {
                if !self.requests.is_empty() || self.hle.wake_stub_returned {
                    return Ok((done, true));
                }
                continue;
            }
            if let Some(cov) = &mut self.coverage {
                cov.hit(pc);
            }
            let step = self.cores[core].step(&mut self.bus);
            match step {
                Step::Ok => {}
                Step::Wfi => return Ok((done, false)),
                Step::Exception(e) => {
                    log::debug!("core {core}: exception {e:?} at {}", self.syms.describe(pc));
                }
                Step::Trap(Trap::Unimplemented { pc, raw, len }) => {
                    self.msg(format!(
                        "core {core}: unimplemented instruction {raw:0w$x} at {} -> IllegalInstruction",
                        self.syms.describe(pc),
                        w = len as usize * 2
                    ));
                    self.cores[core].raise_exception(cause::ILLEGAL, 0);
                }
                Step::Trap(Trap::Break { pc, .. }) => {
                    let desc = format!("core {core}: BREAK at {}", self.syms.describe(pc));
                    return Err(SliceExit::Halted(format!("{desc}\n{}", self.debug_dump())));
                }
                Step::Trap(Trap::HleCall { pc }) => {
                    let desc = format!("core {core}: stray HLE pseudo-PC {pc:#x}");
                    return Err(SliceExit::Halted(format!("{desc}\n{}", self.debug_dump())));
                }
            }
            done += 1;
            self.retired += 1;
            if self.bus.irq_dirty {
                self.update_irq();
                if self.bus.mc.as_ref().is_some_and(|mc| mc.pending.is_some())
                    && let Some(exit) = self.memcheck_poll(core, pc)
                {
                    return Err(exit);
                }
            }
            if self.bus.p.reset_request.is_some() || !self.requests.is_empty() {
                return Ok((done, true));
            }
        }
        Ok((done, true))
    }
}

/// `ets_set_appcpu_boot_addr(addr)`: remember where the APP core should start.
fn hook_appcpu_boot_addr(c: &mut HleCtx) -> hle::Flow {
    let addr = c.cpu.arg(0);
    c.state.appcpu_boot_addr = Some(addr);
    hle::Flow::Continue
}

/// `s_test_psram(...)` (IDF esp_psram.c): the boot-time test writes and reads back a word every
/// 32 bytes of PSRAM, on every boot and deep-sleep wake. Emulated PSRAM can't fail it, so pass
/// without running it.
fn hook_skip_psram_test(_c: &mut HleCtx) -> hle::Flow {
    hle::Flow::Return(Some(1))
}

/// A boot entry point returned (it shouldn't), or the wake stub finished.
fn trampoline_boot_return(c: &mut HleCtx) -> hle::Flow {
    c.state.wake_stub_returned = true;
    hle::Flow::Redirected
}

impl Machine for Esp32s3 {
    fn run_slice(&mut self, until_ns: u64) -> SliceExit {
        if let Some(m) = self.pending_halt.take() {
            return SliceExit::Halted(m);
        }
        let until = self.bus.clock.cycles_at(until_ns);
        loop {
            if self.bus.clock.cycles >= until {
                return SliceExit::Reached;
            }
            if let Some((wake_at, opt)) = self.light_sleep {
                let now = self.bus.now_ns();
                self.bus.gpio_sample(now);
                let wake_gpio = opt & (1 << 2) != 0 && self.bus.gpio_wake_triggered();
                let wake_timer = opt & (1 << 3) != 0 && wake_at.is_some_and(|t| now >= t);
                if wake_timer || wake_gpio {
                    self.light_sleep = None;
                    self.bus.p.wakeup_cause = if wake_gpio { 1 << 2 } else { 1 << 3 };
                } else {
                    let target = wake_at.map_or(until, |t| until.min(self.bus.clock.cycles_at(t)));
                    self.bus.clock.cycles = self.bus.clock.cycles.max(target);
                    self.service();
                    continue;
                }
            }
            let stop = until.min(self.bus.next_event);
            while self.bus.clock.cycles < stop {
                let q = QUANTUM.min(stop - self.bus.clock.cycles);
                if self.bus.irq_dirty {
                    self.update_irq();
                }
                let mut busy = false;
                let mut executed = [0u64; 2];
                for core in 0..2 {
                    if !self.core_running[core] {
                        continue;
                    }
                    if self.parked[core] {
                        if !self.cores[core].irq_pending() {
                            continue;
                        }
                        self.parked[core] = false;
                    }
                    match self.run_core(core, q) {
                        Ok((n, b)) => {
                            executed[core] = n;
                            busy |= b;
                            self.parked[core] = !b;
                        }
                        Err(exit) => return exit,
                    }
                    if let Some(msg) = self.bus.flash.power_lost() {
                        return SliceExit::PowerLoss(msg.to_string());
                    }
                    if self.bus.p.reset_request.is_some() || !self.requests.is_empty() {
                        break;
                    }
                }
                if self.hle.wake_stub_returned {
                    self.hle.wake_stub_returned = false;
                    if self.in_wake_stub
                        && let Err(e) = self.start_bootloader()
                    {
                        return SliceExit::Halted(format!("boot failed: {e:#}"));
                    }
                }
                if let Some(addr) = self.hle.appcpu_boot_addr.take() {
                    self.appcpu_boot_addr = Some(addr);
                }
                if !self.core_running[1] && self.appcpu_boot_addr.is_some() && self.bus.p.core1_running() {
                    self.start_app_cpu();
                }
                if let Some(r) = self.bus.p.reset_request.take() {
                    self.handle_reset_request(r);
                    break;
                }
                if !self.requests.is_empty() {
                    break;
                }
                // When every core sleeps, jump to the next SoC event or core timer.
                let idle_jump = (0..2)
                    .filter(|&c| self.core_running[c])
                    .map(|c| self.cores[c].cycles_to_timer().max(1))
                    .fold(stop - self.bus.clock.cycles, u64::min);
                let advance = if busy { q } else { idle_jump };
                self.bus.clock.cycles += advance;
                // Keep each core's CCOUNT in step with shared time (firing CCOMPARE timers).
                for core in 0..2 {
                    let idle = advance.saturating_sub(executed[core]);
                    if idle > 0 {
                        self.cores[core].advance_ccount(idle);
                    }
                }
            }
            for r in std::mem::take(&mut self.requests) {
                match r {
                    MachineRequest::DeepSleep { timer_ns, gpio_low_mask } => {
                        // esp_sleep_start (skipped by the HLE hook) points the ROM at the
                        // wake stub trampoline, which calls the handler registered with
                        // esp_set_deep_sleep_wake_stub (IDF 5: RTC_CNTL_STORE6).
                        if let Some(entry) = self.syms.addr("esp_wake_stub_entry") {
                            self.bus.p.store_set(0x6000_8000 + 0xC8, entry);
                        }
                        self.bus.flash.flush().ok();
                        return SliceExit::DeepSleep { timer_ns, gpio_low_mask };
                    }
                    MachineRequest::Halt(m) => return SliceExit::Halted(m),
                    MachineRequest::LightSleep { timer_ns, gpio } => {
                        let now = self.bus.now_ns();
                        let opt = if gpio { 1 << 2 } else { 0 } | if timer_ns.is_some() { 1 << 3 } else { 0 };
                        self.light_sleep = Some((timer_ns.map(|t| now + t), opt));
                    }
                    MachineRequest::RtcSleep { wakeup_opt } => {
                        let now = self.bus.now_ns();
                        let wake_at = self.bus.rtc_sleep_target_ns(now);
                        self.light_sleep = Some((wake_at, wakeup_opt));
                    }
                }
            }
            if self.bus.clock.cycles >= self.bus.next_event {
                self.service();
            }
        }
    }

    fn reset(&mut self, kind: ResetKind) {
        match kind {
            ResetKind::PowerOn => {
                self.bus.p.reset_reason = ResetReason::PowerOn;
                self.bus.p.wakeup_cause = 0;
                self.bus.p.rtc_ticks_base = 0;
                self.reset_internal(true);
            }
            ResetKind::ResetPin => {
                self.bus.p.reset_reason = ResetReason::PowerOn;
                self.bus.p.wakeup_cause = 0;
                self.reset_internal(false);
            }
            ResetKind::DeepSleepWake { by_timer, by_gpio } => {
                self.bus.p.reset_reason = ResetReason::DeepSleep;
                // S3 wake causes: EXT0 (RTC GPIO) = BIT(0), timer = BIT(3), GPIO = BIT(2)
                self.bus.p.wakeup_cause = (by_gpio as u32) | (by_timer as u32) << 3;
                self.reset_internal(false);
            }
        }
    }

    fn now_ns(&self) -> u64 {
        self.bus.now_ns()
    }

    fn advance_time(&mut self, ns: u64) {
        let t = self.bus.now_ns() + ns;
        self.bus.clock.advance_to_ns(t);
        self.bus.board.update(t);
    }

    fn instructions(&self) -> u64 {
        self.retired
    }

    fn board(&mut self) -> &mut dyn Board {
        self.bus.board.as_mut()
    }

    fn take_output(&mut self) -> Vec<Output> {
        let console = std::mem::take(&mut self.bus.p.console_out);
        let mut out = Vec::new();
        let mut pos = 0;
        for (at, msg) in std::mem::take(&mut self.sim_msgs) {
            let at = at.min(console.len());
            if at > pos {
                out.push(Output::Serial(console[pos..at].to_vec()));
                pos = at;
            }
            out.push(Output::Sim(msg));
        }
        if pos < console.len() {
            out.push(Output::Serial(console[pos..].to_vec()));
        }
        out
    }

    fn flush(&mut self) {
        if let Err(e) = self.bus.flash.flush() {
            log::error!("saving flash: {e}");
        }
    }

    fn debug_dump(&self) -> String {
        let mut s = format!(
            "light_sleep: {:?} gpio_in={:#x} gpio38 pin reg={:#x} wakeup_cause={:#x}\n",
            self.light_sleep,
            self.bus.p.gpio_in,
            self.bus.p.store_get(0x6000_4000 + 0x74 + 4 * 38),
            self.bus.p.wakeup_cause
        );
        for (i, c) in self.cores.iter().enumerate() {
            if i == 1 && !self.core_running[1] {
                continue;
            }
            s += &format!("--- core {i}: pc {}\n{}", self.syms.describe(c.pc()), c.gpr_dump());
            if let Some(t) = self.current_task(i) {
                s += &format!("  task: {t}\n");
            }
            // SIM_PEEK=addr[,addr...] (hex): extra memory words to include in the dump.
            if let Ok(w) = std::env::var("SIM_PEEK") {
                for a in w.split(',').filter_map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok()) {
                    s += &format!("  peek {a:#x} = {:?}\n", self.bus.peek32(a).map(|v| format!("{v:#010x}")));
                }
            }
            let a2 = c.a(2);
            s += &format!("  *a2 ({a2:#x}) = {:?}\n", self.bus.peek32(a2).map(|v| format!("{v:#010x}")));
            s += &format!("  backtrace: {}\n", self.backtrace(c));
        }
        s
    }

    fn profile(&mut self) -> Vec<(String, u64)> {
        let mut by_sym: HashMap<String, u64> = HashMap::new();
        for (pc, n) in self.samples.drain() {
            let d = self.syms.describe(pc);
            let name = d.split('+').next().unwrap_or(&d).to_string();
            *by_sym.entry(name).or_default() += n;
        }
        let mut v: Vec<_> = by_sym.into_iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        v
    }

    fn boot_count(&self) -> u32 {
        self.boots
    }

    fn set_wifi_available(&mut self, on: bool) {
        let now = self.bus.now_ns();
        self.hle.wifi.set_available(on, now);
        self.bus.board.set_wifi_available(on);
    }

    /// The S3's radio only sees the 2.4 GHz access points (channels 1-14); a radio on the
    /// board (the X's modem) may see them all.
    fn set_wifi_networks(&mut self, networks: &[sim_api::WifiNetwork]) {
        let now = self.bus.now_ns();
        let own: Vec<_> = networks.iter().filter(|n| n.channel <= 14).cloned().collect();
        self.hle.wifi.set_networks(&own, now);
        self.bus.board.set_wifi_networks(networks);
    }

    fn set_portal_client(&mut self, on: bool) {
        let now = self.bus.now_ns();
        self.hle.wifi.set_portal_client(on, now);
    }

    fn bluetooth(&mut self) -> &mut crate::hle::bluetooth::BluetoothState {
        &mut self.hle.bluetooth
    }

    fn net_status(&self) -> NetStatus {
        let w = &self.hle.wifi;
        NetStatus { connected: w.is_connected(), ip: w.ip().map(|i| i.to_string()), portal_url: w.portal_url() }
    }

    fn set_coverage(&mut self, cov: Coverage) {
        self.coverage = Some(Box::new(cov));
        self.cover_app();
    }

    fn install_firmware(&mut self, fw: &firmware::Firmware) -> anyhow::Result<()> {
        let data = firmware::install(&self.bus.flash.data, firmware::CHIP_ESP32S3, fw)?;
        self.bus.flash.replace_image(data)?;
        self.add_app(fw.elf_sha256, fw.symbols.clone(), fw.name.clone());
        Ok(())
    }

    fn add_app(&mut self, sha: [u8; 32], symbols: Symbols, name: String) {
        if !self.apps.iter().any(|a| a.0 == sha) {
            self.apps.push((sha, symbols, name));
        }
    }

    fn coverage(&mut self) -> Option<&mut Coverage> {
        self.coverage.as_deref_mut()
    }

    fn light_sleep(&self) -> Option<Option<u64>> {
        self.light_sleep.map(|(w, opt)| w.filter(|_| opt & (1 << 3) != 0))
    }

    fn memcheck_json(&mut self) -> Option<String> {
        self.memcheck_scan_stacks();
        self.bus.mc.as_ref().map(|mc| mc.json().to_string())
    }

    fn memcheck_summary(&mut self) -> Option<String> {
        self.memcheck_scan_stacks();
        self.bus.mc.as_ref().map(|mc| mc.summary())
    }

    fn realtime_required(&self) -> bool {
        // A light sleep without a timer only ends on an external event (e.g. docking):
        // fast-forwarding it would just burn virtual time.
        self.waiting_for_external() || self.hle.wifi.net_busy() || self.bus.board.realtime_required()
    }
    fn waiting_for_external(&self) -> bool {
        matches!(self.light_sleep, Some((w, opt)) if w.is_none() || opt & (1 << 3) == 0)
    }
    fn save_soc(&mut self, rtc: bool) -> SocState {
        SocState {
            mac: self.bus.p.mac,
            now_ns: self.bus.now_ns(),
            boots: self.boots,
            rtc_ticks_base: self.bus.p.rtc_ticks_base,
            rtc_regs: if rtc { self.bus.p.rtc_regs() } else { Vec::new() },
            rtc_mem: if rtc { vec![self.bus.rtc_slow.to_vec(), self.bus.rtc_fast.to_vec()] } else { Vec::new() },
            mmu: if rtc { self.bus.mmu.to_vec() } else { Vec::new() },
            flash: self.bus.flash.data.clone(),
        }
    }

    fn restore_soc(&mut self, s: &SocState) -> anyhow::Result<()> {
        if s.flash.len() != self.bus.flash.data.len() {
            anyhow::bail!("save point has {} bytes of flash, this device {}", s.flash.len(), self.bus.flash.data.len());
        }
        let rtc_ok = s.rtc_mem.is_empty()
            || (s.rtc_mem.len() == 2
                && s.rtc_mem[0].len() == self.bus.rtc_slow.len()
                && s.rtc_mem[1].len() == self.bus.rtc_fast.len());
        if !rtc_ok || !(s.mmu.is_empty() || s.mmu.len() == self.bus.mmu.len()) {
            anyhow::bail!("save point RTC memory / MMU doesn't fit an ESP32-S3");
        }
        self.bus.flash.data.copy_from_slice(&s.flash);
        self.bus.flash.dirty = true;
        self.bus.flash.flush()?;
        self.bus.p.mac = s.mac;
        self.hle.wifi.base_mac = s.mac;
        self.hle.bluetooth.controller.set_address(s.mac);
        self.hle.chip_reset();
        self.hooks.clear_pending();
        self.requests.clear();
        self.light_sleep = None;
        self.pending_halt = None;
        self.recent_resets.clear();
        self.appcpu_boot_addr = None;
        self.in_wake_stub = false;
        self.bus.p.chip_reset(false);
        self.bus.p.set_rtc_regs(&s.rtc_regs);
        self.bus.p.rtc_ticks_base = s.rtc_ticks_base;
        self.bus.rtc_slow.fill(0);
        self.bus.rtc_fast.fill(0);
        if let [slow, fast] = &s.rtc_mem[..] {
            self.bus.rtc_slow.copy_from_slice(slow);
            self.bus.rtc_fast.copy_from_slice(fast);
        }
        self.bus.mmu.fill(bus::MMU_INVALID);
        if !s.mmu.is_empty() {
            self.bus.mmu.copy_from_slice(&s.mmu);
        }
        let cycles = self.bus.clock.cycles;
        self.bus.clock = crate::soc::esp32c3::bus::Clock::new(40_000_000);
        self.bus.clock.cycles = cycles;
        self.bus.clock.set_base(s.now_ns);
        self.bus.p.systimer.reset_counters(s.now_ns);
        self.boots = s.boots;
        Ok(())
    }

    fn set_faults(&mut self, faults: &sim_api::Faults) -> Result<(), String> {
        self.bus.p.chip_temp_c = faults.chip_temp_c.unwrap_or(25.0);
        crate::faults::apply(faults, &mut self.bus.flash, &mut self.hle.wifi, self.bus.board.as_mut())
    }

    fn flash_stats(&self) -> (u64, u64) {
        (self.bus.flash.programs, self.bus.flash.erases)
    }

    fn change_preference(&mut self, change: &sim_api::PreferenceChange) -> Result<(), String> {
        let data = crate::nvs::prepare(&self.bus.flash.data, change)?;
        self.bus.flash.replace_image(data).map_err(|e| format!("saving flash: {e}"))
    }

    fn preferences(&self) -> sim_api::PreferencesSnapshot {
        crate::nvs::inspect(&self.bus.flash.data)
    }

    fn partitions(&self) -> Vec<sim_api::PartitionInfo> {
        firmware::partitions(&self.bus.flash.data)
    }
}
