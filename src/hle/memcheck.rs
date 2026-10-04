//! Hooks feeding the memory checker (`--memcheck`, see [`crate::memcheck`]): the
//! `multi_heap_*` layer of the IDF heap, and FreeRTOS task creation and deletion.
//!
//! `multi_heap_*` sits under every allocation API (`heap_caps_*`, `malloc`, `new`,
//! newlib's `_malloc_r` from ROM), so each block is seen exactly once. Return values come
//! from [`Flow::Wrap`]. Frees can be held in quarantine (the hook returns without
//! freeing; a later free releases the oldest block instead). `multi_heap_realloc` is
//! done as malloc + copy + free so that the old block is quarantined too.

use super::{Flow, HleCtx, Hooks, MachineRequest};
use crate::firmware::Symbols;
use crate::memcheck::{self, Bindings, FRAMES, Free, Memcheck, Mode, Site, Violation};

/// TCB_t field offsets, the same in IDF 4.4 and 5.x (16-byte task names).
const TCB_STACK: u32 = 0x30;
const TCB_NAME: u32 = 0x34;
/// pxEndOfStack follows xCoreID, which single-core IDF 5 builds (ESP32-C5) leave out.
const TCB_END_OF_STACK: u32 = 0x48;
const TCB_END_OF_STACK_UNICORE: u32 = 0x44;

/// Install the hooks, and bind `mc` to the firmware: the addresses the hooks need, the
/// allocator's code (exempt from access checks) and the word-at-a-time string functions.
pub fn install(hooks: &mut Hooks, syms: &Symbols, mc: &mut Memcheck) {
    hooks.install(syms, "multi_heap_register", heap_register);
    hooks.install(syms, "multi_heap_malloc", heap_malloc);
    hooks.install(syms, "multi_heap_aligned_alloc", heap_malloc);
    hooks.install(syms, "multi_heap_aligned_alloc_offs", heap_malloc);
    hooks.install(syms, "multi_heap_free", heap_free);
    hooks.install(syms, "multi_heap_aligned_free", heap_free);
    hooks.install(syms, "multi_heap_realloc", heap_realloc);
    hooks.install(syms, "multi_heap_get_allocated_size", heap_allocated_size);
    if !hooks.install(syms, "prvAddNewTaskToReadyList", task_created) {
        // GCC's IPA-SRA clone (IDF 4.4 on the S3: prvAddNewTaskToReadyList$isra$6) keeps the
        // TCB as its first argument
        if let Some(name) = syms.names_with_prefix("prvAddNewTaskToReadyList$isra").into_iter().next()
            && let Some(a) = syms.addr(&name)
        {
            hooks.install_at(a & !1, "prvAddNewTaskToReadyList", task_created);
        }
    }
    hooks.install(syms, "prvDeleteTCB", task_deleted);
    mc.bindings = Bindings {
        malloc: syms.addr("multi_heap_malloc").unwrap_or(0),
        free: syms.addr("multi_heap_free").unwrap_or(0),
    };
    mc.set_code_ranges(syms.ranges(memcheck::is_allocator_code), syms.ranges(memcheck::is_word_scanner));
}

/// Where the CPU is: call chain and running task.
fn site(c: &HleCtx) -> Site {
    let mem = &*c.mem;
    let frames = c.cpu.backtrace(&|a| mem.read_u32(a), FRAMES);
    let core = c.cpu.core_id();
    Site { frames, tcb: current_tcb(c, core), core }
}

fn current_tcb(c: &HleCtx, core: usize) -> Option<u32> {
    c.mem.read_u32(memcheck::current_tcb_addr(c.syms, core)?).filter(|&t| t != 0)
}

/// Print a new violation and, in halt mode, stop the machine.
fn raise(c: &mut HleCtx, v: Violation) {
    let summary = v.summary();
    let Some(mc) = c.mem.memcheck() else { return };
    let halt = mc.mode == Mode::Halt;
    if let Some((lines, counts)) = mc.record(v) {
        for l in lines {
            c.env.console(&l);
        }
        if halt && counts {
            c.env.request(MachineRequest::Halt(format!("memcheck: {summary}")));
        }
    }
}

