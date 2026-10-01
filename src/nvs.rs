//! ESP-IDF NVS inspection and host-side editing. Layout follows nvs_types.hpp and nvs_page.hpp;
//! values come from live flash, never from guest API calls or the backing file.
use sim_api::{Preference, PreferencesSnapshot};

mod write;
use std::collections::BTreeMap;
pub use write::prepare;

const PAGE: usize = 4096;
const ENTRY: usize = 32;
const COUNT: usize = 126;

fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

// ESP ROM crc32_le with initial value 0xffffffff (zlib's seeded CRC convention).
fn checksum(data: impl IntoIterator<Item = u8>) -> u32 {
    let mut crc = 0u32;
    for b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn written(page: &[u8], index: usize) -> bool {
    (page[32 + index / 4] >> ((index % 4) * 2)) & 3 == 2
}

#[derive(Clone)]
struct Item {
    ty: u8,
    data: Vec<u8>,
}

pub fn inspect(flash: &[u8]) -> PreferencesSnapshot {
    let mut result = PreferencesSnapshot::default();
    let parts = crate::firmware::partitions(flash);
    let mut found = false;
    for part in parts.iter().filter(|p| p.kind == 1 && p.subtype == 2) {
        found = true;
        let start = part.offset as usize;
        let Some(end) = start.checked_add(part.size as usize) else { continue };
        let Some(data) = flash.get(start..end) else {
            result.warnings.push(format!("{}: partition extends beyond flash", part.label));
            continue;
        };
        decode_partition(&part.label, data, &mut result);
    }
    if !found {
        result.warnings.push("No NVS partition found in the firmware partition table.".into());
    }
    result.entries.sort_by(|a, b| (&a.partition, &a.namespace, &a.key).cmp(&(&b.partition, &b.namespace, &b.key)));
    result
}

struct Decoded {
    namespaces: BTreeMap<u8, String>,
    values: BTreeMap<(u8, String), Item>,
}

fn decode_partition(label: &str, data: &[u8], result: &mut PreferencesSnapshot) -> Decoded {
    let mut pages = Vec::new();
    let mut invalid = 0;
    for page in data.as_chunks::<PAGE>().0 {
        let state = u32le(page);
        if state == u32::MAX {
            continue;
        }
        if !matches!(state, 0xfffffffe | 0xfffffffc | 0xfffffff8)
            || !matches!(page[8], 0xff | 0xfe)
            || checksum(page[4..28].iter().copied()) != u32le(&page[28..32])
        {
            invalid += 1;
            continue;
        }
        pages.push(page);
    }
    if !data.len().is_multiple_of(PAGE) {
        result.warnings.push(format!("{label}: truncated NVS page"));
    }
    // Sequence numbers, unlike physical sectors, reflect write order (including wrap).
    pages.sort_by(|a, b| (u32le(&a[4..]).wrapping_sub(u32le(&b[4..])) as i32).cmp(&0));
    let mut namespaces = BTreeMap::new();
    let mut values = BTreeMap::new();
    let mut chunks = BTreeMap::new();
    for page in pages {
        let mut slot = 0;
        while slot < COUNT {
            if !written(page, slot) {
                slot += 1;
                continue;
            }
            let offset = 64 + slot * ENTRY;
            let e = &page[offset..offset + ENTRY];
            let span = usize::from(e[2]);
            if checksum(e[..4].iter().chain(&e[8..]).copied()) != u32le(&e[4..]) || span == 0 || span > COUNT - slot {
                invalid += 1;
                slot += 1;
                continue;
            }
            let original = slot;
            slot += span;
            if !(original..slot).all(|i| written(page, i)) {
                invalid += 1;
                continue;
            }
            let Some(zero) = e[8..24].iter().position(|b| *b == 0) else {
                invalid += 1;
                continue;
            };
            let Ok(key) = std::str::from_utf8(&e[8..8 + zero]) else {
                invalid += 1;
                continue;
            };
            if key.is_empty() {
                invalid += 1;
                continue;
            }
            let ty = e[1];
            let value = if matches!(ty, 0x21 | 0x41 | 0x42) {
                let len = usize::from(u16::from_le_bytes(e[24..26].try_into().unwrap()));
                if len > (span - 1) * ENTRY {
                    invalid += 1;
                    continue;
                }
                let bytes = &page[offset + ENTRY..offset + ENTRY + len];
                if checksum(bytes.iter().copied()) != u32le(&e[28..]) {
                    invalid += 1;
                    continue;
                }
                bytes.to_vec()
            } else {
                if span != 1 {
                    invalid += 1;
                    continue;
                }
                e[24..32].to_vec()
            };
            if e[0] == 0 {
                if ty == 1 && value[0] != 0 && value[0] != 255 {
                    namespaces.insert(value[0], key.to_owned());
                } else {
                    invalid += 1;
                }
            } else if ty == 0x42 {
                chunks.insert((e[0], key.to_owned(), e[3]), value);
            } else {
                values.insert((e[0], key.to_owned()), Item { ty, data: value });
            }
        }
    }
    let mut committed = BTreeMap::new();
    for ((ns, key), mut item) in values {
        let Some(namespace) = namespaces.get(&ns) else {
            invalid += 1;
            continue;
        };
        let decoded = if item.ty == 0x48 {
            let size = u32le(&item.data) as usize;
            let count = item.data[4];
            let start = item.data[5];
            let mut bytes = Vec::new();
            let mut complete = matches!(start, 0 | 0x80) && count <= 0x7f;
            if complete {
                for index in start..start + count {
                    match chunks.get(&(ns, key.clone(), index)) {
                        Some(chunk) => bytes.extend_from_slice(chunk),
                        None => {
                            complete = false;
                            break;
                        }
                    }
                }
            }
            if complete && bytes.len() == size {
                item = Item { ty: 0x41, data: bytes };
                Some(("blob", blob(&item.data)))
            } else {
                None
            }
        } else {
            format_value(item.ty, &item.data)
        };
        if let Some((kind, value)) = decoded {
            committed.insert((ns, key.clone()), item);
            result.entries.push(Preference { partition: label.into(), namespace: namespace.clone(), key, kind, value });
        } else {
            invalid += 1;
        }
    }
    if invalid != 0 {
        result.warnings.push(format!("{label}: skipped {invalid} unreadable pages or entries (incomplete, corrupt, encrypted, or unsupported data)."));
    }
    Decoded { namespaces, values: committed }
}

fn blob(bytes: &[u8]) -> String {
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
    format!("{hex} ({} bytes)", bytes.len())
}

fn format_value(ty: u8, data: &[u8]) -> Option<(&'static str, String)> {
    let value = match ty {
        1 => ("u8", data[0].to_string()),
        2 => ("u16", u16::from_le_bytes(data[..2].try_into().ok()?).to_string()),
        4 => ("u32", u32le(data).to_string()),
        8 => ("u64", u64::from_le_bytes(data.try_into().ok()?).to_string()),
        0x11 => ("i8", (data[0] as i8).to_string()),
        0x12 => ("i16", i16::from_le_bytes(data[..2].try_into().ok()?).to_string()),
        0x14 => ("i32", i32::from_le_bytes(data[..4].try_into().ok()?).to_string()),
        0x18 => ("i64", i64::from_le_bytes(data.try_into().ok()?).to_string()),
        0x21 => ("string", std::str::from_utf8(data.strip_suffix(&[0])?).ok()?.to_owned()),
        0x41 => ("blob", blob(data)),
        _ => return None,
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc(data: &[u8]) -> u32 {
        let mut crc = 0u32;
        for b in data {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    fn page(sequence: u32) -> Vec<u8> {
        let mut p = vec![255; 4096];
        p[..4].copy_from_slice(&0xfffffffeu32.to_le_bytes());
        p[4..8].copy_from_slice(&sequence.to_le_bytes());
        p[8] = 0xfe;
        let checksum = crc(&p[4..28]);
        p[28..32].copy_from_slice(&checksum.to_le_bytes());
        p
    }

    fn item(p: &mut [u8], slot: usize, ns: u8, ty: u8, key: &str, data: &[u8], chunk: u8) {
        let variable = matches!(ty, 0x21 | 0x41 | 0x42);
        let span = 1 + if variable { data.len().div_ceil(32) } else { 0 };
        let mut e = [255; 32];
        e[..4].copy_from_slice(&[ns, ty, span as u8, chunk]);
        e[8..8 + key.len()].copy_from_slice(key.as_bytes());
        e[8 + key.len()] = 0;
        if variable {
            e[24..26].copy_from_slice(&(data.len() as u16).to_le_bytes());
            e[28..32].copy_from_slice(&crc(data).to_le_bytes());
        } else {
            e[24..24 + data.len()].copy_from_slice(data);
        }
        let bytes: Vec<_> = e[..4].iter().chain(&e[8..]).copied().collect();
        e[4..8].copy_from_slice(&crc(&bytes).to_le_bytes());
        let start = 64 + slot * 32;
        p[start..start + 32].copy_from_slice(&e);
        if variable {
            p[start + 32..start + 32 + data.len()].copy_from_slice(data);
        }
        for i in slot..slot + span {
            p[32 + i / 4] &= !(1 << ((i % 4) * 2));
        }
    }

    pub(super) fn flash(pages: &[Vec<u8>]) -> Vec<u8> {
        let mut f = vec![255; 0x9000];
        f[0x8000..0x8004].copy_from_slice(&[0xaa, 0x50, 1, 2]);
        f[0x8004..0x8008].copy_from_slice(&0x9000u32.to_le_bytes());
        f[0x8008..0x800c].copy_from_slice(&((pages.len() * 4096) as u32).to_le_bytes());
        f[0x800c..0x8020].fill(0);
        f[0x800c..0x800f].copy_from_slice(b"nvs");
        for p in pages {
            f.extend(p);
        }
        f
    }

    #[test]
    fn reads_esp_idf_generated_partition() {
        // Generated by ESP-IDF 4.4.7 nvs_partition_gen.py from tests/nvs/preferences.csv.
        let pages: Vec<_> = include_bytes!("../tests/nvs/preferences.bin").chunks(PAGE).map(|p| p.to_vec()).collect();
        let snapshot = inspect(&flash(&pages));
        assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
        let values: Vec<_> = snapshot.entries.iter().map(|e| (e.key.as_str(), e.value.as_str())).collect();
        assert_eq!(
            values,
            [
                ("blob", "00 aa ff (3 bytes)"),
                ("count", "4294967295"),
                ("password", "visible-secret"),
                ("signed", "-9223372036854775808")
            ]
        );
    }

    #[test]
    #[ignore = "requires a saved firmware flash image"]
    fn inspect_saved_firmware_flash() {
        let path = std::env::var("SIM_NVS_FLASH").expect("SIM_NVS_FLASH");
        let snapshot = inspect(&std::fs::read(path).unwrap());
        assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
        assert!(!snapshot.entries.is_empty());
        eprintln!("Decoded {} saved preferences", snapshot.entries.len());
    }

    #[test]
    fn crc_matches_esp_idf_seeded_zlib() {
        assert_eq!(checksum(b"123456789".iter().copied()), 0xd202d277);
    }

    #[test]
    fn reads_namespaces_scalars_strings_and_latest_written_value() {
        let mut old = page(3);
        item(&mut old, 0, 0, 1, "settings", &[1], 255);
        item(&mut old, 1, 1, 4, "count", &1u32.to_le_bytes(), 255);
        item(&mut old, 2, 1, 1, "deleted", &[7], 255);
        old[32] &= !(3 << 4);
        let mut new = page(4);
        item(&mut new, 0, 1, 4, "count", &42u32.to_le_bytes(), 255);
        item(&mut new, 1, 1, 0x21, "password", b"visible-secret\0", 255);
        item(&mut new, 3, 1, 0x12, "signed", &(-123i16).to_le_bytes(), 255);
        let f = flash(&[new, old]); // Flash order differs from NVS page sequence.
        let before = f.clone();
        let s = inspect(&f);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.entries.len(), 3);
        let values: Vec<_> =
            s.entries.iter().map(|e| (e.namespace.as_str(), e.key.as_str(), e.value.as_str())).collect();
        assert_eq!(
            values,
            [("settings", "count", "42"), ("settings", "password", "visible-secret"), ("settings", "signed", "-123")]
        );
        assert_eq!(f, before);
    }

    #[test]
    fn reconstructs_only_committed_blob_version_across_pages() {
        let mut a = page(0);
        item(&mut a, 0, 0, 1, "settings", &[1], 255);
        item(&mut a, 1, 1, 0x42, "blob", &[0xaa, 0xbb], 0x80);
        let mut b = page(1);
        item(&mut b, 0, 1, 0x42, "blob", &[0xcc], 0x81);
        item(&mut b, 2, 1, 0x48, "blob", &[3, 0, 0, 0, 2, 0x80], 255);
        item(&mut b, 3, 1, 0x42, "blob", &[0xdd], 0); // Interrupted replacement.
        let s = inspect(&flash(&[a, b]));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].value, "aa bb cc (3 bytes)");
    }

    #[test]
    fn rejects_corrupt_entries_and_out_of_bounds_partitions() {
        let mut p = page(0);
        item(&mut p, 0, 0, 1, "settings", &[1], 255);
        item(&mut p, 1, 1, 0x21, "bad", b"broken\0", 255);
        p[64 + 2 * 32] ^= 1;
        let mut f = flash(&[p]);
        let s = inspect(&f);
        assert!(s.entries.is_empty());
        assert!(!s.warnings.is_empty());
        f.truncate(0x9100);
        assert!(!inspect(&f).warnings.is_empty());
        assert!(inspect(&[]).entries.is_empty());
    }
}
