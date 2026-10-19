//! esp_lcd Intel 8080 color transfers done natively: the bytes go straight to the board,
//! skipping the IDF driver's queue, DMA descriptors and interrupt, which are most of the guest
//! instructions an X refresh runs. The core still spends the bus time in ets_delay_us, as the
//! firmware waits out the DMA on the chip. Only the plain case is taken; anything else,
//! including what the driver rejects or asserts on, runs the IDF driver.

use std::collections::HashMap;

use super::{Flow, HleCtx, Hooks};
use crate::firmware::Symbols;

const ESP_OK: u32 = 0;
/// `esp_lcd_i80_bus_config_t` fields (IDF 5.5, ESP_LCD_I80_BUS_WIDTH_MAX = 16).
const BUS_CONFIG_BUS_WIDTH: u32 = 76;
const BUS_CONFIG_MAX_TRANSFER_BYTES: u32 = 80;
/// `esp_lcd_panel_io_i80_config_t` fields.
const IO_CONFIG_PCLK_HZ: u32 = 4;
const IO_CONFIG_ON_COLOR_TRANS_DONE: u32 = 12;
const IO_CONFIG_USER_CTX: u32 = 16;
const IO_CONFIG_FLAGS: u32 = 32;
/// `reverse_color_bits` and `swap_color_bytes` in the config's flags.
const REORDERED_COLOR_FLAGS: u32 = 0b110;
/// The internal SRAM the S3's GDMA reads (SOC_DMA_LOW..SOC_DMA_HIGH). Its alignment there is
/// one byte (AHB GDMA version 1), so the driver accepts any address and length.
const DMA_RAM: std::ops::Range<u32> = 0x3FC8_8000..0x3FD0_0000;

