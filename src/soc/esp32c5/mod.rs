//! ESP32-C5: single RV32IMAC core at up to 240 MHz with a CLIC interrupt controller.
//!
//! The chip family of the ESP32-C6/H2: clocks and resets in PCR, the low-power domain
//! (PMU, LP_CLKRST, LP_TIMER, LP_AON) keeps its registers and the 16 KB LP SRAM through
//! deep sleep, flash through SPI_MEM and a 512-entry cache MMU.

pub mod bus;
pub mod crypto;
pub mod periph;

use std::collections::HashMap;

use crate::arch::GuestCpu;
use crate::arch::riscv::{Rv32, Step};
use crate::board::Board;
use crate::coverage::Coverage;
use crate::devices::spi_flash::SpiFlash;
use crate::firmware::{self, Symbols};
use crate::hle::{self, GuestMem, HleCtx, HleEnv, HleState, Hooks, MachineRequest};
use crate::memcheck::{self, Memcheck};
use crate::savepoint::SocState;
use periph::ClicIrq;

use super::{Machine, NetStatus, Output, ResetKind, SliceExit};
use bus::C5Bus;
use periph::{ResetReason, ResetRequest};

/// The mask ROM of production (v1.x) silicon; `esp32c5_rev0_rom.elf` is the v0.x
/// engineering samples' and doesn't match IDF's ROM linker scripts for v1.0+.
pub const ROM_ELF: &str = "esp32c5_rev100_rom.elf";
/// Top of the ROM's boot stack (end of .stack_pro).
const ROM_STACK_TOP: u32 = 0x4085_E5A0;
/// How often (virtual time) peripherals/devices are serviced when nothing else is due.
const SERVICE_NS: u64 = 500_000;

pub struct Esp32c5 {
    pub cpu: Rv32,
    pub bus: C5Bus,
    rom_sections: Vec<(u32, Vec<u8>)>,
    rom_syms: Symbols,
    /// Known app builds: (ELF SHA-256, symbols, name). HLE hooks follow the one that boots.
    apps: Vec<([u8; 32], Symbols, String)>,
    active_app: Option<usize>,
    trace: Vec<String>,
    pending_halt: Option<String>,
    recent_resets: Vec<u64>,
    syms: Symbols,
    hooks: Hooks,
    irq: Option<ClicIrq>,
    requests: Vec<MachineRequest>,
    /// Simulator messages tagged with the serial output position they follow.
    sim_msgs: Vec<(usize, String)>,
    samples: HashMap<u32, u64>,
    next_sample: u64,
    boots: u32,
    trap_depth: u32,
    irq_counts: [u64; 48],
    hle: HleState,
    /// Light sleep in progress: (wake time, wake on GPIO).
    light_sleep: Option<(Option<u64>, bool)>,
    /// Executed instructions, when recording code coverage.
    coverage: Option<Box<Coverage>>,
}

/// Minimal board used while the real one is lent to HLE code.
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

impl GuestMem for C5Bus {
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

impl Esp32c5 {
    pub fn new(
        rom_elf: &[u8],
        flash: SpiFlash,
        board: Box<dyn Board>,
        apps: Vec<([u8; 32], Symbols, String)>,
        trace: &[String],
    ) -> anyhow::Result<Self> {
        let rom_sections = firmware::rom_sections(rom_elf)?;
        let rom_syms = Symbols::from_elf(rom_elf)?;
        let mut rom = vec![0u8; bus::ROM_SIZE].into_boxed_slice();
        for (addr, data) in &rom_sections {
            if !(bus::ROM_BASE..bus::ROM_BASE + bus::ROM_SIZE as u32).contains(addr) {
                continue;
            }
            let off = (addr - bus::ROM_BASE) as usize;
            let end = (off + data.len()).min(rom.len());
            rom[off..end].copy_from_slice(&data[..end - off]);
        }
        let mut m = Esp32c5 {
            cpu: new_cpu(),
            bus: C5Bus::new(rom, flash, board),
            rom_sections,
            syms: rom_syms.clone(),
            rom_syms,
            apps,
            active_app: None,
            trace: trace.to_vec(),
            pending_halt: None,
            recent_resets: Vec::new(),
            hooks: Hooks::default(),
            irq: None,
            requests: Vec::new(),
            sim_msgs: Vec::new(),
            samples: HashMap::new(),
            next_sample: 0,
            boots: 0,
            trap_depth: 0,
            irq_counts: [0; 48],
            hle: dual_band(HleState::new(periph::Periph::new().mac)),
            light_sleep: None,
            coverage: None,
        };
        m.reset(ResetKind::PowerOn);
        Ok(m)
    }

