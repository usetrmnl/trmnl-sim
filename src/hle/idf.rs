//! HLE hooks for ESP-IDF / Arduino-ESP32 APIs. These are chip-independent: any
//! SoC running an IDF build can install them.

use super::{Flow, HleCtx, Hooks, MachineRequest};
use crate::firmware::Symbols;

pub fn install(hooks: &mut Hooks, syms: &Symbols) {
    hooks.install(syms, "analogReadMilliVolts", analog_read_mv);
    hooks.install(syms, "__analogReadMilliVolts", analog_read_mv);
    hooks.install(syms, "analogRead", analog_read);
    hooks.install(syms, "__analogRead", analog_read);

    hooks.install(syms, "esp_sleep_enable_timer_wakeup", sleep_enable_timer);
    hooks.install(syms, "esp_deep_sleep_enable_gpio_wakeup", deep_sleep_enable_gpio);
    hooks.install(syms, "esp_sleep_enable_gpio_wakeup", sleep_enable_gpio);
    hooks.install(syms, "esp_sleep_disable_wakeup_source", sleep_disable_source);
    hooks.install(syms, "esp_deep_sleep_start", deep_sleep_start);
    hooks.install(syms, "esp_light_sleep_start", light_sleep_start);
    super::wifi::install(hooks, syms);
}

/// esp_err_t esp_sleep_enable_timer_wakeup(uint64_t time_in_us)
fn sleep_enable_timer(c: &mut HleCtx) -> Flow {
    let us = c.cpu.arg(0) as u64 | (c.cpu.arg(1) as u64) << 32;
    c.state.sleep.timer_us = Some(us);
    Flow::Continue
}

/// esp_err_t esp_deep_sleep_enable_gpio_wakeup(uint64_t gpio_pin_mask, esp_deepsleep_gpio_wake_up_mode_t mode)
fn deep_sleep_enable_gpio(c: &mut HleCtx) -> Flow {
    let mask = c.cpu.arg(0) as u64 | (c.cpu.arg(1) as u64) << 32;
    let high = c.cpu.arg(2) != 0;
    if !high {
        c.state.sleep.gpio_low_mask |= mask;
    }
    Flow::Continue
}

fn sleep_enable_gpio(c: &mut HleCtx) -> Flow {
    c.state.sleep.light_gpio = true;
    Flow::Continue
}

/// esp_err_t esp_sleep_disable_wakeup_source(esp_sleep_source_t source)
fn sleep_disable_source(c: &mut HleCtx) -> Flow {
    match c.cpu.arg(0) {
        0 => c.state.sleep = Default::default(), // ESP_SLEEP_WAKEUP_ALL
        4 => c.state.sleep.timer_us = None,      // ESP_SLEEP_WAKEUP_TIMER
        7 => {
            c.state.sleep.gpio_low_mask = 0; // ESP_SLEEP_WAKEUP_GPIO
            c.state.sleep.light_gpio = false;
        }
        _ => {}
    }
    Flow::Continue
}

fn deep_sleep_start(c: &mut HleCtx) -> Flow {
    let s = c.state.sleep.clone();
    c.env.request(MachineRequest::DeepSleep {
        timer_ns: s.timer_us.map(|us| us * 1000),
        gpio_low_mask: s.gpio_low_mask,
    });
    // Never returns on hardware; the machine stops executing.
    Flow::Return(None)
}

fn light_sleep_start(c: &mut HleCtx) -> Flow {
    let s = c.state.sleep.clone();
    c.env.request(MachineRequest::LightSleep { timer_ns: s.timer_us.map(|us| us * 1000), gpio: s.light_gpio });
    Flow::Return(Some(0))
}

/// uint32_t analogReadMilliVolts(uint8_t pin)
fn analog_read_mv(c: &mut HleCtx) -> Flow {
    let pin = c.cpu.arg(0) as u8;
    let mv = c.env.adc_millivolts(pin);
    Flow::Return(Some(mv))
}

/// uint16_t analogRead(uint8_t pin): 12-bit reading, ~2500 mV full scale (11 dB).
fn analog_read(c: &mut HleCtx) -> Flow {
    let pin = c.cpu.arg(0) as u8;
    let mv = c.env.adc_millivolts(pin);
    Flow::Return(Some((mv * 4095 / 2500).min(4095)))
}