#[derive(Default)]
pub struct LcdState {
    buses: HashMap<u32, Bus>,
    ios: HashMap<u32, PanelIo>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bus {
    bus_width: u32,
    max_transfer_bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanelIo {
    bus: Bus,
    pclk_hz: u32,
    on_color_trans_done: u32,
    user_ctx: u32,
    reordered_color: bool,
}

pub fn install(hooks: &mut Hooks, syms: &Symbols) {
    hooks.install(syms, "esp_lcd_new_i80_bus", new_bus);
    hooks.install(syms, "esp_lcd_new_panel_io_i80", new_panel_io);
    hooks.install(syms, "esp_lcd_panel_io_register_event_callbacks", register_event_callbacks);
    hooks.install(syms, "esp_lcd_panel_io_del", del_panel_io);
    hooks.install(syms, "esp_lcd_panel_io_tx_color", tx_color);
}

/// Is this transfer one the IDF driver would send unchanged, with no command phase?
fn is_plain(io: &PanelIo, lcd_cmd: i32, color: u32, color_size: u32) -> bool {
    lcd_cmd == -1
        && !io.reordered_color
        && color_size > 0
        && color_size <= io.bus.max_transfer_bytes
        && io.bus.bus_width >= 8
        && io.pclk_hz > 0
        && DMA_RAM.contains(&color)
        && DMA_RAM.contains(&(color + color_size - 1))
}

/// Microseconds the bus takes to clock out `color_size` bytes, one bus width per PCLK.
fn bus_time_us(io: &PanelIo, color_size: u32) -> u32 {
    let clocks = color_size as u64 * 8 / io.bus.bus_width as u64;
    clocks.saturating_mul(1_000_000).div_ceil(io.pclk_hz as u64) as u32
}

/// esp_err_t esp_lcd_new_i80_bus(const esp_lcd_i80_bus_config_t *bus_config, esp_lcd_i80_bus_handle_t *ret_bus)
fn new_bus(c: &mut HleCtx) -> Flow {
    let (config, ret_bus) = (c.cpu.arg(0), c.cpu.arg(1));
    let (Some(bus_width), Some(max_transfer_bytes)) =
        (c.mem.read_u32(config + BUS_CONFIG_BUS_WIDTH), c.mem.read_u32(config + BUS_CONFIG_MAX_TRANSFER_BYTES))
    else {
        return Flow::Continue;
    };
    Flow::Wrap(Box::new(move |c, ret| {
        if ret == ESP_OK
            && let Some(bus) = c.mem.read_u32(ret_bus)
        {
            c.state.lcd.buses.insert(bus, Bus { bus_width, max_transfer_bytes });
        }
    }))
}

/// esp_err_t esp_lcd_new_panel_io_i80(esp_lcd_i80_bus_handle_t bus, const esp_lcd_panel_io_i80_config_t *io_config, esp_lcd_panel_io_handle_t *ret_io)
fn new_panel_io(c: &mut HleCtx) -> Flow {
    let (bus, config, ret_io) = (c.cpu.arg(0), c.cpu.arg(1), c.cpu.arg(2));
    let field = |c: &HleCtx, offset| c.mem.read_u32(config + offset);
    let (Some(&bus), Some(pclk_hz), Some(on_color_trans_done), Some(user_ctx), Some(flags)) = (
        c.state.lcd.buses.get(&bus),
        field(c, IO_CONFIG_PCLK_HZ),
        field(c, IO_CONFIG_ON_COLOR_TRANS_DONE),
        field(c, IO_CONFIG_USER_CTX),
        field(c, IO_CONFIG_FLAGS),
    ) else {
        return Flow::Continue;
    };
    let io =
        PanelIo { bus, pclk_hz, on_color_trans_done, user_ctx, reordered_color: flags & REORDERED_COLOR_FLAGS != 0 };
    Flow::Wrap(Box::new(move |c, ret| {
        if ret == ESP_OK
            && let Some(handle) = c.mem.read_u32(ret_io)
        {
            c.state.lcd.ios.insert(handle, io);
        }
    }))
}

/// esp_err_t esp_lcd_panel_io_register_event_callbacks(esp_lcd_panel_io_handle_t io, const esp_lcd_panel_io_callbacks_t *cbs, void *user_ctx)
fn register_event_callbacks(c: &mut HleCtx) -> Flow {
    let (handle, callbacks, user_ctx) = (c.cpu.arg(0), c.cpu.arg(1), c.cpu.arg(2));
    let on_color_trans_done = c.mem.read_u32(callbacks);
    Flow::Wrap(Box::new(move |c, ret| {
        if let (ESP_OK, Some(on_color_trans_done), Some(io)) =
            (ret, on_color_trans_done, c.state.lcd.ios.get_mut(&handle))
        {
            io.on_color_trans_done = on_color_trans_done;
            io.user_ctx = user_ctx;
        }
    }))
}

/// esp_err_t esp_lcd_panel_io_del(esp_lcd_panel_io_handle_t io)
fn del_panel_io(c: &mut HleCtx) -> Flow {
    c.state.lcd.ios.remove(&c.cpu.arg(0));
    Flow::Continue
}

/// esp_err_t esp_lcd_panel_io_tx_color(esp_lcd_panel_io_handle_t io, int lcd_cmd, const void *color, size_t color_size)
/// Natively: the bytes, the bus time in ets_delay_us, then the done callback as the driver's
/// interrupt calls it (edata NULL).
fn tx_color(c: &mut HleCtx) -> Flow {
    let (handle, lcd_cmd, color, color_size) = (c.cpu.arg(0), c.cpu.arg(1) as i32, c.cpu.arg(2), c.cpu.arg(3));
    let Some(io) = c.state.lcd.ios.get(&handle).copied() else {
        return Flow::Continue;
    };
    if !is_plain(&io, lcd_cmd, color, color_size) {
        return Flow::Continue;
    }
    let (Some(data), Some(delay_us)) = (c.mem.read_bytes(color, color_size as usize), c.syms.addr("ets_delay_us"))
    else {
        return Flow::Continue;
    };
    c.env.lcd_transfer(&data);
    Flow::Call {
        func: delay_us,
        args: vec![bus_time_us(&io, color_size)],
        then: Box::new(move |_, _| {
            if io.on_color_trans_done == 0 {
                return Flow::Return(Some(ESP_OK));
            }
            Flow::Call {
                func: io.on_color_trans_done,
                args: vec![handle, 0, io.user_ctx],
                then: Box::new(|_, _| Flow::Return(Some(ESP_OK))),
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IO: PanelIo = PanelIo {
        bus: Bus { bus_width: 8, max_transfer_bytes: 1024 },
        pclk_hz: 12_000_000,
        on_color_trans_done: 0x4200_0000,
        user_ctx: 0,
        reordered_color: false,
    };
    const BUFFER: u32 = 0x3FC9_0000;

    #[test]
    fn a_data_only_transfer_from_internal_ram_is_plain() {
        assert!(is_plain(&IO, -1, BUFFER, 1024));
    }

    #[test]
    fn a_command_phase_runs_the_driver() {
        assert!(!is_plain(&IO, 0x2C, BUFFER, 1024));
    }

    #[test]
    fn a_transfer_over_max_transfer_bytes_runs_the_driver_and_its_assert() {
        assert!(!is_plain(&IO, -1, BUFFER, 1025));
    }

    #[test]
    fn a_buffer_outside_dma_ram_runs_the_driver() {
        assert!(!is_plain(&IO, -1, 0x3C00_0000, 1024)); // PSRAM
        assert!(!is_plain(&IO, -1, DMA_RAM.end - 512, 1024)); // runs off the end
    }

    #[test]
    fn reordered_color_bytes_run_the_driver() {
        assert!(!is_plain(&PanelIo { reordered_color: true, ..IO }, -1, BUFFER, 1024));
    }

    #[test]
    fn an_empty_transfer_runs_the_driver() {
        assert!(!is_plain(&IO, -1, BUFFER, 0));
    }

    #[test]
    fn an_unusable_bus_runs_the_driver() {
        assert!(!is_plain(&PanelIo { pclk_hz: 0, ..IO }, -1, BUFFER, 1024));
        assert!(!is_plain(&PanelIo { bus: Bus { bus_width: 0, ..IO.bus }, ..IO }, -1, BUFFER, 1024));
    }

    #[test]
    fn bus_time_is_one_bus_width_per_pixel_clock() {
        assert_eq!(bus_time_us(&IO, 1200), 100); // 1200 clocks at 12 MHz
        assert_eq!(bus_time_us(&PanelIo { bus: Bus { bus_width: 16, ..IO.bus }, ..IO }, 1200), 50);
        assert_eq!(bus_time_us(&IO, 1), 1); // rounded up
    }
}
