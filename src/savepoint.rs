//! Save points: a device captured so a run can later resume from exactly there, in this
//! process or a new one ("onboarded, asleep, image X showing").
//!
//! A save point taken in **deep sleep** holds everything that survives deep sleep on the
//! real device, plus the simulator state needed to resume it faithfully: flash, RTC memory,
//! the RTC_CNTL registers, the S3 cache MMU, virtual time, boot count, the pending wake
//! (timer / GPIO mask), and the board's devices (e-paper image and controller RAM, I2C chip
//! configuration, the modem's flash). Restoring it puts the machine back into that deep
//! sleep; it wakes like it would have.
//!
//! One taken at any other time (running, light sleep, halted) only holds what survives
//! pulling the battery: flash, the e-paper image, the modem's flash. Restoring it powers the
//! device on from there.
//!
//! File format: [`MAGIC`], the format version (u32 LE), then a zlib stream of the payload
//! written with [`StateWriter`]. Nested device state is length-prefixed ([`StateWriter::section`])
//! and every section must be read to its end, so format drift is caught instead of
//! misparsed.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const MAGIC: &[u8; 8] = b"TRMNLSAV";
/// Bump on any change to what is written.
pub const VERSION: u32 = 1;

/// The firmware build a save point belongs to. Its flash holds that build's app images
/// and HLE hooks are bound to that build's ELF, so it only restores onto the same build.
#[derive(Clone, Debug, PartialEq)]
pub struct FirmwareId {
    pub name: String,
    pub elf_sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SavedPower {
    /// Deep sleep until virtual time `wake_at` or a pin in `gpio_low_mask` reads low.
    DeepSleep { wake_at: Option<u64>, gpio_low_mask: u64 },
    /// Not in deep sleep when saved: only non-volatile state; restored by a power-on.
    Off,
}

/// SoC state kept through deep sleep (RTC fields empty for [`SavedPower::Off`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SocState {
    /// eFuse MAC: the device's identity on the server.
    pub mac: [u8; 6],
    pub now_ns: u64,
    pub boots: u32,
    pub rtc_ticks_base: u64,
    /// The RTC_CNTL register block.
    pub rtc_regs: Vec<u32>,
    /// RTC memories (C3: one; S3: slow, fast).
    pub rtc_mem: Vec<Vec<u8>>,
    /// Cache MMU entries (S3; empty on the C3, whose MMU is reset on wake).
    pub mmu: Vec<u32>,
    pub flash: Vec<u8>,
}

pub struct SavePoint {
    pub firmware: FirmwareId,
    /// `BoardInfo::name`, e.g. "TRMNL OG".
    pub board: String,
    pub label: String,
    /// Wall-clock creation time, Unix seconds.
    pub created: u64,
    pub power: SavedPower,
    pub soc: SocState,
    /// `Board::save_state` output.
    pub board_state: Vec<u8>,
    /// Front-end settings at the time (also restored).
    pub battery_mv: u32,
    pub docked: bool,
    pub wifi_available: bool,
}

impl SavePoint {
    pub fn deep_sleep(&self) -> bool {
        matches!(self.power, SavedPower::DeepSleep { .. })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = StateWriter::new();
        w.str(&self.firmware.name);
        w.bytes(&self.firmware.elf_sha256);
        w.str(&self.board);
        w.str(&self.label);
        w.u64(self.created);
        match self.power {
            SavedPower::DeepSleep { wake_at, gpio_low_mask } => {
                w.u8(1);
                w.opt_u64(wake_at);
                w.u64(gpio_low_mask);
            }
            SavedPower::Off => w.u8(0),
        }
        let s = &self.soc;
        w.bytes(&s.mac);
        w.u64(s.now_ns);
        w.u32(s.boots);
        w.u64(s.rtc_ticks_base);
        w.u32s(&s.rtc_regs);
        w.u32(s.rtc_mem.len() as u32);
        for m in &s.rtc_mem {
            w.bytes(m);
        }
        w.u32s(&s.mmu);
        w.bytes(&s.flash);
        w.bytes(&self.board_state);
        w.u32(self.battery_mv);
        w.bool(self.docked);
        w.bool(self.wifi_available);

        let mut out = MAGIC.to_vec();
        out.extend(VERSION.to_le_bytes());
        let mut z = flate2::write::ZlibEncoder::new(out, flate2::Compression::fast());
        z.write_all(&w.into_bytes()).expect("in-memory write");
        z.finish().expect("in-memory write")
    }