/// multi_heap_handle_t multi_heap_register(void *start, size_t size)
fn heap_register(c: &mut HleCtx) -> Flow {
    let (start, size) = (c.cpu.arg(0), c.cpu.arg(1));
    Flow::Wrap(Box::new(move |c, heap| {
        if heap != 0
            && let Some(mc) = c.mem.memcheck()
        {
            mc.register_heap(start, size);
        }
    }))
}

/// void *multi_heap_malloc(heap, size), multi_heap_aligned_alloc[_offs](heap, size, alignment[, offset])
fn heap_malloc(c: &mut HleCtx) -> Flow {
    let size = c.cpu.arg(1);
    let site = site(c);
    Flow::Wrap(Box::new(move |c, p| record_alloc(c, p, size, &site)))
}

fn record_alloc(c: &mut HleCtx, p: u32, size: u32, site: &Site) {
    // TLSF block header: the block size (with two flag bits) is the word before the pointer.
    let extent = if p != 0 { c.mem.read_u32(p.wrapping_sub(4)).unwrap_or(0) & !3 } else { 0 };
    if p != 0
        && let Some(mc) = c.mem.memcheck()
    {
        mc.on_alloc(p, size, extent, site);
    }
}

/// void multi_heap_free(heap, void *p)
fn heap_free(c: &mut HleCtx) -> Flow {
    let (heap, p) = (c.cpu.arg(0), c.cpu.arg(1));
    if heap == 0 || p == 0 {
        return Flow::Continue;
    }
    // A block leaving the quarantine: really free it. It stays in `releasing` until the free
    // returns (release_all), since an interrupt taken before the free's first instruction
    // runs this hook again when the task resumes.
    let tcb = current_tcb(c, c.cpu.core_id());
    if c.mem.memcheck().is_some_and(|mc| mc.releasing.contains(&(p, tcb))) {
        return Flow::Continue;
    }
    let site = site(c);
    let now = c.env.now_ns();
    let Some(mc) = c.mem.memcheck() else { return Flow::Continue };
    match mc.on_free(p, &site, c.syms, now) {
        Free::Ok { extent } => {
            // Free the blocks leaving the quarantine instead.
            let leaving = mc.quarantine(heap, p, extent);
            release_all(c, leaving, None)
        }
        Free::Bad(v) => {
            // Keep the allocator's state intact: the bad free doesn't happen.
            raise(c, *v);
            Flow::Return(None)
        }
    }
}

/// size_t multi_heap_get_allocated_size(heap, void *p): the size asked for, for a live block.
/// When a realloc can't grow a block in its heap, `heap_caps_realloc` moves it to another
/// one, copying this many bytes: the whole TLSF block would read past what was asked for.
fn heap_allocated_size(c: &mut HleCtx) -> Flow {
    let p = c.cpu.arg(1);
    match c.mem.memcheck().and_then(|mc| mc.live_size(p)) {
        Some(n) => Flow::Return(Some(n)),
        None => Flow::Continue,
    }
}

/// void *multi_heap_realloc(heap, void *p, size_t size)
fn heap_realloc(c: &mut HleCtx) -> Flow {
    let (heap, p, size) = (c.cpu.arg(0), c.cpu.arg(1), c.cpu.arg(2));
    let Some(mc) = c.mem.memcheck() else { return Flow::Continue };
    let Bindings { malloc, free, .. } = mc.bindings;
    if heap == 0 || malloc == 0 || free == 0 {
        return Flow::Continue;
    }
    let site = site(c);
    if p == 0 {
        return Flow::Wrap(Box::new(move |c, q| record_alloc(c, q, size, &site)));
    }
    let now = c.env.now_ns();
    let Some(mc) = c.mem.memcheck() else { return Flow::Continue };
    let Some(old_size) = mc.live_size(p) else {
        if let Free::Bad(v) = mc.on_free(p, &site, c.syms, now) {
            raise(c, *v);
        }
        return Flow::Return(Some(0));
    };
    if size == 0 {
        return release(c, heap, p, &site, 0);
    }
    Flow::Call {
        func: malloc,
        args: vec![heap, size],
        then: Box::new(move |c, q| {
            if q == 0 {
                if let Some(mc) = c.mem.memcheck() {
                    mc.unpoison_slack(p);
                }
                return Flow::Return(Some(0));
            }
            if let Some(data) = c.mem.read_bytes(p, old_size.min(size) as usize) {
                c.mem.write_bytes(q, &data);
            }
            if let Some(mc) = c.mem.memcheck() {
                mc.set_alloc_site(q, &site);
            }
            release(c, heap, p, &site, q)
        }),
    }
}