    /// Set the factory MAC (eFuse) and restart from power-on.
    pub fn set_mac(&mut self, mac: [u8; 6]) {
        self.bus.p.mac = mac;
        let (cfg, portal) = (self.hle.wifi.net_config.clone(), self.hle.wifi.portal_forward);
        self.hle = dual_band(HleState::new(mac));
        self.hle.wifi.set_net_config(cfg);
        self.hle.wifi.portal_forward = portal;
        self.reset(ResetKind::PowerOn);
        self.boots = 1;
    }

    pub fn set_net_config(&mut self, cfg: vnet::NetConfig) {
        self.hle.wifi.set_net_config(cfg);
    }

    pub fn set_portal_port(&mut self, port: u16) {
        self.hle.wifi.portal_forward = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    }

    /// Turn on `--memcheck` and restart from power-on (so the heap is followed from the start).
    pub fn enable_memcheck(&mut self, mode: memcheck::Mode, suppressions: Vec<String>) {
        // SRAM, and the 32 MB cache window PSRAM is mapped into
        self.bus.mc = Some(Box::new(Memcheck::new(
            mode,
            &[(bus::SRAM_BASE, bus::SRAM_SIZE as u32, None), (bus::EXT_BASE, 0x200_0000, None)],
        )));
        if let Some(mc) = self.bus.mc.as_deref_mut() {
            mc.suppressions = suppressions;
        }
        self.active_app = None;
        self.bus.p.console_out.clear();
        self.reset(ResetKind::PowerOn);
        self.boots = 1;
    }