    pub fn decode(data: &[u8]) -> Result<SavePoint> {
        if data.len() < 12 || &data[..8] != MAGIC {
            bail!("not a trmnl-sim save point");
        }
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != VERSION {
            bail!("save point format version {version} is not supported (this simulator reads version {VERSION})");
        }
        let mut payload = Vec::new();
        flate2::read::ZlibDecoder::new(&data[12..])
            .read_to_end(&mut payload)
            .context("save point is corrupt (bad compressed data)")?;
        let mut r = StateReader::new(&payload);
        let firmware = FirmwareId { name: r.str()?, elf_sha256: r.array()? };
        let board = r.str()?;
        let label = r.str()?;
        let created = r.u64()?;
        let power = match r.u8()? {
            1 => SavedPower::DeepSleep { wake_at: r.opt_u64()?, gpio_low_mask: r.u64()? },
            _ => SavedPower::Off,
        };
        let mac = r.array()?;
        let now_ns = r.u64()?;
        let boots = r.u32()?;
        let rtc_ticks_base = r.u64()?;
        let rtc_regs = r.u32s()?;
        let n = r.u32()?;
        let rtc_mem = (0..n).map(|_| r.bytes().map(<[u8]>::to_vec)).collect::<Result<_>>()?;
        let mmu = r.u32s()?;
        let flash = r.bytes()?.to_vec();
        let soc = SocState { mac, now_ns, boots, rtc_ticks_base, rtc_regs, rtc_mem, mmu, flash };
        let board_state = r.bytes()?.to_vec();
        let sp = SavePoint {
            firmware,
            board,
            label,
            created,
            power,
            soc,
            board_state,
            battery_mv: r.u32()?,
            docked: r.bool()?,
            wifi_available: r.bool()?,
        };
        r.finish()?;
        Ok(sp)
    }

    pub fn load(path: &Path) -> Result<SavePoint> {
        let data = std::fs::read(path).with_context(|| format!("reading save point {}", path.display()))?;
        Self::decode(&data).with_context(|| format!("loading save point {}", path.display()))
    }

    /// Refuse to restore onto a different firmware build or board.
    pub fn check_compatible(&self, fw: &FirmwareId, board: &str) -> Result<()> {
        if self.firmware.elf_sha256 != fw.elf_sha256 {
            bail!(
                "save point was taken with firmware {} (ELF sha256 {}), but this simulator runs {} ({}); \
                 restore it with the build it was saved from",
                self.firmware.name,
                crate::firmware::hex(&self.firmware.elf_sha256[..8]),
                fw.name,
                crate::firmware::hex(&fw.elf_sha256[..8]),
            );
        }
        if self.board != board {
            bail!("save point is of a {}, this simulator runs a {board}", self.board);
        }
        Ok(())
    }
}

/// Little-endian binary writer for save point state.
#[derive(Default)]
pub struct StateWriter {
    buf: Vec<u8>,
}

impl StateWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend(v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend(v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend(v.to_le_bytes());
    }

    pub fn opt_u64(&mut self, v: Option<u64>) {
        self.bool(v.is_some());
        self.u64(v.unwrap_or(0));
    }

    /// Length-prefixed bytes.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    pub fn opt_bytes(&mut self, v: Option<&[u8]>) {
        self.bool(v.is_some());
        self.bytes(v.unwrap_or_default());
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub fn u16s(&mut self, v: &[u16]) {
        self.u32(v.len() as u32);
        v.iter().for_each(|x| self.u16(*x));
    }

    pub fn u32s(&mut self, v: &[u32]) {
        self.u32(v.len() as u32);
        v.iter().for_each(|x| self.u32(*x));
    }

    pub fn f32s(&mut self, v: &[f32]) {
        self.u32(v.len() as u32);
        self.buf.reserve(v.len() * 4);
        v.iter().for_each(|x| self.buf.extend(x.to_le_bytes()));
    }

    /// A length-prefixed nested section (read back with [`StateReader::section`]).
    pub fn section(&mut self, f: impl FnOnce(&mut StateWriter)) {
        let mut w = StateWriter::new();
        f(&mut w);
        self.bytes(&w.buf);
    }
}