/// Free live block `p` on behalf of realloc (through the quarantine), then return `ret`.
fn release(c: &mut HleCtx, heap: u32, p: u32, site: &Site, ret: u32) -> Flow {
    let now = c.env.now_ns();
    let Some(mc) = c.mem.memcheck() else { return Flow::Return(Some(ret)) };
    let Free::Ok { extent } = mc.on_free(p, site, c.syms, now) else { return Flow::Return(Some(ret)) };
    let leaving = mc.quarantine(heap, p, extent);
    release_all(c, leaving, Some(ret))
}

/// Really free `blocks` (leaving the quarantine) one after another through the allocator,
/// then return `ret`.
fn release_all(c: &mut HleCtx, mut blocks: Vec<(u32, u32)>, ret: Option<u32>) -> Flow {
    let Some((h, q)) = blocks.pop() else { return Flow::Return(ret) };
    let tcb = current_tcb(c, c.cpu.core_id());
    let Some(mc) = c.mem.memcheck() else { return Flow::Return(ret) };
    mc.releasing.push((q, tcb));
    let free = mc.bindings.free;
    Flow::Call {
        func: free,
        args: vec![h, q],
        then: Box::new(move |c, _| {
            if let Some(mc) = c.mem.memcheck()
                && let Some(i) = mc.releasing.iter().position(|&r| r == (q, tcb))
            {
                mc.releasing.swap_remove(i);
            }
            release_all(c, blocks, ret)
        }),
    }
}

/// static void prvAddNewTaskToReadyList(TCB_t *pxNewTCB, ...): the TCB is filled in.
fn task_created(c: &mut HleCtx) -> Flow {
    let tcb = c.cpu.arg(0);
    // IDF 4.4's Xtensa port has one more word before pxStack (the S3's Arduino 2 builds): the
    // fields are where the task name reads as one.
    let shift = [0, 4]
        .into_iter()
        .find(|s| {
            c.mem.read_bytes(tcb + TCB_NAME + s, 16).is_some_and(|b| {
                let n = b.iter().position(|&x| x == 0).unwrap_or(16);
                n > 0 && b[..n].iter().all(|x| x.is_ascii_graphic())
            })
        })
        .unwrap_or(0);
    let name = super::idf::read_cstr(c, tcb + TCB_NAME + shift, 16);
    let stack = c.mem.read_u32(tcb + TCB_STACK + shift).unwrap_or(0);
    let looks_like_end = |e: &u32| *e > stack && e - stack < 1 << 20;
    let end = [TCB_END_OF_STACK, TCB_END_OF_STACK_UNICORE]
        .iter()
        .filter_map(|off| c.mem.read_u32(tcb + off + shift))
        .find(looks_like_end)
        .unwrap_or(0);
    if let Some(mc) = c.mem.memcheck() {
        // pxEndOfStack is the aligned-down top: prefer the block size if the stack is on the heap.
        let size = match (mc.live_size(stack), end.wrapping_sub(stack).wrapping_add(4)) {
            (Some(s), _) => s,
            (None, s) if s < 1 << 20 => s,
            _ => 0,
        };
        if stack != 0 && size != 0 {
            mc.task_created(tcb, name, stack, size);
        }
    }
    Flow::Continue
}

/// static void prvDeleteTCB(TCB_t *pxTCB): take the final high-water mark first.
fn task_deleted(c: &mut HleCtx) -> Flow {
    let tcb = c.cpu.arg(0);
    let Some((_, stack, len)) = c.mem.memcheck().and_then(|mc| mc.stacks_to_scan().into_iter().find(|s| s.0 == tcb))
    else {
        return Flow::Continue;
    };
    let free = c.mem.read_bytes(stack, len as usize).map(|b| memcheck::stack_free(&b));
    if let Some(mc) = c.mem.memcheck() {
        if let Some(f) = free {
            mc.update_stack(tcb, f);
        }
        mc.task_deleted(tcb);
    }
    Flow::Continue
}
