//! CPU architectures. Each core is generic over [`MemBus`] so the hot loop is
//! monomorphized per SoC, and implements [`GuestCpu`] so chip-agnostic code
//! (HLE hooks, debugging) can drive it without knowing the ISA or ABI.

pub mod riscv;
pub mod xtensa;

/// A bus access that hit nothing (unmapped address or a device refusing it).
#[derive(Debug, Clone, Copy)]
pub struct BusFault;

pub type BusResult<T> = Result<T, BusFault>;

/// The memory system as seen by a core.
pub trait MemBus {
    fn fetch16(&mut self, addr: u32) -> BusResult<u16>;
    /// Fetch four instruction bytes (little-endian) at any byte address. Xtensa uses
    /// this for its 16/24/32-bit instructions; override it with a single lookup for speed.
    #[inline(always)]
    fn fetch32(&mut self, addr: u32) -> BusResult<u32> {
        let lo = self.fetch16(addr)? as u32;
        let hi = self.fetch16(addr.wrapping_add(2))? as u32;
        Ok(lo | (hi << 16))
    }
    fn read8(&mut self, addr: u32) -> BusResult<u8>;
    fn read16(&mut self, addr: u32) -> BusResult<u16>;
    fn read32(&mut self, addr: u32) -> BusResult<u32>;
    fn write8(&mut self, addr: u32, v: u8) -> BusResult<()>;
    fn write16(&mut self, addr: u32, v: u16) -> BusResult<()>;
    fn write32(&mut self, addr: u32, v: u32) -> BusResult<()>;
    /// Current time, in CPU cycles.
    fn cycles(&self) -> u64;
    /// Advance time by one retired instruction.
    fn tick(&mut self);
}

/// ISA/ABI-neutral view of a core, used by HLE and diagnostics.
///
/// "Hook" methods assume the core is sitting at the first instruction of a
/// function (i.e. it was just called): arguments are live and returning means
/// jumping to the caller. On windowed-ABI cores (Xtensa) the implementation is
/// responsible for making that look the same.
pub trait GuestCpu {
    fn pc(&self) -> u32;
    fn set_pc(&mut self, pc: u32);
    fn arg(&self, n: usize) -> u32;
    fn set_arg(&mut self, n: usize, v: u32);
    fn ret_val(&self) -> u32;
    fn sp(&self) -> u32;
    fn set_sp(&mut self, v: u32);
    fn return_address(&self) -> u32;
    /// Return from the hooked function, optionally setting its return value.
    fn return_from_hook(&mut self, ret: Option<u32>);
    /// Scratch memory for the arguments of the next `begin_call` (valid until that call
    /// returns). The default carves it out below the stack pointer.
    fn alloc_scratch(&mut self, n: u32) -> u32 {
        let sp = (self.sp() - n - 16) & !15;
        self.set_sp(sp);
        sp
    }
    /// Set up a call to `func` that returns to `return_to`.
    fn begin_call(&mut self, func: u32, args: &[u32], return_to: u32);
    /// After a `begin_call` returned: restore the hooked function's return address
    /// (needed where the call clobbered it, e.g. RISC-V `ra`).
    fn restore_after_call(&mut self, _return_address: u32) {}
    fn irq_enabled(&self) -> bool;
    fn enter_interrupt(&mut self, line: u32, level: u32);
    fn gpr_dump(&self) -> String;
}
