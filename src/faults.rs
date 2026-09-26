//! Fault injection glue: turns the front-end's [`sim_api::Faults`] into settings of the
//! parts that implement them (the `vnet` router, the flash chip, board devices). Chip
//! modules call [`apply`] from `Machine::set_faults`.

use std::time::Duration;

use sim_api::{DnsFault, Faults, PartitionInfo, PowerLoss};

use crate::board::Board;
use crate::devices::spi_flash::{PowerLossTrigger, SpiFlash};
use crate::firmware;
use crate::hle::wifi::WifiState;

/// Network faults in `vnet` terms (also used by the TRMNL X modem's HTTP client).
pub fn net_faults(f: &sim_api::NetFaults) -> vnet::NetFaults {
    vnet::NetFaults {
        latency: Duration::from_millis(f.latency_ms as u64),
        loss: f.loss.clamp(0.0, 1.0),
        bandwidth: f.bandwidth_bps,
        dns: f.dns.map(|d| match d {
            DnsFault::ServFail => vnet::DnsFault::ServFail,
            DnsFault::NxDomain => vnet::DnsFault::NxDomain,
            DnsFault::Empty => vnet::DnsFault::Empty,
            DnsFault::Timeout => vnet::DnsFault::Timeout,
        }),
        no_internet: f.no_internet,
        offline: f.offline,
        tcp_cut: f.tcp_cut.map(|c| vnet::TcpCut { after_bytes: c.after_bytes, stall: c.stall, port: c.port }),
    }
}

/// Resolve a power-loss spec against the flash's partition table.
pub fn power_loss_trigger(p: &PowerLoss, flash: &[u8]) -> Result<PowerLossTrigger, String> {
    let mut range = p.range;
    let mut what = Vec::new();
    if let Some(name) = &p.partition {
        let parts = firmware::partitions(flash);
        let part = PartitionInfo::find(&parts, name).ok_or_else(|| {
            let names: Vec<&str> = parts.iter().map(|p| p.label.as_str()).collect();
            format!("no partition named {name:?} (the table has {})", names.join(", "))
        })?;
        let (a, b) = (part.offset, part.offset + part.size);
        range = Some(match range {
            Some((s, e)) => (s.max(a), e.min(b)),
            None => (a, b),
        });
        what.push(format!("partition {}", part.label));
    }
    if let Some((a, b)) = p.range {
        what.push(format!("{a:#x}..{b:#x}"));
    }
    Ok(PowerLossTrigger { op: p.op, range, nth: p.nth.max(1), cut: p.cut, what: what.join(" ") })
}

/// Apply `f` to a chip's flash, WiFi model and board. On an error (e.g. an unknown
/// partition name) the other faults are still applied.
pub fn apply(f: &Faults, flash: &mut SpiFlash, wifi: &mut WifiState, board: &mut dyn Board) -> Result<(), String> {
    wifi.set_net_faults(net_faults(&f.net));
    board.set_faults(f);
    match &f.power_loss {
        None => {
            flash.set_power_loss(None);
            Ok(())
        }
        Some(p) => {
            let t = power_loss_trigger(p, &flash.data);
            flash.set_power_loss(t.as_ref().ok().cloned());
            t.map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(label: &str, kind: u8, subtype: u8, offset: u32, size: u32) -> PartitionInfo {
        PartitionInfo { label: label.into(), kind, subtype, offset, size }
    }

    #[test]
    fn partition_aliases() {
        let parts = [
            part("nvs", 1, 2, 0x9000, 0x5000),
            part("app0", 0, 0x10, 0x10000, 0x1000),
            part("app1", 0, 0x11, 0x20000, 0x1000),
            part("spiffs", 1, 0x82, 0x30000, 0x1000),
        ];
        let find = |n| PartitionInfo::find(&parts, n).map(|p| p.label.as_str());
        assert_eq!(find("NVS"), Some("nvs"));
        assert_eq!(find("ota_1"), Some("app1"));
        assert_eq!(find("littlefs"), Some("spiffs"));
        assert_eq!(find("ota_x"), None);
        assert_eq!(find("coredump"), None);
    }

    #[test]
    fn power_loss_range_is_the_partition() {
        let mut flash = vec![0xff; 0x10000];
        // One partition table entry: nvs at 0x9000, 0x5000 bytes.
        let mut e = vec![0xAA, 0x50, 1, 2];
        e.extend_from_slice(&0x9000u32.to_le_bytes());
        e.extend_from_slice(&0x5000u32.to_le_bytes());
        e.extend_from_slice(b"nvs\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        flash[0x8000..0x8020].copy_from_slice(&e[..32]);
        let p = PowerLoss { partition: Some("nvs".into()), ..Default::default() };
        let t = power_loss_trigger(&p, &flash).unwrap();
        assert_eq!(t.range, Some((0x9000, 0xe000)));
        assert_eq!(t.what, "partition nvs");
        let p = PowerLoss { partition: Some("otadata".into()), ..Default::default() };
        assert!(power_loss_trigger(&p, &flash).unwrap_err().contains("the table has nvs"));
    }
}
