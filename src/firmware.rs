//! Loading build artifacts: flash images, ELF symbols, ROM images.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

/// Symbol table of an ELF, for HLE hooks and symbolized diagnostics.
#[derive(Default, Clone)]
pub struct Symbols {
    by_name: HashMap<String, u32>,
    /// Sorted (addr, size, name) of functions/objects.
    sorted: Vec<(u32, u32, String)>,
}

impl Symbols {
    pub fn from_elf(data: &[u8]) -> Result<Self> {
        let obj = object::File::parse(data)?;
        let mut s = Symbols::default();
        for sym in obj.symbols() {
            let Ok(name) = sym.name() else { continue };
            if name.is_empty() || name.starts_with('$') || name.starts_with(".L") {
                continue;
            }
            let addr = sym.address() as u32;
            if addr == 0 {
                continue;
            }
            if sym.is_global() || !s.by_name.contains_key(name) {
                s.by_name.insert(name.to_string(), addr);
            }
            if matches!(sym.kind(), object::SymbolKind::Text | object::SymbolKind::Data | object::SymbolKind::Unknown) {
                s.sorted.push((addr, sym.size() as u32, name.to_string()));
            }
        }
        s.sorted.sort();
        Ok(s)
    }

    pub fn merge(&mut self, other: &Symbols) {
        for (k, v) in &other.by_name {
            self.by_name.entry(k.clone()).or_insert(*v);
        }
        self.sorted.extend(other.sorted.iter().cloned());
        self.sorted.sort();
    }

    pub fn addr(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    /// "func+0x12" for an address.
    pub fn describe(&self, addr: u32) -> String {
        let i = self.sorted.partition_point(|(a, _, _)| *a <= addr);
        if i > 0 {
            let (a, size, name) = &self.sorted[i - 1];
            if addr - a < (*size).max(0x4000) {
                return format!("{}+{:#x}", demangle(name), addr - a);
            }
        }
        format!("{addr:#010x}")
    }

    /// Address ranges (start, end) of the sized symbols whose name satisfies `pred`.
    pub fn ranges(&self, pred: impl Fn(&str) -> bool) -> Vec<(u32, u32)> {
        self.sorted
            .iter()
            .filter(|(_, size, name)| *size > 0 && pred(name))
            .map(|(a, size, _)| (a & !1, (a & !1) + size))
            .collect()
    }

    pub fn names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }
}

fn demangle(s: &str) -> String {
    // Keep it light: C++ names are recognisable enough mangled for diagnostics.
    s.to_string()
}

/// Initialized sections of a ROM ELF: (address, bytes).
pub fn rom_sections(data: &[u8]) -> Result<Vec<(u32, Vec<u8>)>> {
    let obj = object::File::parse(data)?;
    let mut out = Vec::new();
    for sec in obj.sections() {
        if sec.size() == 0 || matches!(sec.kind(), SectionKind::UninitializedData | SectionKind::Metadata) {
            continue;
        }
        let Ok(bytes) = sec.data() else { continue };
        if bytes.is_empty() || sec.address() == 0 {
            continue;
        }
        out.push((sec.address() as u32, bytes.to_vec()));
    }
    Ok(out)
}

/// Everything needed to boot a firmware build.
pub struct Firmware {
    pub name: String,
    /// SHA-256 of the ELF file; the app image records it in esp_app_desc_t.
    pub elf_sha256: [u8; 32],
    pub symbols: Symbols,
    /// (flash offset, bytes) images to write into flash, like `esptool write_flash`.
    pub images: Vec<(u32, Vec<u8>)>,
    /// ESP image header chip id (5 = ESP32-C3, 9 = ESP32-S3, 23 = ESP32-C5).
    pub chip_id: u16,
    /// Flash size in bytes, from the bootloader image header.
    pub flash_size: usize,
}

pub const CHIP_ESP32C3: u16 = 5;
pub const CHIP_ESP32S3: u16 = 9;
pub const CHIP_ESP32C5: u16 = 23;

/// Where the chip's ROM looks for the 2nd-stage bootloader in flash.
pub fn bootloader_offset(chip_id: u16) -> u32 {
    match chip_id {
        CHIP_ESP32C5 => 0x2000,
        _ => 0,
    }
}

