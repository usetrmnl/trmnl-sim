//! What the modem's flash contents mean: whether the chip boots into ESP-AT from them, and
//! which TRMNL modem firmware version they are.
//!
//! The image the firmware writes (`data/system/factory_ESP32C5-4MB.bin` in trmnl-firmware) is
//! a whole 4 MB flash: second-stage bootloader at 0x2000 (the C5's offset), a partition table
//! (at 0xC000 in ESP-AT's layout) and the application in the first app partition.

/// `esp_image_header_t.magic`.
const IMAGE_MAGIC: u8 = 0xe9;
const BOOTLOADER_OFFSET: usize = 0x2000;
/// `esp_partition_info_t.magic` (little endian 0x50AA), one 32-byte entry each.
const PARTITION_MAGIC: [u8; 2] = [0xaa, 0x50];
const PARTITION_TYPE_APP: u8 = 0;
/// The custom command's response format in the TRMNL build of ESP-AT. The version it prints
/// is the string constant the linker placed right before it.
const FWVER_FORMAT: &[u8] = b"+FWVER:\"%s\"";

fn word(d: &[u8], at: usize) -> Option<usize> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
}

/// An ESP image at `at` whose segments are all there: the header's magic, then each
/// segment's bytes XORed into the checksum byte that ends the image (what the ROM checks for
/// the bootloader and the bootloader for the application; the SHA-256 after it is skipped).
fn image_valid(flash: &[u8], at: usize) -> bool {
    let (Some(&IMAGE_MAGIC), Some(&segments)) = (flash.get(at), flash.get(at + 1)) else { return false };
    if segments == 0 || segments > 16 {
        return false;
    }
    // esp_image_header_t is 24 bytes; each segment is {load address, length} + data.
    let mut pos = at + 24;
    let mut sum = 0xefu8;
    for _ in 0..segments {
        let Some(len) = word(flash, pos + 4) else { return false };
        let Some(data) = flash.get(pos + 8..(pos + 8).saturating_add(len)) else { return false };
        sum = data.iter().fold(sum, |a, &b| a ^ b);
        pos += 8 + len;
    }
    // The checksum is the last byte of the 16-byte padded image.
    let end = (pos - at + 16) & !15;
    flash.get(at + end - 1) == Some(&sum)
}

/// Offset of the first app partition, from the partition table (searched for where the
/// bootloader's `CONFIG_PARTITION_TABLE_OFFSET` may put it: a 4 KiB boundary below 64 KiB).
fn app_offset(flash: &[u8]) -> Option<usize> {
    let table = (0x8000..0x10000).step_by(0x1000).find(|&o| flash.get(o..o + 2) == Some(&PARTITION_MAGIC[..]))?;
    (table..table + 0xc00)
        .step_by(32)
        .take_while(|&e| flash.get(e..e + 2) == Some(&PARTITION_MAGIC[..]))
        .find(|&e| flash[e + 2] == PARTITION_TYPE_APP)
        .and_then(|e| word(flash, e + 4))
}

/// Whether a chip with this flash gets as far as ESP-AT's `ready`: an erased or partly
/// written flash (a flash operation that failed half way) leaves the ROM or the bootloader
/// printing errors in a reset loop instead.
pub(super) fn bootable(flash: &[u8]) -> bool {
    image_valid(flash, BOOTLOADER_OFFSET) && app_offset(flash).is_some_and(|app| image_valid(flash, app))
}

/// The version `AT+FWVER?` reports, None for an image without the command (the firmware the
/// first TRMNL X units shipped with, or anything that is not a TRMNL modem image).
pub(super) fn fw_version(flash: &[u8]) -> Option<String> {
    let format = flash.windows(FWVER_FORMAT.len()).position(|w| w == FWVER_FORMAT)?;
    let end = flash[..format].iter().rposition(|&b| b != 0)? + 1;
    let start =
        flash[..end].iter().rposition(|&b| !(b.is_ascii_alphanumeric() || b"._-+".contains(&b))).map_or(0, |p| p + 1);
    let version = std::str::from_utf8(&flash[start..end]).ok()?;
    version.starts_with(|c: char| c.is_ascii_digit()).then(|| version.to_string())
}

/// A minimal flash that boots and reports `version` (None: no `AT+FWVER`), for tests.
#[cfg(test)]
pub(super) fn test_image(version: Option<&str>) -> Vec<u8> {
    fn image(payload: &[u8]) -> Vec<u8> {
        let mut img = vec![IMAGE_MAGIC, 1];
        img.resize(24, 0);
        img.extend_from_slice(&0x4200_0020u32.to_le_bytes());
        img.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        img.extend_from_slice(payload);
        let end = (img.len() + 16) & !15;
        img.resize(end, 0);
        img[end - 1] = payload.iter().fold(0xef, |a, &b| a ^ b);
        img
    }
    const APP: usize = 0x1_0000;
    let mut flash = vec![0xff; 4 << 20];
    let mut put = |at: usize, d: &[u8]| flash[at..at + d.len()].copy_from_slice(d);
    put(BOOTLOADER_OFFSET, &image(b"second stage bootloader"));
    let mut entry = vec![PARTITION_MAGIC[0], PARTITION_MAGIC[1], PARTITION_TYPE_APP, 0x10];
    entry.extend_from_slice(&(APP as u32).to_le_bytes());
    entry.extend_from_slice(&0x10_0000u32.to_le_bytes());
    entry.resize(32, 0);
    put(0xc000, &entry);
    let mut rodata = b"ESP32C5-SPI\0".to_vec();
    if let Some(v) = version {
        rodata.extend_from_slice(v.as_bytes());
        rodata.extend_from_slice(b"\0\0\0+FWVER:\"%s\"\r\n\0");
    }
    put(APP, &image(&rodata));
    flash
}
