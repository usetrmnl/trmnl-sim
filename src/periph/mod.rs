//! Chip-independent models of Espressif peripheral IP blocks, shared by the SoC
//! modules (which own the register maps and wiring).

pub mod crypto_math;
pub mod i2c;
pub mod sha;
pub mod systimer;