/// Offset of the first app partition (factory, else ota_0) in a partition table image.
fn table_app_offset(table: &[u8]) -> Option<u32> {
    let mut ota0 = None;
    for e in table.chunks(32) {
        if e.len() < 32 || e[0] != 0xAA || e[1] != 0x50 {
            break;
        }
        let off = u32::from_le_bytes(e[4..8].try_into().ok()?);
        match (e[2], e[3]) {
            (0, 0) => return Some(off),
            (0, 0x10) => ota0 = Some(off),
            _ => {}
        }
    }
    ota0
}

/// The parts of a merged image to write into flash. merge_bin pads the gaps with 0xFF, so
/// writing all of it would erase NVS on every start: keep to what flashing the split images
/// writes, the bootloader and partition table, then each partition the image has contents
/// for (the app, a filesystem image).
fn split_merged(data: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let table = &data[0x8000.min(data.len())..0x8c00.min(data.len())];
    let mut images = vec![(0, data[..0x8c00.min(data.len())].to_vec())];
    for e in table.chunks_exact(32).take_while(|e| e[..2] == [0xAA, 0x50]) {
        let off = u32::from_le_bytes([e[4], e[5], e[6], e[7]]) as usize;
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        if let Some(part) = data.get(off.min(data.len())..(off + size).min(data.len()))
            && part.iter().any(|&b| b != 0xff)
        {
            images.push((off as u32, part.to_vec()));
        }
    }
    images
}

impl Firmware {
    /// Load a merged flash image (`merged_firmware.bin`, everything `esptool write_flash 0x0`
    /// would write) and its ELF.
    pub fn from_merged(path: &Path, elf_path: &Path) -> Result<Self> {
        let elf = std::fs::read(elf_path).with_context(|| format!("reading {}", elf_path.display()))?;
        let elf_sha256 = sha256(&elf);
        let symbols = Symbols::from_elf(&elf)?;
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        // A merged image starts at 0 even where the bootloader lives at 0x2000 (ESP32-C5).
        if data.first() != Some(&0xE9) && data.get(0x2000) != Some(&0xE9) {
            bail!("{}: no bootloader image at 0 or 0x2000", path.display());
        }
        if data.get(0x8000..0x8002) != Some(&[0xAA, 0x50]) {
            bail!("{}: no partition table at 0x8000; is it an app image rather than a merged one?", path.display());
        }
        let images = split_merged(&data);
        let boot = image_at(&images, 0)
            .filter(|b| b.first() == Some(&0xE9))
            .or_else(|| image_at(&images, 0x2000))
            .context("no bootloader image")?;
        let flash_size = match boot.get(3).map(|b| b >> 4) {
            Some(0) => 1 << 20,
            Some(1) => 2 << 20,
            Some(3) => 8 << 20,
            Some(4) => 16 << 20,
            Some(5) => 32 << 20,
            _ => 4 << 20,
        };
        let chip_id = u16::from_le_bytes([boot[12], boot[13]]);
        let name = app_desc(&images).unwrap_or_else(|| path.display().to_string());
        Ok(Firmware { name, elf_sha256, symbols, images, chip_id, flash_size })
    }
}

/// An app ELF that may be installed later (e.g. by OTA): symbols only.
pub struct ExtraApp {
    pub name: String,
    pub elf_sha256: [u8; 32],
    pub symbols: Symbols,
}

impl ExtraApp {
    pub fn from_elf(path: &Path) -> Result<Self> {
        let elf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(ExtraApp { name: path.display().to_string(), elf_sha256: sha256(&elf), symbols: Symbols::from_elf(&elf)? })
    }
}

/// Where to look for the ELF of an app booted without one (installed by an OTA update from a
/// server that only sent the image). Its app descriptor records its ELF's SHA-256, so any
/// `.elf` in these directories that hashes to it is the one.
#[derive(Default)]
pub struct ElfSearch {
    dirs: Vec<PathBuf>,
    /// SHA-256 of the files hashed so far, by path and modification time.
    seen: HashMap<PathBuf, (std::time::SystemTime, [u8; 32])>,
}

