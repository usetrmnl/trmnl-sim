use super::{COUNT, ENTRY, Item, PAGE, checksum, decode_partition};
use sim_api::{PreferenceChange, PreferencesSnapshot};

/// Prepare an edited flash image without changing the source. Repack only the selected
/// partition, retaining namespace IDs and all committed values. The guest must be asleep.
pub fn prepare(flash: &[u8], change: &PreferenceChange) -> Result<Vec<u8>, String> {
    name(&change.namespace)?;
    name(&change.key)?;
    let partitions = crate::firmware::partitions(flash);
    let matches: Vec<_> =
        partitions.iter().filter(|p| p.label == change.partition && p.kind == 1 && p.subtype == 2).collect();
    if matches.len() != 1 {
        return Err("need a unique NVS partition name".into());
    }
    let part = matches[0];
    // An encrypted but currently empty partition also must not receive plaintext.
    if flash[0x8000..0x8c00]
        .chunks(32)
        .take_while(|e| e[..2] == [0xaa, 0x50])
        .any(|e| super::u32le(&e[4..]) == part.offset && super::u32le(&e[28..]) & 1 != 0)
    {
        return Err("encrypted NVS partitions cannot be edited".into());
    }
    let start = part.offset as usize;
    let end = start.checked_add(part.size as usize).ok_or("partition range overflow")?;
    if start < 0x9000
        || !start.is_multiple_of(PAGE)
        || partitions.iter().any(|other| {
            !std::ptr::eq(other, part)
                && (start as u64) < u64::from(other.offset) + u64::from(other.size)
                && (end as u64) > u64::from(other.offset)
        })
    {
        return Err("NVS partition is misaligned or overlaps other flash contents".into());
    }
    let data = flash.get(start..end).ok_or("partition extends beyond flash")?;
    if data.len() < 3 * PAGE || !data.len().is_multiple_of(PAGE) {
        return Err("editable NVS requires at least three whole pages".into());
    }
    let mut snapshot = PreferencesSnapshot::default();
    let mut decoded = decode_partition(&part.label, data, &mut snapshot);
    if !snapshot.warnings.is_empty() {
        return Err(format!("refusing to rewrite unreadable NVS: {}", snapshot.warnings.join(" ")));
    }
    let ids: Vec<_> =
        decoded.namespaces.iter().filter_map(|(id, name)| (name == &change.namespace).then_some(*id)).collect();
    if ids.len() > 1 {
        return Err("ambiguous namespace IDs".into());
    }
    let value = change.value.as_ref().map(|(kind, value)| parse_value(kind, value)).transpose()?;
    let ns = match ids.first() {
        Some(id) => *id,
        None if value.is_none() => return Err("preference does not exist".into()),
        None => {
            let id = (1..255).find(|id| !decoded.namespaces.contains_key(id)).ok_or("no namespace IDs available")?;
            decoded.namespaces.insert(id, change.namespace.clone());
            id
        }
    };
    let key = (ns, change.key.clone());
    match value {
        Some(value) => {
            decoded.values.insert(key, value);
        }
        None => {
            decoded.values.remove(&key).ok_or("preference does not exist")?;
        }
    }
    let mut writer = Writer::new(data.len());
    for (id, namespace) in decoded.namespaces {
        writer.entry(0, 1, &namespace, &[id], 255)?;
    }
    for ((ns, key), item) in decoded.values {
        if item.ty == 0x41 {
            // NVS v2 blob data chunks, followed by the committing index item.
            let mut count = 0u8;
            let mut remaining = item.data.as_slice();
            loop {
                if count == 127 {
                    return Err("blob needs too many chunks".into());
                }
                let mut room = (COUNT - writer.slot).saturating_sub(1) * ENTRY;
                if room == 0 && !remaining.is_empty() {
                    writer.next_page()?;
                    room = (COUNT - 1) * ENTRY;
                }
                let len = remaining.len().min(room);
                writer.entry(ns, 0x42, &key, &remaining[..len], count)?;
                count += 1;
                remaining = &remaining[len..];
                if remaining.is_empty() {
                    break;
                }
            }
            let mut index = [255; 8];
            index[..4].copy_from_slice(&(item.data.len() as u32).to_le_bytes());
            index[4..6].copy_from_slice(&[count, 0]);
            writer.entry(ns, 0x48, &key, &index, 255)?;
        } else {
            writer.entry(ns, item.ty, &key, &item.data, 255)?;
        }
    }
    let mut updated = flash.to_vec();
    updated[start..end].copy_from_slice(&writer.data);
    Ok(updated)
}

