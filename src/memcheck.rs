//! Guest memory checking (`--memcheck`): heap use-after-free, buffer overflows, double
//! and invalid frees, and FreeRTOS stack high-water marks.
//!
//! The heap is followed at the `multi_heap_*` layer (see `hle::memcheck`), which every
//! allocation of IDF 4.4 and 5.x goes through exactly once, whatever the entry point
//! (`malloc`, `heap_caps_*`, `new`, newlib in ROM). A shadow byte per guest byte of
//! internal SRAM (and the PSRAM window on the S3) says whether the CPU may access it:
//! user bytes of live blocks and memory outside the heap are fine; freed blocks, TLSF
//! block headers, the slack after a block and never-allocated heap are not. The bus
//! checks every CPU load and store against it; peripheral (DMA) and simulator accesses
//! don't go through that path. Accesses by the allocator itself are exempted by PC.
//!
//! Freed blocks go through a small quarantine before the allocator gets them back, so a
//! use-after-free is caught even if the memory would have been reused right away.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::firmware::Symbols;

/// Return addresses kept per allocation and free (innermost first, 0-terminated).
pub const FRAMES: usize = 12;
pub type Frames = [u32; FRAMES];
/// Frames shown per stack in a report, after dropping the allocator's own.
const SHOWN_FRAMES: usize = 8;
/// Freed-block records kept for use-after-free and double-free reports.
const FREED_RECORDS: usize = 8192;
/// Quarantine budget per memory (internal RAM, PSRAM). Blocks larger than a quarter of
/// it are handed back at once. Kept small: the firmware sizes buffers by free heap.
const QUARANTINE_BYTES: [u32; 2] = [16 << 10, 256 << 10];
/// A task whose stack came within this many bytes of its end is flagged. (IDF's own
/// small tasks, e.g. ipc0/1 with 1280 bytes, use all but ~450.)
pub const STACK_MARGIN: u32 = 256;
/// An access this far below the running task's stack is reported as a stack overflow.
const STACK_OVERFLOW_REACH: u32 = 1024;
/// Distinct violations kept (repeats of the same kind at the same pc are counted).
const MAX_VIOLATIONS: usize = 200;
/// FreeRTOS fills new stacks with this byte (tskSTACK_FILL_BYTE).
const STACK_FILL: u8 = 0xa5;
/// How often (virtual time) the running tasks' stacks are scanned for their high-water mark.
const STACK_SCAN_NS: u64 = 50_000_000;

/// What a shadow byte says about its guest byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Not heap, or a user byte of a live block.
    Ok = 0,
    Freed = 1,
    /// A block header, or past the requested size of a live block.
    Redzone = 2,
    /// Heap memory never handed out (free space, allocator control structures).
    Unallocated = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Report and carry on.
    Log,
    /// Stop the machine at the first violation.
    Halt,
}

/// A load or store that hit a poisoned shadow byte.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Access {
    pub addr: u32,
    pub len: u32,
    pub write: bool,
    pub state: u8,
}

/// Where in the CPU (and which FreeRTOS task) something happened.
pub struct Site {
    pub frames: Vec<u32>,
    pub tcb: Option<u32>,
    pub core: usize,
}

impl Site {
    fn frames(&self) -> Frames {
        let mut f = [0; FRAMES];
        for (d, s) in f.iter_mut().zip(&self.frames) {
            *d = *s;
        }
        f
    }
}

struct Alloc {
    size: u32,
    /// TLSF block size: the block's bytes run from the pointer to pointer + extent.
    extent: u32,
    site: Frames,
    tcb: Option<u32>,
}

struct Freed {
    alloc: Alloc,
    site: Frames,
    tcb: Option<u32>,
    seq: u64,
}

struct Task {
    name: String,
    stack: u32,
    size: u32,
    /// Bytes at the bottom of the stack never touched (still the fill pattern).
    min_free: u32,
    deleted: bool,
}

#[derive(Clone)]
struct StackMark {
    size: u32,
    min_free: u32,
    instances: u32,
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub allocs: u64,
    pub frees: u64,
    /// Live bytes (requested sizes) and blocks per memory: [internal, PSRAM].
    pub live_bytes: [u64; 2],
    pub live_blocks: [u64; 2],
    pub peak_bytes: [u64; 2],
}

/// A block involved in a violation.
pub struct BlockInfo {
    pub ptr: u32,
    pub size: u32,
    /// e.g. "20 bytes inside", "4 bytes after the end of".
    pub relation: String,
    pub allocated: Vec<String>,
    pub alloc_task: Option<String>,
    pub freed: Option<Vec<String>>,
    pub free_task: Option<String>,
}

impl BlockInfo {
    fn json(&self) -> Value {
        json!({
            "ptr": format!("{:#010x}", self.ptr),
            "size": self.size,
            "relation": self.relation,
            "allocated": self.allocated,
            "alloc_task": self.alloc_task,
            "freed": self.freed,
            "free_task": self.free_task,
        })
    }
}

pub struct Violation {
    pub kind: &'static str,
    pub addr: u32,
    /// Access size (loads/stores); 0 for frees.
    pub size: u32,
    pub write: Option<bool>,
    pub pc: u32,
    pub pc_desc: String,
    pub task: Option<String>,
    pub core: usize,
    pub backtrace: Vec<String>,
    pub block: Option<BlockInfo>,
    /// For accesses outside any live block: the nearest live block below.
    pub nearby: Option<BlockInfo>,
    pub time_ns: u64,
    pub count: u64,
}

impl Violation {
    fn key(&self) -> (&'static str, u32) {
        (self.kind, self.block.as_ref().map_or(self.pc, |b| b.ptr))
    }

    pub fn summary(&self) -> String {
        let what = match self.write {
            Some(w) => format!("{}-byte {} at {:#010x}", self.size, if w { "write" } else { "read" }, self.addr),
            None => format!("free of {:#010x}", self.addr),
        };
        let task = self.task.as_ref().map(|t| format!(", task {t:?}")).unwrap_or_default();
        format!("{}: {what} (pc {}{task}, core {})", self.kind, self.pc_desc, self.core)
    }