impl ElfSearch {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        let mut unique = std::collections::HashSet::new();
        let dirs = dirs
            .into_iter()
            .map(|d| if d.as_os_str().is_empty() { PathBuf::from(".") } else { d })
            .filter(|d| unique.insert(d.clone()))
            .collect();
        ElfSearch { dirs, seen: HashMap::new() }
    }

    /// The ELF whose SHA-256 is `sha`, if one of the directories has it.
    pub fn find(&mut self, sha: &[u8; 32]) -> Option<ExtraApp> {
        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for path in entries.flatten().map(|e| e.path()) {
                if path.extension().is_none_or(|e| e != "elf") {
                    continue;
                }
                let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else { continue };
                let hash = match self.seen.get(&path) {
                    Some((t, h)) if *t == mtime => *h,
                    _ => {
                        let Ok(data) = std::fs::read(&path) else { continue };
                        let h = sha256(&data);
                        self.seen.insert(path.clone(), (mtime, h));
                        h
                    }
                };
                if hash == *sha {
                    return ExtraApp::from_elf(&path).ok();
                }
            }
        }
        None
    }

    /// Why an app at `off` can't run: no ELF anywhere.
    pub fn not_found(&self, off: u32, version: &str, sha: &[u8; 32]) -> String {
        let dirs: Vec<String> = self.dirs.iter().map(|d| d.display().to_string()).collect();
        format!(
            "the app at {off:#x} (version {version}, ELF sha256 {}) has no matching ELF, and none of {} \
             has one; HLE needs its symbols. Put its ELF there, or pass --elf-dir",
            hex(sha),
            if dirs.is_empty() { "no directories".into() } else { dirs.join(", ") }
        )
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(data).into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An app image in flash, as described by its esp_app_desc_t.
pub struct BootApp {
    pub offset: u32,
    pub elf_sha256: [u8; 32],
    pub version: String,
    pub idf_version: String,
}

/// The app the 2nd-stage bootloader will boot, following the same otadata rules
/// (highest valid sequence number selects ota_N; else factory; else ota_0).
pub fn booting_app(flash: &[u8]) -> Option<BootApp> {
    let mut ota_apps: Vec<(u8, u32)> = Vec::new();
    let mut factory = None;
    let mut otadata = None;
    for e in flash.get(0x8000..0x8c00)?.chunks(32) {
        if e[0] != 0xAA || e[1] != 0x50 {
            break;
        }
        let off = u32::from_le_bytes(e[4..8].try_into().ok()?);
        match (e[2], e[3]) {
            (0, 0) => factory = Some(off),
            (0, st @ 0x10..=0x1f) => ota_apps.push((st, off)),
            (1, 0) => otadata = Some(off),
            _ => {}
        }
    }
    ota_apps.sort();
    let mut chosen = None;
    if let (Some(od), false) = (otadata, ota_apps.is_empty()) {
        let seq = |o: u32| -> Option<u32> {
            let v = u32::from_le_bytes(flash.get(o as usize..o as usize + 4)?.try_into().ok()?);
            (v != 0 && v != u32::MAX).then_some(v)
        };
        if let Some(s) = [seq(od), seq(od + 0x1000)].into_iter().flatten().max() {
            chosen = Some(ota_apps[((s - 1) as usize) % ota_apps.len()].1);
        }
    }
    let off = chosen.or(factory).or(ota_apps.first().map(|a| a.1))?;
    let d = flash.get(off as usize + 32..off as usize + 32 + 256)?;
    if u32::from_le_bytes(d[0..4].try_into().ok()?) != 0xABCD5432 {
        return None;
    }
    let text = |r: std::ops::Range<usize>| String::from_utf8_lossy(&d[r]).trim_end_matches('\0').to_string();
    Some(BootApp {
        offset: off,
        elf_sha256: d[144..176].try_into().ok()?,
        version: text(16..48),
        idf_version: text(112..144),
    })
}

/// The bytes at flash offset `off`, from whichever image covers it.
fn image_at(images: &[(u32, Vec<u8>)], off: u32) -> Option<&[u8]> {
    images.iter().find_map(|(o, d)| {
        let rel = off.checked_sub(*o)? as usize;
        d.get(rel..)
    })
}

/// "project version (idf version)" from the esp_app_desc_t of the app image.
fn app_desc(images: &[(u32, Vec<u8>)]) -> Option<String> {
    let table = image_at(images, 0x8000)?;
    let app = image_at(images, table_app_offset(table)?)?;
    // image header (24) + first segment header (8) -> esp_app_desc_t
    let d = app.get(32..32 + 256)?;
    if u32::from_le_bytes(d[0..4].try_into().ok()?) != 0xABCD5432 {
        return None;
    }
    let s = |r: std::ops::Range<usize>| String::from_utf8_lossy(&d[r]).trim_end_matches('\0').to_string();
    Some(format!("{} {} (IDF {})", s(48..80), s(16..48), s(112..144)))
}

/// Create or update the persistent flash file: write the firmware images like a
/// USB flash would, keeping everything else (NVS, SPIFFS) unless `erase`.
pub fn prepare_flash(path: &Path, size: usize, fw: &Firmware, erase: bool) -> Result<Vec<u8>> {
    let mut flash = match std::fs::read(path) {
        Ok(d) if d.len() == size && !erase => d,
        _ => vec![0xff; size],
    };
    write_images(&mut flash, fw)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, &flash)?;
    Ok(flash)
}