fn name(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 15 || value.contains('\0') {
        Err("namespace and key must contain 1–15 UTF-8 bytes without NUL".into())
    } else {
        Ok(())
    }
}

fn parse_value(kind: &str, value: &str) -> Result<Item, String> {
    let invalid = || format!("invalid {kind} value");
    let (ty, data) = match kind {
        "u8" => (1, value.parse::<u8>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "u16" => (2, value.parse::<u16>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "u32" => (4, value.parse::<u32>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "u64" => (8, value.parse::<u64>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "i8" => (0x11, value.parse::<i8>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "i16" => (0x12, value.parse::<i16>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "i32" => (0x14, value.parse::<i32>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "i64" => (0x18, value.parse::<i64>().map_err(|_| invalid())?.to_le_bytes().to_vec()),
        "string" => {
            if value.len() >= 4000 || value.contains('\0') {
                return Err("string must fit in 3999 bytes without NUL".into());
            }
            let mut data = value.as_bytes().to_vec();
            data.push(0);
            (0x21, data)
        }
        "blob" => {
            let hex: Vec<_> = value.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            if !hex.len().is_multiple_of(2) {
                return Err(invalid());
            }
            let data = hex
                .chunks(2)
                .map(|pair| {
                    let hi = (pair[0] as char).to_digit(16).ok_or_else(invalid)?;
                    let lo = (pair[1] as char).to_digit(16).ok_or_else(invalid)?;
                    Ok((hi * 16 + lo) as u8)
                })
                .collect::<Result<Vec<_>, String>>()?;
            (0x41, data)
        }
        _ => return Err("type must be u8/u16/u32/u64/i8/i16/i32/i64/string/blob".into()),
    };
    Ok(Item { ty, data })
}

struct Writer {
    data: Vec<u8>,
    page: usize,
    slot: usize,
}

impl Writer {
    fn new(size: usize) -> Self {
        let mut w = Self { data: vec![255; size], page: 0, slot: 0 };
        w.header();
        w
    }

    fn header(&mut self) {
        let p = &mut self.data[self.page * PAGE..][..PAGE];
        p[..4].copy_from_slice(&0xfffffffeu32.to_le_bytes());
        p[4..8].copy_from_slice(&(self.page as u32).to_le_bytes());
        p[8] = 0xfe; // v2
        let crc = checksum(p[4..28].iter().copied());
        p[28..32].copy_from_slice(&crc.to_le_bytes());
    }

    fn next_page(&mut self) -> Result<(), String> {
        self.data[self.page * PAGE..][..4].copy_from_slice(&0xfffffffcu32.to_le_bytes());
        self.page += 1;
        // Always keep one erased sector for ESP-IDF's garbage collection.
        if (self.page + 1) * PAGE >= self.data.len() {
            return Err("not enough NVS space".into());
        }
        self.slot = 0;
        self.header();
        Ok(())
    }

    fn entry(&mut self, ns: u8, ty: u8, key: &str, data: &[u8], chunk: u8) -> Result<(), String> {
        name(key)?;
        let variable = matches!(ty, 0x21 | 0x42);
        let span = 1 + if variable { data.len().div_ceil(ENTRY) } else { 0 };
        if span > COUNT {
            return Err("value does not fit an NVS page".into());
        }
        if self.slot + span > COUNT {
            self.next_page()?;
        }
        let p = &mut self.data[self.page * PAGE..][..PAGE];
        let start = 64 + self.slot * ENTRY;
        let e = &mut p[start..start + ENTRY];
        e[..4].copy_from_slice(&[ns, ty, span as u8, chunk]);
        e[8..24].fill(0);
        e[8..8 + key.len()].copy_from_slice(key.as_bytes());
        if variable {
            e[24..26].copy_from_slice(&(data.len() as u16).to_le_bytes());
            e[28..32].copy_from_slice(&checksum(data.iter().copied()).to_le_bytes());
        } else {
            e[24..24 + data.len()].copy_from_slice(data);
        }
        let crc = checksum(e[..4].iter().chain(&e[8..]).copied());
        e[4..8].copy_from_slice(&crc.to_le_bytes());
        if variable {
            p[start + ENTRY..start + ENTRY + data.len()].copy_from_slice(data);
        }
        for i in self.slot..self.slot + span {
            p[32 + i / 4] &= !(1 << ((i % 4) * 2));
        }
        self.slot += span;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvs::{PAGE, inspect, tests::flash};

    fn fixture() -> Vec<u8> {
        flash(&include_bytes!("../../tests/nvs/preferences.bin").chunks(PAGE).map(|p| p.to_vec()).collect::<Vec<_>>())
    }

    fn change(key: &str, value: Option<(&str, &str)>) -> PreferenceChange {
        PreferenceChange {
            partition: "nvs".into(),
            namespace: "settings".into(),
            key: key.into(),
            value: value.map(|(t, v)| (t.into(), v.into())),
        }
    }

    #[test]
    fn edit_create_delete_preserve_other_values_and_flash() {
        let original = fixture();
        let updated = prepare(&original, &change("password", Some(("string", "new-secret")))).unwrap();
        assert_eq!(&original[..0x9000], &updated[..0x9000]);
        let expected = inspect(&original).entries.into_iter().filter(|e| e.key != "password").collect::<Vec<_>>();
        let snapshot = inspect(&updated);
        assert!(snapshot.warnings.is_empty());
        assert_eq!(snapshot.entries.iter().find(|e| e.key == "password").unwrap().value, "new-secret");
        assert_eq!(snapshot.entries.into_iter().filter(|e| e.key != "password").collect::<Vec<_>>(), expected);
        let added = prepare(&updated, &change("enabled", Some(("u8", "1")))).unwrap();
        assert_eq!(inspect(&added).entries.len(), 5);
        let deleted = prepare(&added, &change("password", None)).unwrap();
        assert!(!inspect(&deleted).entries.iter().any(|e| e.key == "password"));
        assert!(prepare(&deleted, &change("password", None)).is_err());
    }

    #[test]
    fn new_namespace_empty_partition_and_multipage_blob() {
        let original = flash(&vec![vec![255; PAGE]; 3]);
        let value = "a5 ".repeat(5000);
        let updated = prepare(&original, &change("large", Some(("blob", &value)))).unwrap();
        let snapshot = inspect(&updated);
        assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
        assert_eq!(snapshot.entries[0].edit_value(), value.trim());
        assert_eq!(&updated[updated.len() - PAGE..], &[255; PAGE]);
    }

    #[test]
    fn typed_values_round_trip_and_repeated_edits_reclaim_space() {
        let mut image = fixture();
        for (kind, value) in [
            ("u8", "255"),
            ("u16", "65535"),
            ("u32", "4294967295"),
            ("u64", "18446744073709551615"),
            ("i8", "-128"),
            ("i16", "-32768"),
            ("i32", "-2147483648"),
            ("i64", "-9223372036854775808"),
            ("string", ""),
            ("blob", ""),
        ] {
            image = prepare(&image, &change("typed", Some((kind, value)))).unwrap();
            let snapshot = inspect(&image);
            assert!(snapshot.warnings.is_empty());
            let entry = snapshot.entries.iter().find(|e| e.key == "typed").unwrap();
            assert_eq!((entry.kind, entry.edit_value()), (kind, value));
        }
        for value in 0..150 {
            image = prepare(&image, &change("typed", Some(("u32", &value.to_string())))).unwrap();
        }
        assert_eq!(inspect(&image).entries.iter().find(|e| e.key == "typed").unwrap().value, "149");
    }

    #[test]
    fn rejects_invalid_values_corruption_and_full_partition() {
        let original = fixture();
        for (kind, value) in [
            ("u8", "256"),
            ("i8", "-129"),
            ("u32", "-1"),
            ("blob", "abc"),
            ("blob", "zz"),
            ("float", "1.5"),
            ("string", "a\0b"),
        ] {
            assert!(prepare(&original, &change("bad", Some((kind, value)))).is_err(), "{kind}: {value}");
        }
        assert!(prepare(&original, &change("way-too-long-key", Some(("u8", "1")))).is_err());
        assert!(prepare(&original, &change("large", Some(("blob", &"ff".repeat(12000))))).is_err());
        let mut corrupt = original.clone();
        corrupt[0x9000 + 28] ^= 1;
        assert!(prepare(&corrupt, &change("password", Some(("string", "x")))).is_err());
        let mut encrypted = original.clone();
        encrypted[0x801c] = 1;
        assert!(prepare(&encrypted, &change("password", Some(("string", "x")))).is_err());
        assert_eq!(original, fixture());
    }
}