    /// The full report, one line per entry.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("memcheck: {}", self.summary())];
        out.push(format!("  backtrace: {}", self.backtrace.join(" <- ")));
        let task = |t: &Option<String>| t.as_ref().map(|t| format!(" by task {t:?}")).unwrap_or_default();
        if let Some(b) = &self.block {
            out.push(format!("  {:#010x} is {} a {}-byte block at {:#010x}", self.addr, b.relation, b.size, b.ptr));
            out.push(format!("  allocated{}: {}", task(&b.alloc_task), b.allocated.join(" <- ")));
            if let Some(f) = &b.freed {
                out.push(format!("  freed{}: {}", task(&b.free_task), f.join(" <- ")));
            }
        }
        if let Some(b) = &self.nearby {
            out.push(format!(
                "  nearest live block below: {:#010x} is {} a {}-byte block at {:#010x}, allocated{}: {}",
                self.addr,
                b.relation,
                b.size,
                b.ptr,
                task(&b.alloc_task),
                b.allocated.join(" <- ")
            ));
        }
        out
    }

    fn json(&self) -> Value {
        json!({
            "kind": self.kind,
            "address": format!("{:#010x}", self.addr),
            "size": self.size,
            "write": self.write,
            "pc": self.pc_desc,
            "task": self.task,
            "core": self.core,
            "backtrace": self.backtrace,
            "block": self.block.as_ref().map(BlockInfo::json),
            "nearby": self.nearby.as_ref().map(BlockInfo::json),
            "time_s": self.time_ns as f64 / 1e9,
            "count": self.count,
            "report": self.lines(),
        })
    }
}

/// Outcome of a free as seen by the tracker.
pub enum Free {
    /// A live block; the free goes ahead (or into quarantine).
    Ok { extent: u32 },
    /// Not a live block: the report to raise. The free must not reach the allocator.
    Bad(Box<Violation>),
}

/// Firmware addresses the hooks need, bound to the booting app.
#[derive(Default, Clone, Copy)]
pub struct Bindings {
    pub malloc: u32,
    pub free: u32,
}

/// Where FreeRTOS keeps the running task of `core`: `pxCurrentTCBs[core]` (IDF 5) or
/// `pxCurrentTCB` (IDF 4.4, single core).
pub fn current_tcb_addr(syms: &Symbols, core: usize) -> Option<u32> {
    syms.addr("pxCurrentTCBs").map(|a| a + 4 * core as u32).or_else(|| syms.addr("pxCurrentTCB"))
}

/// A linear piece of the guest address space covered by the shadow.
struct Window {
    base: u32,
    len: u32,
    off: usize,
}

pub struct Memcheck {
    pub mode: Mode,
    pub bindings: Bindings,
    /// Blocks leaving the quarantine through a free the simulator started: the free hook
    /// lets these through.
    pub releasing: Vec<u32>,
    windows: Vec<Window>,
    shadow: Vec<u8>,
    /// Allocator code, whose accesses to headers and free blocks are fine: sorted (start, end).
    exempt: Vec<(u32, u32)>,
    /// Word-at-a-time string functions, whose byte loads may run on to the end of the
    /// aligned word holding the terminator: sorted (start, end).
    word_scanners: Vec<(u32, u32)>,
    /// The first poisoned access of the instruction being executed, checked against the
    /// exemptions (by pc) once it retired.
    pub pending: Option<Access>,
    live: BTreeMap<u32, Alloc>,
    freed: BTreeMap<u32, Freed>,
    freed_order: VecDeque<(u32, u64)>,
    /// Frees held back: (heap, ptr, extent), per memory.
    quarantine: [VecDeque<(u32, u32, u32)>; 2],
    quarantine_bytes: [u32; 2],
    heaps: Vec<(u32, u32)>,
    tasks: HashMap<u32, Task>,
    /// Stack high-water marks of earlier boots, by task name.
    history: BTreeMap<String, StackMark>,
    pub stats: Stats,
    violations: Vec<Violation>,
    /// Known bugs to tolerate: a violation is suppressed if one of these functions is in
    /// its backtrace or its block's allocation or free stack.
    pub suppressions: Vec<String>,
    suppressed: Vec<Violation>,
    /// (kind, block or pc) of the violations seen: one report per bad block or place.
    seen: HashSet<(&'static str, u32)>,
    seq: u64,
    next_stack_scan: u64,
}

/// 0: internal RAM, 1: PSRAM (the S3's data-bus window, the C5's cache window: no heap
/// lives in flash-mapped memory, so a heap block there is in PSRAM).
fn region(addr: u32) -> usize {
    matches!(addr >> 24, 0x3C | 0x3D | 0x42 | 0x43) as usize
}

fn round4(n: u32) -> u32 {
    n.saturating_add(3) & !3
}

/// Allocator entry points, dropped from the top of allocation and free stacks.
fn is_allocator_frame(name: &str) -> bool {
    const PREFIXES: [&str; 7] = ["multi_heap_", "heap_caps_", "tlsf_", "_Znw", "_Zna", "_ZdlPv", "_ZdaPv"];
    const NAMES: [&str; 18] = [
        "malloc",
        "valloc",
        "pvalloc",
        "memalign",
        "aligned_alloc",
        "aligned_or_unaligned_alloc",
        "calloc",
        "realloc",
        "free",
        "_malloc_r",
        "_calloc_r",
        "_realloc_r",
        "_free_r",
        "pvPortMalloc",
        "vPortFree",
        "malloc_internal_wrapper",
        "calloc_internal_wrapper",
        "realloc_internal_wrapper",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p)) || NAMES.contains(&name)
}

/// String functions whose byte loads (C3 ROM) read ahead within the aligned word.
pub fn is_word_scanner(name: &str) -> bool {
    ["strlen", "strnlen", "strcmp", "strncmp", "strchr", "strrchr", "memchr", "strcpy", "stpcpy", "strncpy", "strcat"]
        .contains(&name)
}