/// Write the firmware into `flash` like a USB flash: its images, and a blank otadata so its app
/// boots. NVS and SPIFFS are left alone.
pub fn write_images(flash: &mut [u8], fw: &Firmware) -> Result<()> {
    for (off, img) in &fw.images {
        let off = *off as usize;
        if off + img.len() > flash.len() {
            bail!("image at {off:#x} does not fit in flash");
        }
        flash[off..off + img.len()].copy_from_slice(img);
    }
    // PlatformIO also writes boot_app0.bin (blank otadata) so the fresh app boots.
    if let Some((ota_off, ota_len)) = find_partition(flash, 1, 0) {
        flash[ota_off as usize..(ota_off + ota_len) as usize].fill(0xff);
    }
    Ok(())
}

/// `flash` with `fw` written in, for loading a new build into a running device: it must be for
/// the same chip, and its app must be the one its ELF describes (they're written one after the
/// other, so a build in progress may not match yet).
pub fn install(flash: &[u8], chip_id: u16, fw: &Firmware) -> Result<Vec<u8>> {
    if fw.chip_id != chip_id {
        bail!("it is for another chip");
    }
    let mut data = flash.to_vec();
    write_images(&mut data, fw)?;
    if booting_app(&data).map(|a| a.elf_sha256) != Some(fw.elf_sha256) {
        bail!("the image and its ELF don't match (still building?)");
    }
    Ok(data)
}

/// Modification time and size of a file, to notice it changing.
pub fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Find a partition by type/subtype in the table at 0x8000: (offset, size).
pub fn find_partition(flash: &[u8], ptype: u8, subtype: u8) -> Option<(u32, u32)> {
    for e in flash.get(0x8000..0x8c00)?.chunks(32) {
        if e[0] != 0xAA || e[1] != 0x50 {
            break;
        }
        if e[2] == ptype && e[3] == subtype {
            let off = u32::from_le_bytes(e[4..8].try_into().ok()?);
            let size = u32::from_le_bytes(e[8..12].try_into().ok()?);
            return Some((off, size));
        }
    }
    None
}

/// Every entry of the partition table at 0x8000.
pub fn partitions(flash: &[u8]) -> Vec<sim_api::PartitionInfo> {
    let Some(table) = flash.get(0x8000..0x8c00) else { return Vec::new() };
    table
        .chunks(32)
        .take_while(|e| e[0] == 0xAA && e[1] == 0x50)
        .map(|e| sim_api::PartitionInfo {
            label: String::from_utf8_lossy(&e[12..28]).trim_end_matches('\0').to_string(),
            kind: e[2],
            subtype: e[3],
            offset: u32::from_le_bytes(e[4..8].try_into().unwrap()),
            size: u32::from_le_bytes(e[8..12].try_into().unwrap()),
        })
        .collect()
}