    /// A load or store hit poisoned memory: report it, unless the allocator did it.
    #[cold]
    fn memcheck_poll(&mut self, pc: u32) -> Option<SliceExit> {
        let mut mc = self.bus.mc.take()?;
        let mut exit = None;
        if let Some(a) = mc.pending.take()
            && !mc.is_exempt(pc, &a)
        {
            let mut frames = self.cpu.backtrace(&|a| self.bus.peek32(a), memcheck::FRAMES);
            frames[0] = pc;
            let tcb = memcheck::current_tcb_addr(&self.syms, 0).and_then(|a| self.bus.peek32(a));
            let site = memcheck::Site { frames, tcb, core: 0 };
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

    /// Bind symbols and HLE hooks to the app the bootloader is about to start.
    fn select_app(&mut self) {
        let Some(app) = firmware::booting_app(&self.bus.flash.data) else {
            self.pending_halt = Some("no bootable app image in flash".into());
            return;
        };
        let (off, sha, version) = (app.offset, app.elf_sha256, app.version.clone());
        // Only IDF 5.x supports the C5.
        self.hle.wifi.set_abi(hle::wifi::WifiAbi::IDF_5_5_DUAL_BAND);
        let Some(i) = self.apps.iter().position(|a| a.0 == sha) else {
            self.pending_halt = Some(format!(
                "the app at {off:#x} (version {version}, ELF sha256 {}) has no matching ELF; \
                 HLE needs symbols. Pass it with --elf <firmware.elf>",
                firmware::hex(&sha)
            ));
            return;
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
        for name in &self.trace {
            if !hooks.trace(&syms, name) {
                log::warn!("--trace: no symbol named {name}");
            }
        }
        if self.active_app.is_some() {
            let name = self.apps[i].2.clone();
            self.msg(format!("booting a different app at {off:#x}: {version} ({name})"));
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

    fn msg(&mut self, s: String) {
        log::info!("{s}");
        self.sim_msgs.push((self.bus.p.console_out.len(), s));
    }

    /// The ROM's first boot stage: load the 2nd-stage bootloader from flash and jump to it.
    fn rom_boot(&mut self) -> anyhow::Result<()> {
        // ROM .data initial values; .bss is zero (SRAM was cleared).
        for (addr, data) in &self.rom_sections {
            if (bus::SRAM_BASE..bus::SRAM_BASE + bus::SRAM_SIZE as u32).contains(addr) {
                self.bus.load_bytes(*addr, data);
            }
        }
        let boot_off = firmware::bootloader_offset(firmware::CHIP_ESP32C5);
        let (entry, segs) = firmware::parse_image(&self.bus.flash.data, boot_off as usize)?;
        // Where the ROM found the (active) bootloader; the bootloader asks it back.
        if let Some(a) = self.rom_syms.addr("g_offset_of_active_bootloader") {
            self.bus.poke32(a, boot_off);
        }
        let rst = self.bus.p.reset_reason;
        let banner = format!(
            "ESP-ROM:esp32c5-eco2-20250121\r\nBuild:Jan 21 2025\r\nrst:{:#x} ({}),boot:0x18 (SPI_FAST_FLASH_BOOT)\r\n",
            rst as u32,
            match rst {
                ResetReason::PowerOn => "POWERON",
                ResetReason::DeepSleep => "DSLEEP",
                ResetReason::SwSys => "SW_SYS_RESET",
                ResetReason::SwCpu => "SW_CPU_RESET",
                _ => "OTHER",
            }
        );
        let mut out = banner.into_bytes();
        for (addr, data) in &segs {
            if !self.bus.load_bytes(*addr, data) {
                anyhow::bail!("bootloader segment at {addr:#x} is not in RAM");
            }
            out.extend(format!("load:{:#010x},len:{:#x}\r\n", addr, data.len()).bytes());
        }
        out.extend(format!("entry {entry:#010x}\r\n").bytes());
        self.bus.p.console_out.extend(out);
        self.cpu = new_cpu();
        self.cpu.pc = entry;
        self.cpu.x[2] = ROM_STACK_TOP;
        self.cpu.csr.mtvec = bus::ROM_BASE | 3;
        Ok(())
    }

    fn service(&mut self) {
        let now = self.bus.now_ns();
        self.bus.systimer_update(now);
        self.bus.board.update(now);
        self.bus.gpio_sample(now);
        self.bus.usb_sof(now);
        let mut next = now + SERVICE_NS;
        if let Some(t) = self.bus.systimer_next_ns(now) {
            next = next.min(t);
        }
        if let Some(t) = self.bus.board.next_event_ns(now) {
            next = next.min(t.max(now + 1));
        }
        self.bus.next_event = self.bus.clock.cycles_at(next);
        self.bus.irq_dirty = true;
        if self.bus.clock.cycles >= self.next_sample {
            *self.samples.entry(self.cpu.pc).or_default() += 1;
            self.next_sample = self.bus.clock.cycles + 100_000;
        }
        if self.bus.mc.as_deref_mut().is_some_and(|mc| mc.stack_scan_due(now)) {
            self.memcheck_scan_stacks();
        }
    }

    /// Take a CLIC interrupt: hardware vectoring reads the handler from the `mtvt` table.
    fn take_irq(&mut self, irq: ClicIrq) {
        let vector = if irq.shv { self.bus.peek32(self.cpu.csr.mtvt.wrapping_add(4 * irq.id)) } else { None };
        self.cpu.take_clic_interrupt(irq.id, irq.level, vector);
        self.bus.p.intc.taken(irq.id);
        self.bus.irq_dirty = true;
        self.irq_counts[irq.id as usize] += 1;
        self.trap_depth = 0;
    }

    #[inline]
    fn update_irq(&mut self) {
        self.bus.irq_dirty = false;
        self.bus.refresh_sources();
        self.irq = self.bus.p.intc.best();
    }

    fn run_hook(&mut self) -> bool {
        let board = std::mem::replace(&mut self.bus.board, Box::new(NullBoard));
        let mut board = board;
        let now = self.bus.now_ns();
        let redirected = {
            let uart_pos = self.bus.p.console_out.len();
            let mut env =
                Env { now, board: board.as_mut(), requests: &mut self.requests, msgs: &mut self.sim_msgs, uart_pos };
            let mut ctx = HleCtx {
                cpu: &mut self.cpu,
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

    fn handle_trap(&mut self, cause: u32, tval: u32) -> Option<SliceExit> {
        let pc = self.cpu.pc;
        self.trap_depth += 1;
        let desc = format!(
            "CPU exception {} at {} (mtval={tval:#x})",
            match cause {
                1 => "instruction access fault",
                2 => "illegal instruction",
                3 => "breakpoint",
                5 => "load access fault",
                7 => "store access fault",
                11 => "ecall",
                _ => "?",
            },
            self.syms.describe(pc)
        );
        if cause != 11 {
            self.msg(desc.clone());
        }
        if self.trap_depth > 4 || self.cpu.csr.mtvec & !3 == 0 {
            return Some(SliceExit::Halted(format!("{desc}\n{}", self.debug_dump())));
        }
        self.cpu.take_trap(cause, tval, pc);
        None
    }

    fn handle_reset_request(&mut self, r: ResetRequest) {
        self.msg(format!("software reset ({r:?})"));
        // A firmware stuck restarting is a failure worth surfacing, not spinning on.
        let now = self.bus.now_ns();
        self.recent_resets.retain(|t| now - *t < 2_000_000_000);
        self.recent_resets.push(now);
        if self.recent_resets.len() > 10 {
            self.pending_halt = Some("reset loop: more than 10 software resets within 2 s".into());
        }
        self.bus.p.reset_reason = match r {
            ResetRequest::System => ResetReason::SwSys,
            ResetRequest::Cpu => ResetReason::SwCpu,
        };
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
        // Keep the RTC time running across resets.
        let now = self.bus.now_ns();
        self.bus.p.rtc_ticks_base = self.bus.rtc_ticks(now);
        let cycles = self.bus.clock.cycles;
        self.bus.clock = bus::Clock::new(bus::XTAL_HZ);
        self.bus.clock.cycles = cycles;
        self.bus.clock.set_base(now);
        self.bus.p.systimer.reset_counters(now);
        self.bus.sram.fill(0);
        self.bus.psram.fill(0);
        if power_on {
            self.bus.lp.fill(0);
        }
        self.bus.mmu = [0; bus::MMU_ENTRIES];
        self.hooks.clear_pending();
        self.hle.chip_reset();
        if let Some(mc) = self.bus.mc.as_deref_mut() {
            mc.chip_reset();
        }
        self.light_sleep = None;
        self.irq = None;
        self.trap_depth = 0;
        self.bus.irq_dirty = true;
        self.boots += 1;
        self.select_app();
        if let Err(e) = self.rom_boot() {
            self.msg(format!("boot failed: {e:#}"));
        }
        self.service();
    }
}

impl Machine for Esp32c5 {
    fn set_coverage(&mut self, cov: Coverage) {
        self.coverage = Some(Box::new(cov));
        self.cover_app();
    }

    fn coverage(&mut self) -> Option<&mut Coverage> {
        self.coverage.as_deref_mut()
    }

    fn light_sleep(&self) -> Option<Option<u64>> {
        self.light_sleep.map(|(w, _)| w)
    }

    fn run_slice(&mut self, until_ns: u64) -> SliceExit {
        if let Some(m) = self.pending_halt.take() {
            return SliceExit::Halted(m);
        }
        let until = self.bus.clock.cycles_at(until_ns);
        loop {
            if self.bus.clock.cycles >= until {
                return SliceExit::Reached;
            }
            if let Some((wake_at, gpio)) = self.light_sleep {
                // CPU parked: only time passes (paced by the runner).
                let now = self.bus.now_ns();
                let all = (1u64 << periph::GPIO_COUNT) - 1;
                let button_low = gpio && self.bus.board.gpio_in(now).0 & all != all;
                if wake_at.is_some_and(|t| now >= t) || button_low {
                    self.light_sleep = None;
                    // PMU_GPIO_WAKEUP_EN / PMU_LP_TIMER_WAKEUP_EN
                    self.bus.p.wakeup_cause = if button_low { 1 << 2 } else { 1 << 4 };
                } else {
                    let target = wake_at.map_or(until, |t| until.min(self.bus.clock.cycles_at(t)));
                    self.bus.clock.cycles = self.bus.clock.cycles.max(target);
                    self.service();
                    continue;
                }
            }
            let stop = until.min(self.bus.next_event);
            while self.bus.clock.cycles < stop {
                if self.hooks.maybe(self.cpu.pc) {
                    if self.run_hook() {
                        if !self.requests.is_empty() {
                            break;
                        }
                        continue;
                    }
                    if !self.requests.is_empty() {
                        break;
                    }
                }
                let pc = self.cpu.pc;
                if let Some(cov) = &mut self.coverage {
                    cov.hit(pc);
                }
                match self.cpu.step(&mut self.bus) {
                    Step::Ok => {}
                    Step::Wfi => {
                        if self.bus.irq_dirty {
                            self.update_irq();
                        }
                        if self.irq.is_none_or(|i| i.level <= self.cpu.clic_level()) {
                            self.bus.clock.cycles = self.bus.clock.cycles.max(stop);
                            break;
                        }
                    }
                    Step::Trap(t) => {
                        if let Some(exit) = self.handle_trap(t.cause, t.tval) {
                            return exit;
                        }
                    }
                }
                if self.bus.irq_dirty {
                    if self.bus.flash.power_lost().is_some() {
                        break;
                    }
                    self.update_irq();
                    if let Some(r) = self.bus.p.reset_request.take() {
                        self.handle_reset_request(r);
                        break;
                    }
                    if self.bus.mc.as_ref().is_some_and(|mc| mc.pending.is_some())
                        && let Some(exit) = self.memcheck_poll(pc)
                    {
                        return exit;
                    }
                }
                if let Some(irq) = self.irq
                    && self.cpu.irq_enabled()
                    && irq.level > self.cpu.clic_level()
                {
                    self.take_irq(irq);
                }
            }
            if let Some(msg) = self.bus.flash.power_lost() {
                return SliceExit::PowerLoss(msg.to_string());
            }
            if let Some(r) = self.bus.p.reset_request.take() {
                self.handle_reset_request(r);
            }
            if !self.requests.is_empty() {
                for r in std::mem::take(&mut self.requests) {
                    match r {
                        MachineRequest::DeepSleep { timer_ns, gpio_low_mask } => {
                            self.bus.flash.flush().ok();
                            return SliceExit::DeepSleep { timer_ns, gpio_low_mask };
                        }
                        MachineRequest::Halt(m) => return SliceExit::Halted(m),
                        MachineRequest::LightSleep { timer_ns, gpio } => {
                            let now = self.bus.now_ns();
                            self.light_sleep = Some((timer_ns.map(|t| now + t), gpio));
                        }
                        MachineRequest::RtcSleep { .. } => {
                            // Not on this chip (light sleep goes through pmu_sleep_start, which
                            // esp_light_sleep_start's hook covers).
                            log::warn!("rtc_sleep_start on an ESP32-C5");
                        }
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
                // PMU wakeup cause bits: GPIO = BIT(2), LP timer = BIT(4)
                self.bus.p.wakeup_cause = (by_gpio as u32) << 2 | (by_timer as u32) << 4;
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
        self.bus.clock.cycles
    }

    fn board(&mut self) -> &mut dyn Board {
        self.bus.board.as_mut()
    }

    fn take_output(&mut self) -> Vec<Output> {
        let uart = std::mem::take(&mut self.bus.p.console_out);
        let mut out = Vec::new();
        let mut pos = 0;
        for (at, msg) in std::mem::take(&mut self.sim_msgs) {
            let at = at.min(uart.len());
            if at > pos {
                out.push(Output::Serial(uart[pos..at].to_vec()));
                pos = at;
            }
            out.push(Output::Sim(msg));
        }
        if pos < uart.len() {
            out.push(Output::Serial(uart[pos..].to_vec()));
        }
        out
    }

    fn flush(&mut self) {
        if let Err(e) = self.bus.flash.flush() {
            log::error!("saving flash: {e}");
        }
    }

    fn debug_dump(&self) -> String {
        let c = &self.cpu;
        let mut s = format!("pc  {}\nra  {}\n", self.syms.describe(c.pc), self.syms.describe(c.x[1]));
        s += &c.gpr_dump();
        s += &format!(
            "irqs taken per line: {:?}\n",
            self.irq_counts.iter().enumerate().filter(|(_, n)| **n > 0).collect::<Vec<_>>()
        );
        s += &format!("intc: {:?}\n", self.bus.p.intc);
        s += &format!("systimer: {:?}\n", self.bus.p.systimer);
        // Heuristic backtrace: code addresses found on the stack.
        s += "stack scan:";
        let sp = c.x[2];
        let mut n = 0;
        for i in 0..256 {
            if let Some(v) = self.bus.peek32(sp + 4 * i)
                && ((0x4200_0000..0x4400_0000).contains(&v) || (0x4080_0000..0x4086_0000).contains(&v))
            {
                s += &format!("\n  {}", self.syms.describe(v));
                n += 1;
                if n >= 12 {
                    break;
                }
            }
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
    }

    fn set_wifi_networks(&mut self, networks: &[sim_api::WifiNetwork]) {
        let now = self.bus.now_ns();
        self.hle.wifi.set_networks(networks, now);
    }

    fn set_portal_client(&mut self, on: bool) {
        let now = self.bus.now_ns();
        self.hle.wifi.set_portal_client(on, now);
    }

    fn realtime_required(&self) -> bool {
        self.hle.wifi.net_busy()
    }

    fn set_faults(&mut self, faults: &sim_api::Faults) -> Result<(), String> {
        self.bus.p.chip_temp_c = faults.chip_temp_c.unwrap_or(25.0);
        crate::faults::apply(faults, &mut self.bus.flash, &mut self.hle.wifi, self.bus.board.as_mut())
    }

    fn flash_stats(&self) -> (u64, u64) {
        (self.bus.flash.programs, self.bus.flash.erases)
    }

    fn partitions(&self) -> Vec<sim_api::PartitionInfo> {
        firmware::partitions(&self.bus.flash.data)
    }

    fn memcheck_json(&mut self) -> Option<String> {
        self.memcheck_scan_stacks();
        self.bus.mc.as_ref().map(|mc| mc.json().to_string())
    }

    fn memcheck_summary(&mut self) -> Option<String> {
        self.memcheck_scan_stacks();
        self.bus.mc.as_ref().map(|mc| mc.summary())
    }

    fn net_status(&self) -> NetStatus {
        let w = &self.hle.wifi;
        NetStatus { connected: w.is_connected(), ip: w.ip().map(|i| i.to_string()), portal_url: w.portal_url() }
    }
    fn save_soc(&mut self, rtc: bool) -> SocState {
        SocState {
            mac: self.bus.p.mac,
            now_ns: self.bus.now_ns(),
            boots: self.boots,
            rtc_ticks_base: self.bus.p.rtc_ticks_base,
            rtc_regs: if rtc { self.bus.p.rtc_regs() } else { Vec::new() },
            rtc_mem: if rtc { vec![self.bus.lp.to_vec()] } else { Vec::new() },
            mmu: Vec::new(),
            flash: self.bus.flash.data.clone(),
        }
    }

    fn restore_soc(&mut self, s: &SocState) -> anyhow::Result<()> {
        if s.flash.len() != self.bus.flash.data.len() {
            anyhow::bail!("save point has {} bytes of flash, this device {}", s.flash.len(), self.bus.flash.data.len());
        }
        if s.rtc_mem.iter().any(|m| m.len() != self.bus.lp.len()) || s.rtc_mem.len() > 1 {
            anyhow::bail!("save point RTC memory doesn't fit an ESP32-C5");
        }
        self.bus.flash.data.copy_from_slice(&s.flash);
        self.bus.flash.dirty = true;
        self.bus.flash.flush()?;
        self.bus.p.mac = s.mac;
        self.hle.wifi.base_mac = s.mac;
        self.hle.chip_reset();
        self.hooks.clear_pending();
        self.requests.clear();
        self.light_sleep = None;
        self.pending_halt = None;
        self.recent_resets.clear();
        self.bus.p.chip_reset(false);
        self.bus.p.set_rtc_regs(&s.rtc_regs);
        self.bus.p.rtc_ticks_base = s.rtc_ticks_base;
        self.bus.lp.fill(0);
        if let Some(m) = s.rtc_mem.first() {
            self.bus.lp.copy_from_slice(m);
        }
        let cycles = self.bus.clock.cycles;
        self.bus.clock = bus::Clock::new(bus::XTAL_HZ);
        self.bus.clock.cycles = cycles;
        self.bus.clock.set_base(s.now_ns);
        self.bus.p.systimer.reset_counters(s.now_ns);
        self.boots = s.boots;
        Ok(())
    }
}

/// The C5's core: RV32IMAC with the CLIC.
fn new_cpu() -> Rv32 {
    let mut c = Rv32::new();
    c.csr.clic = true;
    c
}

/// The C5's radio does 2.4 and 5 GHz.
fn dual_band(mut h: HleState) -> HleState {
    h.wifi.set_dual_band();
    h
}