/// Code whose memory accesses are the allocator's own business.
pub fn is_allocator_code(name: &str) -> bool {
    ["multi_heap_", "heap_caps_", "tlsf_", "assert_valid_block"].iter().any(|p| name.starts_with(p))
}

/// Symbolize a stack, optionally without the allocator frames on top.
pub fn symbolize(frames: &[u32], syms: &Symbols, skip_allocator: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for &pc in frames.iter().take_while(|&&pc| pc != 0) {
        let d = syms.describe(pc);
        if skip_allocator && out.is_empty() && is_allocator_frame(d.split('+').next().unwrap_or("")) {
            continue;
        }
        out.push(d);
        if out.len() >= SHOWN_FRAMES {
            break;
        }
    }
    out
}

/// Untouched bytes at the bottom of a stack (FreeRTOS's high-water mark).
pub fn stack_free(bytes: &[u8]) -> u32 {
    bytes.iter().take_while(|&&b| b == STACK_FILL).count() as u32
}

impl Memcheck {
    /// `windows`: (guest base, length, alias of) — an alias shares the shadow of the
    /// window starting at that address. The first window without an alias comes first
    /// in the shadow, and so on.
    pub fn new(mode: Mode, windows: &[(u32, u32, Option<u32>)]) -> Self {
        let mut ws: Vec<Window> = Vec::new();
        let mut off = 0;
        for &(base, len, alias) in windows {
            match alias.and_then(|a| ws.iter().find(|w| w.base <= a && a - w.base < w.len)) {
                Some(w) => {
                    let o = w.off + (alias.unwrap() - w.base) as usize;
                    ws.push(Window { base, len, off: o });
                }
                None => {
                    ws.push(Window { base, len, off });
                    off += len as usize;
                }
            }
        }
        Memcheck {
            mode,
            bindings: Bindings::default(),
            releasing: Vec::new(),
            windows: ws,
            shadow: vec![0; off],
            exempt: Vec::new(),
            word_scanners: Vec::new(),
            pending: None,
            live: BTreeMap::new(),
            freed: BTreeMap::new(),
            freed_order: VecDeque::new(),
            quarantine: Default::default(),
            quarantine_bytes: [0; 2],
            heaps: Vec::new(),
            tasks: HashMap::new(),
            history: BTreeMap::new(),
            stats: Stats::default(),
            violations: Vec::new(),
            suppressions: Vec::new(),
            suppressed: Vec::new(),
            seen: HashSet::new(),
            seq: 0,
            next_stack_scan: 0,
        }
    }

    /// The chip reset: RAM contents (and so the heap) are gone. Violations, counters and
    /// stack marks are kept.
    pub fn chip_reset(&mut self) {
        let tasks: Vec<u32> = self.tasks.keys().copied().collect();
        for tcb in tasks {
            self.retire_task(tcb);
        }
        // A fresh (lazily zeroed) buffer rather than a fill: the PSRAM window is 32 MB.
        self.shadow = vec![0; self.shadow.len()];
        self.pending = None;
        self.next_stack_scan = 0;
        self.releasing.clear();
        self.live.clear();
        self.freed.clear();
        self.freed_order.clear();
        self.quarantine = Default::default();
        self.quarantine_bytes = [0; 2];
        self.heaps.clear();
        self.stats.live_bytes = [0; 2];
        self.stats.live_blocks = [0; 2];
    }

    /// Code ranges: the allocator's own, and the word-at-a-time string functions.
    pub fn set_code_ranges(&mut self, mut exempt: Vec<(u32, u32)>, mut word_scanners: Vec<(u32, u32)>) {
        exempt.sort();
        word_scanners.sort();
        self.exempt = exempt;
        self.word_scanners = word_scanners;
    }

    fn in_ranges(ranges: &[(u32, u32)], pc: u32) -> bool {
        let i = ranges.partition_point(|r| r.0 <= pc);
        i > 0 && pc < ranges[i - 1].1
    }

    /// Whether the access the instruction at `pc` made is fine after all: the allocator's
    /// own, or a string function reading the rest of the aligned word its string ends in.
    pub fn is_exempt(&self, pc: u32, a: &Access) -> bool {
        if Self::in_ranges(&self.exempt, pc) {
            return true;
        }
        let word = a.addr & !3;
        !a.write
            && word != a.addr
            && Self::in_ranges(&self.word_scanners, pc)
            && (word..a.addr).any(|b| self.index(b).is_some_and(|i| self.shadow[i] == 0))
    }

    #[inline(always)]
    fn index(&self, addr: u32) -> Option<usize> {
        self.windows.iter().find(|w| addr.wrapping_sub(w.base) < w.len).map(|w| w.off + (addr - w.base) as usize)
    }

