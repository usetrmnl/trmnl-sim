//! Firmware code coverage (`--coverage FILE`).
//!
//! The SoC run loops mark every executed instruction address in a bitmap over the
//! app ELF's executable sections ([`Coverage::hit`]). A report maps the bitmap to
//! source lines through the ELF's DWARF line tables ([`dwarf::LineMap`]) and writes an
//! lcov tracefile: a line is hit if any instruction attributed to it executed, and
//! reported with count 0 if it has code that never ran.
//!
//! Each known app ELF (`firmware.elf`, plus an `--ota-firmware` build booted after an OTA) has
//! its own bitmap; the one the bootloader starts is active. Bitmaps survive resets
//! and deep sleep, so a run accumulates everything the firmware did. Functions
//! replaced by an HLE hook never run their guest body; unless it ran anyway (a hook
//! that continues into the original), such a function is left out of the report.

pub mod dwarf;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sim_api::CoverageSummary;

pub use dwarf::PathFilter;

/// One bit per byte address of a code range (Xtensa instructions may start at any byte).
#[derive(Clone)]
pub struct Region {
    base: u32,
    len: u32,
    bits: Vec<u64>,
}

impl Region {
    pub fn new(start: u32, end: u32) -> Self {
        let len = end - start;
        Region { base: start, len, bits: vec![0; (len as usize).div_ceil(64)] }
    }

    #[inline(always)]
    fn mark(&mut self, addr: u32) -> bool {
        let off = addr.wrapping_sub(self.base);
        if off < self.len {
            self.bits[(off >> 6) as usize] |= 1 << (off & 63);
            true
        } else {
            false
        }
    }

    /// Whether any instruction starting in `[start, end)` executed (clipped to the region).
    pub fn any(&self, start: u32, end: u32) -> bool {
        let lo = start.max(self.base) - self.base;
        let hi = end.min(self.base + self.len).saturating_sub(self.base);
        if lo >= hi {
            return false;
        }
        let (wa, wb) = ((lo >> 6) as usize, ((hi - 1) >> 6) as usize);
        (wa..=wb).any(|w| {
            let mut m = u64::MAX;
            if w == wa {
                m &= u64::MAX << (lo & 63);
            }
            if w == wb {
                m &= u64::MAX >> (63 - ((hi - 1) & 63));
            }
            self.bits[w] & m != 0
        })
    }
}

struct Image {
    /// Code ranges of the ELF (`[start, end)`), for mapping.
    ranges: Vec<(u32, u32)>,
    /// Its bitmaps, while another image is active (the active one's are in `live`).
    regions: Vec<Region>,
    /// The bootloader started this app at least once.
    booted: bool,
    /// Entry points of HLE hooks installed for this app.
    hooked: Vec<u32>,
}

/// Executed-instruction bitmaps of every known app image.
pub struct Coverage {
    images: Vec<Image>,
    active: Option<usize>,
    /// Bitmaps of the active image, largest region first.
    live: Vec<Region>,
}

impl Coverage {
    /// One image per app ELF, in the SoC's app order.
    pub fn new(elfs: &[&[u8]]) -> Result<Self> {
        let mut images = Vec::new();
        for elf in elfs {
            let ranges = dwarf::code_ranges(elf)?;
            let mut regions: Vec<Region> = ranges.iter().map(|&(s, e)| Region::new(s, e)).collect();
            regions.sort_by_key(|r| std::cmp::Reverse(r.len));
            images.push(Image { ranges, regions, booted: false, hooked: Vec::new() });
        }
        Ok(Coverage { images, active: None, live: Vec::new() })
    }

    /// App `i` is about to run, with HLE hooks at `hooked`.
    pub fn activate(&mut self, i: usize, hooked: impl IntoIterator<Item = u32>) {
        // An app added after coverage started (an OTA file picked in the window) isn't
        // recorded: park the live regions so its code doesn't mark the previous app's.
        if i >= self.images.len() {
            if let Some(a) = self.active.take() {
                std::mem::swap(&mut self.live, &mut self.images[a].regions);
            }
            return;
        }
        if self.active != Some(i) {
            if let Some(a) = self.active {
                std::mem::swap(&mut self.live, &mut self.images[a].regions);
            }
            std::mem::swap(&mut self.live, &mut self.images[i].regions);
            self.active = Some(i);
        }
        let img = &mut self.images[i];
        img.booted = true;
        img.hooked = hooked.into_iter().collect();
    }

    /// Record an instruction about to execute at `pc`.
    #[inline(always)]
    pub fn hit(&mut self, pc: u32) {
        for r in &mut self.live {
            if r.mark(pc) {
                return;
            }
        }
    }

    /// Forget everything recorded so far.
    pub fn clear(&mut self) {
        for r in self.live.iter_mut().chain(self.images.iter_mut().flat_map(|i| i.regions.iter_mut())) {
            r.bits.fill(0);
        }
    }

    fn regions(&self, i: usize) -> &[Region] {
        if self.active == Some(i) { &self.live } else { &self.images[i].regions }
    }

    fn executed(&self, i: usize, start: u32, end: u32) -> bool {
        self.regions(i).iter().any(|r| r.any(start, end))
    }
}

