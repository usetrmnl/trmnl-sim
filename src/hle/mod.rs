//! High-level emulation: replace selected firmware functions (found by ELF
//! symbol) with host implementations. Used where register-level emulation is
//! impractical (the WiFi blob) or pointless (sleep, ADC calibration).
//!
//! Hooks are ISA-neutral: they manipulate the core through [`GuestCpu`] and
//! memory through [`GuestMem`]. A hook may also call back into guest code
//! (e.g. `esp_event_post`) and resume in a continuation when it returns.

pub mod idf;
pub mod wifi;

use std::collections::HashMap;

use crate::arch::GuestCpu;
use crate::firmware::Symbols;

/// Guest memory access for HLE code (no peripheral side effects).
pub trait GuestMem {
    fn read_bytes(&self, addr: u32, len: usize) -> Option<Vec<u8>>;
    fn write_bytes(&mut self, addr: u32, data: &[u8]) -> bool;
    fn read_u32(&self, addr: u32) -> Option<u32> {
        self.read_bytes(addr, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        self.write_bytes(addr, &v.to_le_bytes())
    }
}

/// Machine services available to hooks.
pub trait HleEnv {
    fn now_ns(&self) -> u64;
    fn adc_millivolts(&mut self, gpio: u8) -> u32;
    fn console(&mut self, msg: &str);
    /// Request the machine to perform a system-level action after this hook.
    fn request(&mut self, r: MachineRequest);
}

#[derive(Debug, Clone, PartialEq)]
pub enum MachineRequest {
    /// Enter deep sleep. Wake on timer (ns from now) and/or GPIO low on any pin in mask.
    DeepSleep { timer_ns: Option<u64>, gpio_low_mask: u64 },
    /// Light sleep: stall the CPU until the timer (or a GPIO, if enabled) wakes it.
    LightSleep { timer_ns: Option<u64>, gpio: bool },
    /// `rtc_sleep_start(wakeup_opt, ..)`: the chip halts until a wake source in
    /// `wakeup_opt` (RTC_*_TRIG_EN bits) fires; the SoC reads its RTC registers.
    RtcSleep { wakeup_opt: u32 },
    /// Stop the machine with a message (unsupported firmware behaviour).
    #[allow(dead_code)]
    Halt(String),
}

/// What a hook wants the CPU to do next.
pub enum Flow {
    /// Return from the hooked function (with an optional return value).
    Return(Option<u32>),
    /// Execute the original function as if nothing happened.
    Continue,
    /// Call a guest function; `then` runs when it returns, with its return value.
    Call { func: u32, args: Vec<u32>, then: Cont },
    /// The CPU already has been redirected by the hook.
    Redirected,
}

pub type Cont = Box<dyn FnOnce(&mut HleCtx, u32) -> Flow + Send>;

/// Wake-up sources configured through the IDF sleep API.
#[derive(Default, Clone, Debug)]
pub struct SleepConfig {
    pub timer_us: Option<u64>,
    /// GPIOs that wake the chip when low (deep sleep).
    pub gpio_low_mask: u64,
    /// GPIO wakeup enabled for light sleep (levels configured per pin).
    pub light_gpio: bool,
}

/// Host-side state owned by the HLE layer. Survives across hooks; reset with the chip.
pub struct HleState {
    pub wifi: wifi::WifiState,
    pub sleep: SleepConfig,
    /// Where the second core should start (ESP32 dual-core ROM API).
    pub appcpu_boot_addr: Option<u32>,
    /// A simulator-initiated guest call returned to the boot trampoline.
    pub wake_stub_returned: bool,
}

impl HleState {
    pub fn new(mac: [u8; 6]) -> Self {
        HleState {
            wifi: wifi::WifiState::new(mac),
            sleep: SleepConfig::default(),
            appcpu_boot_addr: None,
            wake_stub_returned: false,
        }
    }

    pub fn chip_reset(&mut self) {
        self.wifi.reset();
        self.sleep = SleepConfig::default();
        self.appcpu_boot_addr = None;
        self.wake_stub_returned = false;
    }
}

/// Everything a hook gets to touch.
pub struct HleCtx<'a> {
    pub cpu: &'a mut dyn GuestCpu,
    pub mem: &'a mut dyn GuestMem,
    pub env: &'a mut dyn HleEnv,
    pub syms: &'a Symbols,
    pub state: &'a mut HleState,
}

pub type HookFn = fn(&mut HleCtx) -> Flow;

/// Guest "return addresses" at or above this are continuation trampolines.
pub const MAGIC_BASE: u32 = 0x7F00_0000;