    /// Check a CPU access. Returns true if it is the instruction's first bad one.
    #[inline(always)]
    pub fn access(&mut self, addr: u32, len: u32, write: bool) -> bool {
        let Some(i) = self.index(addr) else { return false };
        // An aligned load that starts in a block may run past its end (word-at-a-time
        // strlen/strcmp/memcpy do), like valgrind's partial-loads-ok. Stores are exact.
        let len = if !write && addr & (len - 1) == 0 { 1 } else { len };
        let end = (i + len as usize).min(self.shadow.len());
        match self.shadow[i..end].iter().find(|&&b| b != 0) {
            Some(&state) if self.pending.is_none() => {
                self.pending = Some(Access { addr, len, write, state });
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    fn state(&self, addr: u32) -> State {
        match self.index(addr).map(|i| self.shadow[i]) {
            Some(1) => State::Freed,
            Some(2) => State::Redzone,
            Some(3) => State::Unallocated,
            _ => State::Ok,
        }
    }

    fn mark(&mut self, addr: u32, len: u32, state: State) {
        let (mut a, end) = (addr, addr.saturating_add(len));
        while a < end {
            let Some(w) = self.windows.iter().find(|w| a.wrapping_sub(w.base) < w.len) else {
                a = a.saturating_add(1);
                continue;
            };
            let n = (end - a).min(w.base + w.len - a);
            let i = w.off + (a - w.base) as usize;
            self.shadow[i..i + n as usize].fill(state as u8);
            a += n;
        }
    }

    // ---- heap ----------------------------------------------------------------------------

    /// `multi_heap_register(start, size)`: the whole region belongs to the allocator.
    pub fn register_heap(&mut self, start: u32, size: u32) {
        self.mark(start, size, State::Unallocated);
        self.heaps.push((start, size));
    }

    /// A block was handed out. `extent` is its TLSF block size (0 if unknown).
    pub fn on_alloc(&mut self, ptr: u32, size: u32, extent: u32, site: &Site) {
        if let Some(a) = self.live.get(&ptr) {
            if a.size == size {
                return; // seen through two nested entry points (e.g. realloc(NULL, n))
            }
            self.forget_live(ptr);
        }
        let extent = if (size..size.saturating_add(1 << 24)).contains(&extent) { extent } else { round4(size) };
        self.mark(ptr.wrapping_sub(4), 4, State::Redzone);
        self.mark(ptr, size, State::Ok);
        self.mark(ptr + size, extent - size, State::Redzone);
        // Freed records the new block overlaps are stale now.
        let stale: Vec<u32> = self
            .freed
            .range(..ptr + extent)
            .rev()
            .take_while(|(p, f)| **p + f.alloc.extent > ptr)
            .map(|(p, _)| *p)
            .collect();
        for p in stale {
            self.freed.remove(&p);
        }
        self.live.insert(ptr, Alloc { size, extent, site: site.frames(), tcb: site.tcb });
        let r = region(ptr);
        let s = &mut self.stats;
        s.allocs += 1;
        s.live_bytes[r] += size as u64;
        s.live_blocks[r] += 1;
        s.peak_bytes[r] = s.peak_bytes[r].max(s.live_bytes[r]);
    }

    fn forget_live(&mut self, ptr: u32) -> Option<Alloc> {
        let a = self.live.remove(&ptr)?;
        let r = region(ptr);
        self.stats.live_bytes[r] -= a.size as u64;
        self.stats.live_blocks[r] -= 1;
        Some(a)
    }

    /// A block is being freed (or realloc moved it away).
    pub fn on_free(&mut self, ptr: u32, site: &Site, syms: &Symbols, now_ns: u64) -> Free {
        let Some(a) = self.forget_live(ptr) else {
            return Free::Bad(Box::new(self.bad_free(ptr, site, syms, now_ns)));
        };
        self.stats.frees += 1;
        self.mark(ptr, a.extent, State::Freed);
        let extent = a.extent;
        self.seq += 1;
        self.freed.insert(ptr, Freed { alloc: a, site: site.frames(), tcb: site.tcb, seq: self.seq });
        self.freed_order.push_back((ptr, self.seq));
        while self.freed_order.len() > FREED_RECORDS {
            let (p, seq) = self.freed_order.pop_front().unwrap();
            if self.freed.get(&p).is_some_and(|f| f.seq == seq) {
                self.freed.remove(&p);
            }
        }
        Free::Ok { extent }
    }

    /// Hold a freed block back from the allocator. Returns the (heap, ptr) to really free
    /// now: an older block leaving the quarantine, the block itself if too big, or none.
    pub fn quarantine(&mut self, heap: u32, ptr: u32, extent: u32) -> Option<(u32, u32)> {
        let r = region(ptr);
        if extent > QUARANTINE_BYTES[r] / 4 {
            return Some((heap, ptr));
        }
        self.quarantine[r].push_back((heap, ptr, extent));
        self.quarantine_bytes[r] += extent;
        if self.quarantine_bytes[r] <= QUARANTINE_BYTES[r] {
            return None;
        }
        let (h, p, e) = self.quarantine[r].pop_front()?;
        self.quarantine_bytes[r] -= e;
        Some((h, p))
    }

    /// Attribute a live block to another allocation site (realloc's rather than the
    /// malloc it made).
    pub fn set_alloc_site(&mut self, ptr: u32, site: &Site) {
        if let Some(a) = self.live.get_mut(&ptr) {
            a.site = site.frames();
            a.tcb = site.tcb;
        }
    }

    /// Make a live block addressable up to its full TLSF size (`heap_caps_realloc` copies
    /// that much when it moves a block to another heap).
    pub fn unpoison_slack(&mut self, ptr: u32) {
        if let Some(a) = self.live.get(&ptr) {
            let (size, extent) = (a.size, a.extent);
            self.mark(ptr + size, extent - size, State::Ok);
        }
    }

    /// User size of a live block.
    pub fn live_size(&self, ptr: u32) -> Option<u32> {
        self.live.get(&ptr).map(|a| a.size)
    }

    fn bad_free(&self, ptr: u32, site: &Site, syms: &Symbols, now_ns: u64) -> Violation {
        let (kind, block) = match self.freed.get(&ptr) {
            Some(f) => ("double-free", Some(self.freed_info(ptr, f, "the start of", syms))),
            None => ("invalid-free", self.live_containing(ptr).map(|(p, a)| self.live_info(ptr, p, a, syms))),
        };
        self.violation(kind, ptr, 0, None, site, block, syms, now_ns)
    }

    fn live_containing(&self, addr: u32) -> Option<(u32, &Alloc)> {
        let (p, a) = self.live.range(..=addr).next_back()?;
        (addr - p < a.extent).then_some((*p, a))
    }

    fn freed_containing(&self, addr: u32) -> Option<(u32, &Freed)> {
        let (p, f) = self.freed.range(..=addr).next_back()?;
        (addr - p < f.alloc.extent).then_some((*p, f))
    }

    fn relation(addr: u32, ptr: u32, size: u32) -> String {
        if addr < ptr {
            format!("{} bytes before", ptr - addr)
        } else if addr >= ptr + size {
            format!("{} bytes after the end of", addr - ptr - size)
        } else if addr == ptr {
            "the start of".into()
        } else {
            format!("{} bytes inside", addr - ptr)
        }
    }

    fn live_info(&self, addr: u32, ptr: u32, a: &Alloc, syms: &Symbols) -> BlockInfo {
        BlockInfo {
            ptr,
            size: a.size,
            relation: Self::relation(addr, ptr, a.size),
            allocated: symbolize(&a.site, syms, true),
            alloc_task: a.tcb.and_then(|t| self.task_name(t)),
            freed: None,
            free_task: None,
        }
    }

    fn freed_info(&self, ptr: u32, f: &Freed, relation: &str, syms: &Symbols) -> BlockInfo {
        BlockInfo {
            ptr,
            size: f.alloc.size,
            relation: relation.into(),
            allocated: symbolize(&f.alloc.site, syms, true),
            alloc_task: f.alloc.tcb.and_then(|t| self.task_name(t)),
            freed: Some(symbolize(&f.site, syms, true)),
            free_task: f.tcb.and_then(|t| self.task_name(t)),
        }
    }

    /// Classify a poisoned access and build its report.
    pub fn access_violation(&self, a: &Access, site: &Site, syms: &Symbols, now_ns: u64) -> Violation {
        let addr = a.addr;
        let own_stack = site.tcb.and_then(|t| self.tasks.get(&t)).map(|t| t.stack);
        let below = self.live.range(..=addr).next_back();
        // Past the end of a live block, up to its successor's header; or in its own header.
        let overflow = below.filter(|(p, x)| addr - **p < x.extent + 4);
        let underflow = self.live.range(addr + 1..).next().filter(|(p, _)| **p - addr <= 4);
        let mut near = None;
        let (kind, block) = if own_stack.is_some_and(|s| addr < s && s - addr <= STACK_OVERFLOW_REACH) {
            ("stack-overflow", None)
        } else if let Some((p, x)) = overflow.or(underflow) {
            ("heap-buffer-overflow", Some(self.live_info(addr, *p, x, syms)))
        } else {
            // Far outside a live block: the nearest one below may be the culprit.
            near = below.filter(|(p, _)| addr - **p < 1 << 20).map(|(p, x)| self.live_info(addr, *p, x, syms));
            match self.freed_containing(addr) {
                Some((p, f)) if a.state == State::Freed as u8 => {
                    let rel = Self::relation(addr, p, f.alloc.size);
                    ("heap-use-after-free", Some(self.freed_info(p, f, &rel, syms)))
                }
                _ if a.state == State::Freed as u8 => ("heap-use-after-free", None),
                _ if a.state == State::Redzone as u8 => ("heap-metadata-access", None),
                _ => ("unallocated-heap-access", None),
            }
        };
        let mut v = self.violation(kind, addr, a.len, Some(a.write), site, block, syms, now_ns);
        v.nearby = near;
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn violation(
        &self,
        kind: &'static str,
        addr: u32,
        size: u32,
        write: Option<bool>,
        site: &Site,
        block: Option<BlockInfo>,
        syms: &Symbols,
        now_ns: u64,
    ) -> Violation {
        let pc = site.frames.first().copied().unwrap_or(0);
        Violation {
            kind,
            addr,
            size,
            write,
            pc,
            pc_desc: syms.describe(pc),
            task: site.tcb.and_then(|t| self.task_name(t)),
            core: site.core,
            backtrace: symbolize(&site.frames, syms, false),
            block,
            nearby: None,
            time_ns: now_ns,
            count: 1,
        }
    }

    /// Record a violation. Returns its report lines if it is new (repeats of the same kind
    /// at the same pc are only counted).
    /// Whether a suppression matches any frame of the violation's stacks.
    fn is_suppressed(&self, v: &Violation) -> bool {
        let b = v.block.as_ref();
        let frames = v
            .backtrace
            .iter()
            .chain(b.map(|b| &b.allocated).into_iter().flatten())
            .chain(b.and_then(|b| b.freed.as_ref()).into_iter().flatten());
        let mut frames = frames.map(|f| f.split('+').next().unwrap_or(f));
        frames.any(|f| self.suppressions.iter().any(|s| s == f))
    }

    /// Record a violation. Returns the lines to print if it is new, and whether it counts
    /// (isn't suppressed): the caller then halts in halt mode. Repeats (same kind, and same
    /// block or else same pc) are only counted; a suppressed one is announced in one line.
    pub fn record(&mut self, v: Violation) -> Option<(Vec<String>, bool)> {
        let suppressed = self.is_suppressed(&v);
        let list = if suppressed { &mut self.suppressed } else { &mut self.violations };
        if !self.seen.insert(v.key()) {
            if let Some(old) = list.iter_mut().find(|o| o.key() == v.key()) {
                old.count += 1;
            }
            return None;
        }
        let lines = if suppressed { vec![format!("memcheck: suppressed: {}", v.summary())] } else { v.lines() };
        if list.len() < MAX_VIOLATIONS {
            list.push(v);
        }
        Some((lines, !suppressed))
    }

    // ---- tasks ---------------------------------------------------------------------------

    pub fn task_created(&mut self, tcb: u32, name: String, stack: u32, size: u32) {
        if self.tasks.contains_key(&tcb) {
            self.retire_task(tcb);
        }
        self.tasks.insert(tcb, Task { name, stack, size, min_free: size, deleted: false });
    }

    /// Fold a deleted (or, at reset, every) task into the per-name history.
    fn retire_task(&mut self, tcb: u32) {
        let Some(t) = self.tasks.remove(&tcb) else { return };
        let m = self.history.entry(t.name).or_insert(StackMark { size: t.size, min_free: t.size, instances: 0 });
        m.instances += 1;
        if t.min_free < m.min_free || t.size != m.size {
            m.min_free = t.min_free.min(m.min_free);
            m.size = t.size.max(m.size);
        }
    }

    pub fn task_deleted(&mut self, tcb: u32) {
        if let Some(t) = self.tasks.get_mut(&tcb) {
            t.deleted = true;
        }
        self.retire_task(tcb);
    }

    pub fn task_name(&self, tcb: u32) -> Option<String> {
        self.tasks.get(&tcb).map(|t| t.name.clone())
    }

    /// Current tasks' stacks as (tcb, stack base, bytes to scan): only the part below the
    /// high-water mark can still be untouched.
    pub fn stacks_to_scan(&self) -> Vec<(u32, u32, u32)> {
        self.tasks.iter().filter(|(_, t)| !t.deleted).map(|(tcb, t)| (*tcb, t.stack, t.min_free)).collect()
    }

    pub fn update_stack(&mut self, tcb: u32, free: u32) {
        if let Some(t) = self.tasks.get_mut(&tcb) {
            t.min_free = t.min_free.min(free);
        }
    }

    /// Whether the periodic stack scan is due (and if so, schedule the next one).
    pub fn stack_scan_due(&mut self, now_ns: u64) -> bool {
        if now_ns < self.next_stack_scan {
            return false;
        }
        self.next_stack_scan = now_ns + STACK_SCAN_NS;
        true
    }

    /// Update every running task's high-water mark; `read` reads guest bytes.
    pub fn scan_stacks(&mut self, read: impl Fn(u32, usize) -> Option<Vec<u8>>) {
        for (tcb, stack, len) in self.stacks_to_scan() {
            if let Some(b) = read(stack, len as usize) {
                self.update_stack(tcb, stack_free(&b));
            }
        }
    }

    /// Stack marks of this boot and earlier ones: (name, size, min free, instances).
    fn stack_marks(&self) -> Vec<(String, u32, u32, u32)> {
        let mut all = self.history.clone();
        for t in self.tasks.values() {
            let m = all.entry(t.name.clone()).or_insert(StackMark { size: t.size, min_free: t.size, instances: 0 });
            m.instances += 1;
            m.min_free = m.min_free.min(t.min_free);
            m.size = m.size.max(t.size);
        }
        let mut v: Vec<_> = all.into_iter().map(|(n, m)| (n, m.size, m.min_free, m.instances)).collect();
        v.sort_by_key(|e| e.2);
        v
    }

    // ---- reporting -----------------------------------------------------------------------

    pub fn json(&self) -> Value {
        let s = &self.stats;
        let mem = |r: usize| {
            json!({
                "live_bytes": s.live_bytes[r],
                "live_blocks": s.live_blocks[r],
                "peak_bytes": s.peak_bytes[r],
                "quarantined_bytes": self.quarantine_bytes[r],
            })
        };
        json!({
            "enabled": true,
            "mode": match self.mode { Mode::Log => "log", Mode::Halt => "halt" },
            "violations": self.violations.iter().map(Violation::json).collect::<Vec<_>>(),
            "suppressed": self.suppressed.iter().map(Violation::json).collect::<Vec<_>>(),
            "heap": {
                "allocs": s.allocs,
                "frees": s.frees,
                "internal": mem(0),
                "psram": mem(1),
            },
            "stacks": self.stack_marks().into_iter().map(|(name, size, free, n)| json!({
                "task": name,
                "size": size,
                "min_free": free,
                "max_used": size - free.min(size),
                "instances": n,
                "low": free < STACK_MARGIN,
            })).collect::<Vec<_>>(),
            "stack_margin": STACK_MARGIN,
        })
    }

    /// Human-readable summary for the end of a run.
    pub fn summary(&self) -> String {
        let s = &self.stats;
        let mut out = String::from("=== memcheck ===\n");
        let _ = writeln!(
            out,
            "heap: {} allocations, {} frees; peak {} bytes internal, {} PSRAM; live now {} blocks / {} bytes",
            s.allocs,
            s.frees,
            s.peak_bytes[0],
            s.peak_bytes[1],
            s.live_blocks[0] + s.live_blocks[1],
            s.live_bytes[0] + s.live_bytes[1]
        );
        let _ = writeln!(out, "stacks (min free of size, lowest first):");
        for (name, size, free, n) in self.stack_marks() {
            let flag = if free < STACK_MARGIN { "  <-- LOW" } else { "" };
            let times = if n > 1 { format!(" x{n}") } else { String::new() };
            let _ = writeln!(out, "  {name:<16} {free:>6} / {size:<6}{times}{flag}");
        }
        if !self.suppressed.is_empty() {
            let _ = writeln!(out, "{} suppressed violation(s):", self.suppressed.len());
            for v in &self.suppressed {
                let _ = writeln!(out, "  {}", v.summary());
            }
        }
        if self.violations.is_empty() {
            out += "no violations\n";
        } else {
            let _ = writeln!(out, "{} violation(s):", self.violations.len());
            for v in &self.violations {
                let times = if v.count > 1 { format!(" (x{})", v.count) } else { String::new() };
                let _ = writeln!(out, "  {}{times}", v.summary());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAP: u32 = 0x3FC9_0000;

    fn mc() -> Memcheck {
        let mut m = Memcheck::new(
            Mode::Log,
            &[
                (0x3FC7_C000, 0x6_4000, None),
                (0x4037_C000, 0x6_4000, Some(0x3FC7_C000)),
                (0x3C00_0000, 0x10_0000, None),
            ],
        );
        m.register_heap(HEAP, 0x1_0000);
        m
    }

    fn site(pc: u32) -> Site {
        Site { frames: vec![pc, 0x4200_0100], tcb: None, core: 0 }
    }

    fn free_ok(m: &mut Memcheck, p: u32) -> u32 {
        match m.on_free(p, &site(0x4200_0200), &Symbols::default(), 0) {
            Free::Ok { extent } => extent,
            Free::Bad(v) => panic!("unexpected {}", v.summary()),
        }
    }

    #[test]
    fn heap_region_is_poisoned_until_allocated() {
        let mut m = mc();
        assert!(!m.access(HEAP - 4, 4, false), "outside the heap");
        assert!(m.access(HEAP + 0x100, 4, false));
        assert_eq!(m.pending.unwrap().state, State::Unallocated as u8);
        m.pending = None;
        m.on_alloc(HEAP + 0x100, 10, 16, &site(1));
        assert!(!m.access(HEAP + 0x100, 4, true));
        assert!(!m.access(HEAP + 0x108, 2, true), "the last two bytes of a 10-byte block");
        assert!(!m.access(HEAP + 0x108, 4, false), "an aligned load running past the end");
        assert!(m.access(HEAP + 0x10A, 1, false), "slack after the block");
        assert_eq!(m.pending.unwrap().state, State::Redzone as u8);
        m.pending = None;
        assert!(m.access(HEAP + 0xFC, 4, false), "the block header");
        m.pending = None;
        assert!(m.access(HEAP + 0x108, 4, true), "a store running past the end");
    }

    #[test]
    fn instruction_bus_alias_shares_the_shadow() {
        let m = mc();
        assert_eq!(m.state(HEAP + 0x100), State::Unallocated);
        assert_eq!(m.state(HEAP + 0x100 - 0x3FC7_C000 + 0x4037_C000), State::Unallocated);
        assert_eq!(m.state(0x3C00_0000), State::Ok, "a separate window");
    }

    #[test]
    fn only_the_first_bad_access_of_an_instruction_is_kept() {
        let mut m = mc();
        assert!(m.access(HEAP, 4, false));
        assert!(!m.access(HEAP + 8, 4, true));
        assert_eq!(m.pending.unwrap().addr, HEAP);
    }

    #[test]
    fn use_after_free_reports_both_sites() {
        let mut m = mc();
        let p = HEAP + 0x200;
        m.on_alloc(p, 32, 32, &site(0x4200_1000));
        assert_eq!(free_ok(&mut m, p), 32);
        assert!(m.access(p + 20, 4, false));
        let a = m.pending.take().unwrap();
        let v = m.access_violation(&a, &site(0x4200_3000), &Symbols::default(), 0);
        assert_eq!(v.kind, "heap-use-after-free");
        let b = v.block.as_ref().unwrap();
        assert_eq!((b.ptr, b.size, b.relation.as_str()), (p, 32, "20 bytes inside"));
        assert_eq!(b.allocated[0], "0x42001000");
        assert_eq!(b.freed.as_ref().unwrap()[0], "0x42000200");
        assert!(v.lines().iter().any(|l| l.contains("freed:")));
    }

    #[test]
    fn overflow_past_the_end_names_the_block() {
        let mut m = mc();
        let p = HEAP + 0x400;
        m.on_alloc(p, 20, 28, &site(1));
        assert!(m.access(p + 20, 4, true));
        let a = m.pending.take().unwrap();
        let v = m.access_violation(&a, &site(2), &Symbols::default(), 0);
        assert_eq!(v.kind, "heap-buffer-overflow");
        assert_eq!(v.block.unwrap().relation, "0 bytes after the end of");
    }

    #[test]
    fn overflow_into_the_next_blocks_header_names_the_block() {
        let mut m = mc();
        let p = HEAP + 0x400;
        m.on_alloc(p, 216, 216, &site(1)); // no slack: byte 216 is the next header
        assert!(m.access(p + 216, 1, false));
        let v = m.access_violation(&m.pending.unwrap(), &site(2), &Symbols::default(), 0);
        assert_eq!(v.kind, "heap-buffer-overflow");
        assert_eq!(v.block.unwrap().relation, "0 bytes after the end of");
    }

    #[test]
    fn far_accesses_name_the_nearest_block_below() {
        let mut m = mc();
        let p = HEAP + 0x400;
        m.on_alloc(p, 64, 64, &site(1));
        assert!(m.access(p + 0x1000, 4, true));
        let v = m.access_violation(&m.pending.unwrap(), &site(2), &Symbols::default(), 0);
        assert_eq!(v.kind, "unallocated-heap-access");
        assert!(v.block.is_none());
        assert_eq!(v.nearby.as_ref().unwrap().relation, format!("{} bytes after the end of", 0x1000 - 64));
        assert!(v.lines().iter().any(|l| l.contains("nearest live block below")));
    }

    #[test]
    fn underflow_into_the_header_names_the_block() {
        let mut m = mc();
        let p = HEAP + 0x400;
        m.on_alloc(p, 20, 20, &site(1));
        assert!(m.access(p - 2, 1, true));
        let v = m.access_violation(&m.pending.unwrap(), &site(2), &Symbols::default(), 0);
        assert_eq!(v.kind, "heap-buffer-overflow");
        assert_eq!(v.block.unwrap().relation, "2 bytes before");
    }

    #[test]
    fn stack_overflow_below_the_running_tasks_stack() {
        let mut m = mc();
        let stack = HEAP + 0x1000;
        m.on_alloc(stack, 2048, 2048, &site(1));
        m.task_created(0x3FCA_0000, "worker".into(), stack, 2048);
        assert!(m.access(stack - 4, 4, true));
        let s = Site { frames: vec![0x4200_0000], tcb: Some(0x3FCA_0000), core: 0 };
        let v = m.access_violation(&m.pending.unwrap(), &s, &Symbols::default(), 0);
        assert_eq!(v.kind, "stack-overflow");
        assert_eq!(v.task.as_deref(), Some("worker"));
    }

    #[test]
    fn double_and_invalid_free() {
        let mut m = mc();
        let p = HEAP + 0x600;
        m.on_alloc(p, 8, 8, &site(1));
        free_ok(&mut m, p);
        match m.on_free(p, &site(3), &Symbols::default(), 0) {
            Free::Bad(v) => {
                assert_eq!(v.kind, "double-free");
                assert!(v.block.unwrap().freed.is_some());
            }
            Free::Ok { .. } => panic!("double free not caught"),
        }
        m.on_alloc(p, 16, 16, &site(1));
        match m.on_free(p + 4, &site(3), &Symbols::default(), 0) {
            Free::Bad(v) => {
                assert_eq!(v.kind, "invalid-free");
                assert_eq!(v.block.unwrap().relation, "4 bytes inside");
            }
            Free::Ok { .. } => panic!("invalid free not caught"),
        }
    }

    #[test]
    fn reallocation_revives_memory_and_forgets_stale_frees() {
        let mut m = mc();
        let p = HEAP + 0x800;
        m.on_alloc(p, 64, 64, &site(1));
        free_ok(&mut m, p);
        m.on_alloc(p + 16, 8, 8, &site(1));
        assert!(!m.access(p + 16, 4, false));
        assert!(m.freed.is_empty(), "the old record overlaps the new block");
        assert_eq!(m.state(p + 32), State::Freed, "the rest is still free");
    }

    #[test]
    fn nested_entry_points_count_once() {
        let mut m = mc();
        m.on_alloc(HEAP + 0x100, 24, 24, &site(1));
        m.on_alloc(HEAP + 0x100, 24, 24, &site(2));
        assert_eq!(m.stats.allocs, 1);
        assert_eq!(m.stats.live_bytes[0], 24);
    }

    #[test]
    fn stats_track_live_and_peak() {
        let mut m = mc();
        m.on_alloc(HEAP + 0x100, 100, 100, &site(1));
        m.on_alloc(HEAP + 0x200, 50, 52, &site(1));
        free_ok(&mut m, HEAP + 0x100);
        m.on_alloc(0x3C00_1000, 1000, 1000, &site(1));
        assert_eq!(m.stats.live_bytes, [50, 1000]);
        assert_eq!(m.stats.peak_bytes, [150, 1000]);
        assert_eq!((m.stats.allocs, m.stats.frees), (3, 1));
    }

    #[test]
    fn quarantine_delays_reuse_within_budget() {
        let mut m = mc();
        let cap = QUARANTINE_BYTES[0];
        assert_eq!(m.quarantine(1, HEAP, cap), Some((1, HEAP)), "too big to hold");
        let n = cap / 1024;
        for i in 0..n {
            assert_eq!(m.quarantine(1, HEAP + i * 1024, 1024), None);
        }
        assert_eq!(m.quarantine(1, HEAP + n * 1024, 1024), Some((1, HEAP)), "the oldest leaves");
        assert_eq!(m.quarantine_bytes[0], cap);
    }

    #[test]
    fn exempt_code() {
        let mut m = mc();
        m.set_code_ranges(
            vec![(0x4038_0100, 0x4038_0200), (0x4038_0000, 0x4038_0010)],
            vec![(0x4000_0000, 0x4000_0040)],
        );
        let a = Access { addr: HEAP + 0x10A, len: 1, write: false, state: State::Redzone as u8 };
        assert!(m.is_exempt(0x4038_0000, &a));
        assert!(m.is_exempt(0x4038_01FE, &a));
        assert!(!m.is_exempt(0x4038_0200, &a));
        assert!(!m.is_exempt(0x4038_0050, &a));
        // strlen reading ahead in the word a 10-byte string ends in
        m.on_alloc(HEAP + 0x100, 10, 12, &site(1));
        assert!(m.is_exempt(0x4000_0010, &a));
        assert!(!m.is_exempt(0x4000_0010, &Access { write: true, ..a }));
        assert!(!m.is_exempt(0x4000_0010, &Access { addr: HEAP + 0x10C, ..a }), "the next word");
        assert!(!m.is_exempt(0x4200_0000, &a), "not a string function");
    }

    #[test]
    fn repeats_are_counted_not_reported() {
        let mut m = mc();
        m.on_alloc(HEAP + 0x100, 8, 8, &site(1));
        free_ok(&mut m, HEAP + 0x100);
        let a = Access { addr: HEAP + 0x100, len: 4, write: false, state: State::Freed as u8 };
        let v = || m.access_violation(&a, &site(7), &Symbols::default(), 0);
        let (v1, v2) = (v(), v());
        assert!(m.record(v1).is_some_and(|(_, counts)| counts));
        assert!(m.record(v2).is_none());
        assert_eq!(m.violations[0].count, 2);
    }

    #[test]
    fn suppressions_match_any_stack() {
        let mut m = mc();
        m.suppressions = vec!["0x42000200".into()]; // unsymbolized frames print as addresses
        m.on_alloc(HEAP + 0x100, 8, 8, &site(1));
        free_ok(&mut m, HEAP + 0x100); // freed at 0x42000200
        let a = Access { addr: HEAP + 0x100, len: 4, write: true, state: State::Freed as u8 };
        let v = m.access_violation(&a, &site(7), &Symbols::default(), 0);
        let (lines, counts) = m.record(v).unwrap();
        assert!(!counts);
        assert!(lines[0].starts_with("memcheck: suppressed: heap-use-after-free"));
        assert!(m.violations.is_empty());
        assert_eq!(m.json()["suppressed"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn stack_marks_survive_resets_by_name() {
        let mut m = mc();
        m.task_created(1, "loopTask".into(), HEAP, 8192);
        m.update_stack(1, 5000);
        m.update_stack(1, 6000);
        m.chip_reset();
        m.task_created(2, "loopTask".into(), HEAP, 8192);
        m.update_stack(2, 7000);
        m.task_created(3, "tiny".into(), HEAP + 0x4000, 1024);
        m.update_stack(3, 100);
        let marks = m.stack_marks();
        assert_eq!(marks[0], ("tiny".into(), 1024, 100, 1));
        assert_eq!(marks[1], ("loopTask".into(), 8192, 5000, 2));
        assert!(m.summary().contains("<-- LOW"));
        assert_eq!(stack_free(&[0xa5, 0xa5, 0x00, 0xa5]), 2);
    }
}