/// Reads what [`StateWriter`] wrote, failing cleanly on truncated or mismatched data.
pub struct StateReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> StateReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        StateReader { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.data.len());
        let Some(end) = end else { bail!("save point is truncated or corrupt") };
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let b = self.bytes()?;
        b.try_into().map_err(|_| anyhow::anyhow!("save point field has {} bytes, expected {N}", b.len()))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        let some = self.bool()?;
        let v = self.u64()?;
        Ok(some.then_some(v))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub fn opt_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let some = self.bool()?;
        let b = self.bytes()?;
        Ok(some.then_some(b))
    }

    pub fn str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    pub fn u16s(&mut self) -> Result<Vec<u16>> {
        let n = self.u32()? as usize;
        Ok(self.take(n * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }

    pub fn u32s(&mut self) -> Result<Vec<u32>> {
        let n = self.u32()? as usize;
        Ok(self.take(n * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
    }

    pub fn f32s(&mut self) -> Result<Vec<f32>> {
        let n = self.u32()? as usize;
        Ok(self.take(n * 4)?.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
    }

    /// Read a fixed-size slice into `out`, checking the saved length matches.
    pub fn fill_u8(&mut self, out: &mut [u8]) -> Result<()> {
        let b = self.bytes()?;
        if b.len() != out.len() {
            bail!("save point field has {} bytes, expected {}", b.len(), out.len());
        }
        out.copy_from_slice(b);
        Ok(())
    }

    /// A section written with [`StateWriter::section`]; `f` must consume all of it.
    pub fn section<T>(&mut self, f: impl FnOnce(&mut StateReader<'a>) -> Result<T>) -> Result<T> {
        let mut r = StateReader::new(self.bytes()?);
        let v = f(&mut r)?;
        r.finish()?;
        Ok(v)
    }

    /// Everything was read (catches format drift between writer and reader).
    pub fn finish(&self) -> Result<()> {
        if self.pos != self.data.len() {
            bail!("save point has {} unexpected trailing bytes (format mismatch)", self.data.len() - self.pos);
        }
        Ok(())
    }
}

/// `f32s` read back into a buffer of known size.
pub fn read_f32s_into(r: &mut StateReader, out: &mut [f32]) -> Result<()> {
    let v = r.f32s()?;
    if v.len() != out.len() {
        bail!("save point has {} values, expected {}", v.len(), out.len());
    }
    out.copy_from_slice(&v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SavePoint {
        let mut flash = vec![0xff; 1 << 16];
        flash[0x1000..0x1010].copy_from_slice(b"nvs: wifi creds!");
        let mut w = StateWriter::new();
        w.section(|w| {
            w.u16s(&[1, 2, 0xffff]);
            w.f32s(&[0.0, 0.5, 1.0]);
            w.opt_bytes(Some(b"modem"));
            w.opt_bytes(None);
        });
        SavePoint {
            firmware: FirmwareId { name: "trmnl 1.2.3".into(), elf_sha256: [7; 32] },
            board: "TRMNL OG".into(),
            label: "onboarded".into(),
            created: 1_700_000_000,
            power: SavedPower::DeepSleep { wake_at: Some(123_456_789_000), gpio_low_mask: 1 << 2 },
            soc: SocState {
                mac: [0x7c, 0xdf, 0xa1, 0, 0, 1],
                now_ns: 42_000_000_000,
                boots: 3,
                rtc_ticks_base: 99,
                rtc_regs: (0..512).collect(),
                rtc_mem: vec![vec![0xa5; 8192]],
                mmu: vec![],
                flash,
            },
            board_state: w.into_bytes(),
            battery_mv: 3700,
            docked: true,
            wifi_available: false,
        }
    }

    #[test]
    fn round_trip() {
        let sp = sample();
        let data = sp.encode();
        assert!(data.len() < 4096, "compressed to {} bytes", data.len());
        let back = SavePoint::decode(&data).unwrap();
        assert_eq!(back.firmware, sp.firmware);
        assert_eq!(back.board, sp.board);
        assert_eq!(back.label, sp.label);
        assert_eq!(back.created, sp.created);
        assert_eq!(back.power, sp.power);
        assert_eq!(back.soc, sp.soc);
        assert_eq!(back.board_state, sp.board_state);
        assert_eq!((back.battery_mv, back.docked, back.wifi_available), (3700, true, false));

        let mut r = StateReader::new(&back.board_state);
        r.section(|r| {
            assert_eq!(r.u16s()?, vec![1, 2, 0xffff]);
            assert_eq!(r.f32s()?, vec![0.0, 0.5, 1.0]);
            assert_eq!(r.opt_bytes()?, Some(&b"modem"[..]));
            assert_eq!(r.opt_bytes()?, None);
            Ok(())
        })
        .unwrap();
        r.finish().unwrap();
    }

    #[test]
    fn power_off_round_trip() {
        let mut sp = sample();
        sp.power = SavedPower::Off;
        sp.soc.rtc_regs.clear();
        sp.soc.rtc_mem.clear();
        let back = SavePoint::decode(&sp.encode()).unwrap();
        assert_eq!(back.power, SavedPower::Off);
        assert!(!back.deep_sleep());
        assert_eq!(back.soc, sp.soc);
    }

    #[test]
    fn rejects_bad_files() {
        assert!(SavePoint::decode(b"hello").is_err());
        let mut data = sample().encode();
        data[8] = 99;
        let e = SavePoint::decode(&data).err().unwrap().to_string();
        assert!(e.contains("version 99"), "{e}");
        let mut data = sample().encode();
        let n = data.len();
        data.truncate(n - 10);
        assert!(SavePoint::decode(&data).is_err());
    }

    #[test]
    fn sections_must_be_read_fully() {
        let mut w = StateWriter::new();
        w.section(|w| {
            w.u32(1);
            w.u32(2);
        });
        let data = w.into_bytes();
        let mut r = StateReader::new(&data);
        assert!(r.section(|r| r.u32()).is_err());
        let mut r = StateReader::new(&data);
        assert!(r.section(|r| Ok((r.u32()?, r.u32()?, r.u32()?))).is_err());
    }

    /// Board state restored into a fresh board saves back to the same bytes (catches
    /// writer/reader mismatches in every device).
    fn board_round_trip(
        mut make: impl FnMut() -> Box<dyn crate::board::Board>,
        prepare: impl Fn(&mut dyn crate::board::Board),
    ) {
        for powered in [true, false] {
            let mut a = make();
            prepare(a.as_mut());
            let mut w = StateWriter::new();
            a.save_state(&mut w, powered);
            let saved = w.into_bytes();
            let mut b = make();
            let mut r = StateReader::new(&saved);
            b.restore_state(&mut r, powered).unwrap();
            r.finish().unwrap();
            let mut w = StateWriter::new();
            b.save_state(&mut w, powered);
            assert_eq!(w.into_bytes(), saved, "powered={powered}");
            assert_eq!(a.charging(), b.charging());
        }
    }

    #[test]
    fn og_and_bwry_boards_round_trip() {
        use crate::board::trmnl_og::TrmnlOg;
        use crate::devices::uc8179::Uc8179;
        board_round_trip(|| Box::new(TrmnlOg::new(Uc8179::new(0x1234))), |b| b.set_battery_mv(3456));
        board_round_trip(|| Box::new(TrmnlOg::new(Uc8179::new_bwry(0x1234))), |b| b.set_battery_mv(3900));
    }

    #[test]
    fn x_board_round_trips() {
        use crate::board::trmnl_x::TrmnlX;
        board_round_trip(
            || Box::new(TrmnlX::new([2, 0, 0, 0, 0, 9], &vnet::NetConfig { offline: true, ..Default::default() })),
            |b| {
                b.set_docked(true);
                b.set_battery_mv(3800);
                b.update(5_000_000);
            },
        );
    }

    #[test]
    fn refuses_other_firmware_and_boards() {
        let sp = sample();
        assert!(sp.check_compatible(&sp.firmware, "TRMNL OG").is_ok());
        let other = FirmwareId { name: "trmnl 1.2.4".into(), elf_sha256: [8; 32] };
        let e = sp.check_compatible(&other, "TRMNL OG").unwrap_err().to_string();
        assert!(e.contains("trmnl 1.2.3") && e.contains("trmnl 1.2.4"), "{e}");
        assert!(sp.check_compatible(&sp.firmware, "TRMNL X").is_err());
    }
}
