//! Source lines and functions of an ELF, from its DWARF line programs and symbol
//! table, for coverage reports.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

/// Which source files a report covers, and how their paths are written.
#[derive(Clone, Debug, Default)]
pub struct PathFilter {
    /// Paths under this directory are written relative to it (e.g. the firmware checkout).
    pub root: Option<PathBuf>,
    /// Keep only files whose (root-relative) path starts with one of these; empty = all.
    pub include: Vec<String>,
}

impl PathFilter {
    /// The path of `p` in reports, or `None` if the file is filtered out.
    pub fn display(&self, p: &Path) -> Option<String> {
        let p = normalize(p);
        let shown = match self.root.as_deref().and_then(|r| p.strip_prefix(r).ok()) {
            Some(rel) => rel.to_string_lossy().into_owned(),
            None => p.to_string_lossy().into_owned(),
        };
        (self.include.is_empty() || self.include.iter().any(|i| shown.starts_with(i.as_str()))).then_some(shown)
    }
}

/// Resolve `.` and `..` without touching the file system (the sources may not exist here).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Code addresses `[start, end)` attributed to one source line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
    pub file: u32,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub addr: u32,
    pub size: u32,
    /// Demangled name.
    pub name: String,
    /// Source position of the entry point (`file` indexes `LineMap::files`).
    pub file: u32,
    pub line: u32,
}

/// Everything a report needs from one ELF.
#[derive(Default)]
pub struct LineMap {
    /// Source files as written in reports.
    pub files: Vec<String>,
    /// Sorted by `start`. Inlined code is attributed to the line it was inlined from
    /// (the header or callee), as in the line table.
    pub ranges: Vec<LineRange>,
    /// Sorted by `addr`.
    pub functions: Vec<Function>,
}

impl LineMap {
    /// Read the line tables of `data`, keeping only rows inside the `code` ranges
    /// (executable sections; code dropped by `--gc-sections` is left at address 0).
    pub fn from_elf(data: &[u8], code: &[(u32, u32)], filter: &PathFilter) -> Result<Self> {
        let obj = object::File::parse(data)?;
        let in_code = |a: u64, b: u64| code.iter().any(|&(s, e)| a >= s as u64 && b <= e as u64);
        let sections = gimli::DwarfSections::load(|id| -> Result<Cow<[u8]>> {
            Ok(match obj.section_by_name(id.name()) {
                Some(s) => s.uncompressed_data()?,
                None => Cow::Borrowed(&[]),
            })
        })?;
        let dwarf = sections.borrow(|s| gimli::EndianSlice::new(s, gimli::LittleEndian));

        let mut map = LineMap::default();
        // Report path -> file id; `None` for files the filter drops.
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut units = dwarf.units();
        while let Some(header) = units.next()? {
            let unit = dwarf.unit(header)?;
            let Some(program) = unit.line_program.clone() else { continue };
            let comp_dir = unit.comp_dir.map(|d| PathBuf::from(d.to_string_lossy().into_owned()));
            // Line-table file index -> file id, per unit.
            let mut files: HashMap<u64, Option<u32>> = HashMap::new();
            let mut rows = program.rows();
            let mut prev: Option<(u64, Option<u32>, u32)> = None;
            while let Some((header, row)) = rows.next_row()? {
                if let Some((start, file, line)) = prev.take()
                    && let Some(file) = file
                    && line != 0
                    && row.address() > start
                    && in_code(start, row.address())
                {
                    map.ranges.push(LineRange { start: start as u32, end: row.address() as u32, file, line });
                }
                if row.end_sequence() {
                    continue;
                }
                let idx = row.file_index();
                let file = match files.get(&idx) {
                    Some(f) => *f,
                    None => {
                        let f = file_path(&dwarf, &unit, header, idx, comp_dir.as_deref())
                            .and_then(|p| filter.display(&p))
                            .map(|shown| {
                                let next = ids.len() as u32;
                                *ids.entry(shown.clone()).or_insert_with(|| {
                                    map.files.push(shown);
                                    next
                                })
                            });
                        files.insert(idx, f);
                        f
                    }
                };
                prev = Some((row.address(), file, row.line().map_or(0, |l| l.get() as u32)));
            }
        }
        map.ranges.sort_by_key(|r| (r.start, r.end));

        let mut seen = std::collections::HashSet::new();
        for sym in obj.symbols() {
            let (addr, size) = (sym.address(), sym.size());
            if sym.kind() != SymbolKind::Text || size == 0 || !in_code(addr, addr + size) || !seen.insert(addr) {
                continue;
            }
            let Ok(raw) = sym.name() else { continue };
            let Some(r) = map.range_at(addr as u32) else { continue };
            let (file, line) = (r.file, r.line);
            map.functions.push(Function { addr: addr as u32, size: size as u32, name: demangle(raw), file, line });
        }
        map.functions.sort_by_key(|f| f.addr);
        Ok(map)
    }

    /// The line range containing `addr`.
    pub fn range_at(&self, addr: u32) -> Option<&LineRange> {
        let i = self.ranges.partition_point(|r| r.start <= addr);
        self.ranges[..i].last().filter(|r| addr < r.end)
    }

    pub fn function_at(&self, addr: u32) -> Option<&Function> {
        self.functions.binary_search_by_key(&addr, |f| f.addr).ok().map(|i| &self.functions[i])
    }
}

fn file_path<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    header: &gimli::LineProgramHeader<R>,
    idx: u64,
    comp_dir: Option<&Path>,
) -> Option<PathBuf> {
    let file = header.file(idx)?;
    let mut p = comp_dir.map(Path::to_path_buf).unwrap_or_default();
    if let Some(dir) = file.directory(header) {
        p.push(dwarf.attr_string(unit, dir).ok()?.to_string_lossy().ok()?.as_ref());
    }
    p.push(dwarf.attr_string(unit, file.path_name()).ok()?.to_string_lossy().ok()?.as_ref());
    Some(p)
}

fn demangle(name: &str) -> String {
    cpp_demangle::Symbol::new(name).ok().and_then(|s| s.demangle().ok()).unwrap_or_else(|| name.to_string())
}

/// Executable sections of an ELF as `[start, end)` ranges, sorted, with ranges less
/// than 64 KiB apart merged (so a coverage bitmap needs only a few regions).
pub fn code_ranges(data: &[u8]) -> Result<Vec<(u32, u32)>> {
    let obj = object::File::parse(data)?;
    let mut v: Vec<(u32, u32)> = obj
        .sections()
        .filter(|s| s.kind() == object::SectionKind::Text && s.address() != 0 && s.size() != 0)
        .map(|s| (s.address() as u32, (s.address() + s.size()) as u32))
        .collect();
    v.sort();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add(0x1_0000) => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    Ok(out)
}