pub struct Hooks {
    by_addr: HashMap<u32, (&'static str, HookFn)>,
    filter: Vec<u64>,
    /// Continuations of guest calls, keyed by their return trampoline: (continuation,
    /// the hooked function's return address).
    pending: HashMap<u32, (Cont, u32)>,
    next_magic: u32,
    /// Fixed trampolines (e.g. HLE-owned task entry points).
    pub trampolines: HashMap<u32, (&'static str, HookFn)>,
    traced: HashMap<u32, &'static str>,
}

const FILTER_BITS: usize = 1 << 20;

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            by_addr: HashMap::new(),
            filter: vec![0; FILTER_BITS / 64],
            pending: HashMap::new(),
            next_magic: MAGIC_BASE + 0x10_0000,
            trampolines: HashMap::new(),
            traced: HashMap::new(),
        }
    }
}

impl Hooks {
    #[inline(always)]
    fn fidx(pc: u32) -> usize {
        ((pc >> 1) as usize) & (FILTER_BITS - 1)
    }

    /// Cheap pre-check done before every instruction.
    #[inline(always)]
    pub fn maybe(&self, pc: u32) -> bool {
        let i = Self::fidx(pc);
        pc >= MAGIC_BASE || self.filter[i / 64] >> (i % 64) & 1 != 0
    }

    pub fn install(&mut self, syms: &Symbols, name: &'static str, f: HookFn) -> bool {
        match syms.addr(name) {
            Some(a) => {
                self.install_at(a & !1, name, f);
                true
            }
            None => false,
        }
    }

    pub fn install_at(&mut self, addr: u32, name: &'static str, f: HookFn) {
        let i = Self::fidx(addr);
        self.filter[i / 64] |= 1 << (i % 64);
        self.by_addr.insert(addr, (name, f));
    }

    /// Log every call to `name` (args and caller) without changing behaviour.
    pub fn trace(&mut self, syms: &Symbols, name: &str) -> bool {
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        match syms.addr(name) {
            Some(a) => {
                let a = a & !1;
                let i = Self::fidx(a);
                self.filter[i / 64] |= 1 << (i % 64);
                self.traced.insert(a, leaked);
                true
            }
            None => false,
        }
    }

    pub fn trampoline(&mut self, addr: u32, name: &'static str, f: HookFn) {
        self.trampolines.insert(addr, (name, f));
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Run the hook at the CPU's pc, if any. Returns true if the CPU was redirected.
    pub fn dispatch(&mut self, ctx: &mut HleCtx) -> bool {
        let pc = ctx.cpu.pc();
        if let Some(name) = self.traced.get(&pc) {
            let args: Vec<String> = (0..4).map(|i| format!("{:#x}", ctx.cpu.arg(i))).collect();
            let from = ctx.syms.describe(ctx.cpu.return_address());
            let mut extra = String::new();
            if let Ok(watch) = std::env::var("SIM_WATCH") {
                for w in watch.split(',') {
                    if let Some(a) = ctx.syms.addr(w) {
                        let v = ctx.mem.read_u32(a).unwrap_or(0);
                        extra += &format!(" [{w}={v:#x}");
                        // follow a linked list through the first word (netif->next)
                        let mut p = v;
                        for _ in 0..6 {
                            if p == 0 {
                                break;
                            }
                            p = ctx.mem.read_u32(p).unwrap_or(0);
                            extra += &format!("->{p:#x}");
                        }
                        extra += "]";
                    }
                }
            }
            let t = ctx.env.now_ns() as f64 / 1e9;
            ctx.env.console(&format!("trace @{t:.6}: {name}({}) from {from}{extra}", args.join(", ")));
        }
        let flow = if let Some((cont, ra)) = self.pending.remove(&pc) {
            let ret = ctx.cpu.ret_val();
            ctx.cpu.restore_after_call(ra);
            cont(ctx, ret)
        } else if let Some((name, f)) = self.trampolines.get(&pc).copied() {
            log::trace!("hle trampoline {name}");
            f(ctx)
        } else if let Some((name, f)) = self.by_addr.get(&pc).copied() {
            log::trace!("hle {name}");
            f(ctx)
        } else {
            return false;
        };
        self.apply(ctx, flow)
    }

    fn apply(&mut self, ctx: &mut HleCtx, flow: Flow) -> bool {
        match flow {
            Flow::Continue => false,
            Flow::Redirected => true,
            Flow::Return(v) => {
                ctx.cpu.return_from_hook(v);
                true
            }
            Flow::Call { func, args, then } => {
                let magic = self.next_magic;
                self.next_magic += 2;
                if self.next_magic >= 0x7FFF_FFF0 {
                    self.next_magic = MAGIC_BASE + 0x10_0000;
                }
                self.pending.insert(magic, (then, ctx.cpu.return_address()));
                ctx.cpu.begin_call(func, &args, magic);
                true
            }
        }
    }
}