/// Segments of an ESP image at `off`: (entry, [(load addr, bytes)]).
/// Load address and contents of each segment of an ESP image.
pub type Segments = Vec<(u32, Vec<u8>)>;

pub fn parse_image(flash: &[u8], off: usize) -> Result<(u32, Segments)> {
    let h = flash.get(off..off + 24).context("image header out of range")?;
    if h[0] != 0xE9 {
        bail!("no ESP image at {off:#x} (magic {:#x})", h[0]);
    }
    let n = h[1] as usize;
    let entry = u32::from_le_bytes(h[4..8].try_into()?);
    let mut p = off + 24;
    let mut segs = Vec::new();
    for _ in 0..n {
        let addr = u32::from_le_bytes(flash[p..p + 4].try_into()?);
        let len = u32::from_le_bytes(flash[p + 4..p + 8].try_into()?) as usize;
        segs.push((addr, flash[p + 8..p + 8 + len].to_vec()));
        p += 8 + len;
    }
    Ok((entry, segs))
}

/// Directories searched for ROM ELFs, in order: the simulator checkout's `rom/` (where
/// `scripts/fetch-rom-elfs.sh` puts Espressif's esp-rom-elfs release), `rom/` next to the
/// executable, the user cache (`$XDG_CACHE_HOME` or `~/.cache`, `trmnl-sim/rom-elfs`), then
/// PlatformIO's `tool-esp-rom-elfs` packages (any version).
pub fn rom_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("rom")];
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("rom"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cache =
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".cache")));
    if let Some(c) = cache {
        dirs.push(c.join("trmnl-sim/rom-elfs"));
    }
    if let Some(h) = home {
        let pkgs = h.join(".platformio/packages");
        dirs.push(pkgs.join("tool-esp-rom-elfs"));
        if let Ok(rd) = std::fs::read_dir(&pkgs) {
            let mut more: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tool-esp-rom-elfs@")))
                .collect();
            more.sort();
            dirs.extend(more);
        }
    }
    dirs
}

/// Locate a ROM ELF (e.g. `esp32c3_rev3_rom.elf`) in [`rom_search_dirs`].
pub fn find_rom_elf(name: &str) -> Option<PathBuf> {
    rom_search_dirs().into_iter().map(|d| d.join(name)).find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_search_matches_by_hash() {
        let dir = std::env::temp_dir().join(format!("trmnl-sim-elf-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The test binary stands in for an ELF: the object crate parses it as well.
        let elf = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        std::fs::write(dir.join("other.elf"), b"not this one").unwrap();
        std::fs::write(dir.join("FW1.8.18-trmnl.elf"), &elf).unwrap();
        std::fs::write(dir.join("FW1.8.18-trmnl.bin"), &elf).unwrap();
        let mut search = ElfSearch::new(vec![dir.clone(), dir.clone(), "/nonexistent".into()]);
        let found = search.find(&sha256(&elf)).map(|a| a.name);
        let missing = search.find(&[0; 32]).map(|a| a.name);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(dir.join("FW1.8.18-trmnl.elf").display().to_string()));
        assert_eq!(missing, None);
        assert!(search.not_found(0x10000, "1.8.18", &[0; 32]).contains("--elf-dir"));
    }

    #[test]
    fn merged_image_keeps_empty_partitions() {
        let entry = |ptype: u8, subtype: u8, off: u32, size: u32| {
            let mut e = vec![0xAA, 0x50, ptype, subtype];
            e.extend(off.to_le_bytes());
            e.extend(size.to_le_bytes());
            e.resize(32, 0);
            e
        };
        let mut data = vec![0xff; 0x30000];
        data[0] = 0xE9;
        let table = [entry(1, 2, 0x9000, 0x5000), entry(0, 0, 0x10000, 0x10000), entry(1, 0x82, 0x20000, 0x10000)];
        data[0x8000..0x8060].copy_from_slice(&table.concat());
        data[0x10000] = 0xE9;
        let parts: Vec<(u32, usize)> = split_merged(&data).iter().map(|(o, d)| (*o, d.len())).collect();
        // NVS and the blank filesystem are left alone; the app partition is written.
        assert_eq!(parts, [(0, 0x8c00), (0x10000, 0x10000)]);
    }
}