/// Per-file results, merged over app images by path and line.
#[derive(Default, Debug, PartialEq)]
pub struct FileCoverage {
    /// Line -> executed.
    pub lines: BTreeMap<u32, bool>,
    /// Function name -> (line, executed).
    pub functions: BTreeMap<String, (u32, bool)>,
}

/// Maps coverage bitmaps to source lines and writes lcov. Line tables are read once
/// per ELF, when first needed.
pub struct Reporter {
    elfs: Vec<PathBuf>,
    maps: Vec<Option<dwarf::LineMap>>,
    filter: PathFilter,
    /// Default output path (`--coverage FILE`).
    pub path: PathBuf,
}

impl Reporter {
    /// `elfs` in the SoC's app order (the same as for [`Coverage::new`]).
    pub fn new(elfs: Vec<PathBuf>, filter: PathFilter, path: PathBuf) -> Self {
        let maps = elfs.iter().map(|_| None).collect();
        Reporter { elfs, maps, filter, path }
    }

    fn map(&mut self, i: usize, cov: &Coverage) -> Result<&dwarf::LineMap> {
        if self.maps[i].is_none() {
            let path = &self.elfs[i];
            let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            self.maps[i] = Some(dwarf::LineMap::from_elf(&data, &cov.images[i].ranges, &self.filter)?);
        }
        Ok(self.maps[i].as_ref().unwrap())
    }

    /// Source-level results for every app image that has booted.
    pub fn collect(&mut self, cov: &Coverage) -> Result<BTreeMap<String, FileCoverage>> {
        let mut files: BTreeMap<String, FileCoverage> = BTreeMap::new();
        for i in 0..cov.images.len() {
            if cov.images[i].booted {
                let map = self.map(i, cov)?;
                merge_image(&mut files, map, |s, e| cov.executed(i, s, e), &cov.images[i].hooked);
            }
        }
        Ok(files)
    }

    /// Write the lcov tracefile to `path` (default: the `--coverage` path).
    pub fn write(&mut self, cov: &Coverage, path: Option<&Path>) -> Result<CoverageSummary> {
        let path = path.unwrap_or(&self.path).to_path_buf();
        let files = self.collect(cov)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, lcov(&files)).with_context(|| format!("writing {}", path.display()))?;
        let mut s = summarize(&files);
        s.path = path.display().to_string();
        Ok(s)
    }
}

/// Add one image's lines and functions to `files`. `executed(start, end)` tells whether
/// any instruction in the range ran. Functions hooked by HLE whose entry never ran are
/// left out, lines and all.
fn merge_image(
    files: &mut BTreeMap<String, FileCoverage>,
    map: &dwarf::LineMap,
    executed: impl Fn(u32, u32) -> bool,
    hooked: &[u32],
) {
    let mut skip: Vec<(u32, u32)> = hooked
        .iter()
        .filter_map(|&a| map.function_at(a))
        .filter(|f| !executed(f.addr, f.addr + 1))
        .map(|f| (f.addr, f.addr + f.size))
        .collect();
    skip.sort();
    let skipped = |a: u32| {
        let i = skip.partition_point(|s| s.0 <= a);
        skip[..i].last().is_some_and(|s| a < s.1)
    };
    for r in &map.ranges {
        if skipped(r.start) {
            continue;
        }
        let f = files.entry(map.files[r.file as usize].clone()).or_default();
        *f.lines.entry(r.line).or_default() |= executed(r.start, r.end);
    }
    for func in &map.functions {
        if skipped(func.addr) {
            continue;
        }
        let f = files.entry(map.files[func.file as usize].clone()).or_default();
        let e = f.functions.entry(func.name.clone()).or_insert((func.line, false));
        e.1 |= executed(func.addr, func.addr + 1);
    }
}

/// An lcov tracefile. Hit counts are 0 or 1: the recorder keeps no counts.
pub fn lcov(files: &BTreeMap<String, FileCoverage>) -> String {
    let mut s = String::from("TN:\n");
    for (path, f) in files {
        let _ = writeln!(s, "SF:{path}");
        let mut funcs: Vec<_> = f.functions.iter().collect();
        funcs.sort_by_key(|(name, (line, _))| (*line, *name));
        for (name, (line, _)) in &funcs {
            let _ = writeln!(s, "FN:{line},{name}");
        }
        for (name, (_, hit)) in &funcs {
            let _ = writeln!(s, "FNDA:{},{name}", *hit as u8);
        }
        let _ = writeln!(s, "FNF:{}\nFNH:{}", funcs.len(), funcs.iter().filter(|f| f.1.1).count());
        for (line, hit) in &f.lines {
            let _ = writeln!(s, "DA:{line},{}", *hit as u8);
        }
        let _ = writeln!(s, "LF:{}\nLH:{}", f.lines.len(), f.lines.values().filter(|h| **h).count());
        s += "end_of_record\n";
    }
    s
}

pub fn summarize(files: &BTreeMap<String, FileCoverage>) -> CoverageSummary {
    let mut s = CoverageSummary { files: files.len() as u64, ..Default::default() };
    for f in files.values() {
        s.lines_found += f.lines.len() as u64;
        s.lines_hit += f.lines.values().filter(|h| **h).count() as u64;
        s.functions_found += f.functions.len() as u64;
        s.functions_hit += f.functions.values().filter(|f| f.1).count() as u64;
    }
    s
}
