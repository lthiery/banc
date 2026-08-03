//! Reference assistant firmware for the banc HIL test framework.
//!
//! The real contents of this crate are RP2350/embassy firmware implementing
//! `banc-icd` (GPIO, pin-edge monitoring with local timestamps, UART,
//! SPI/I2C controller transactions, edge capture) over the postcard-rpc
//! embassy-usb server. This 0.0.x release is a name reservation; the
//! firmware lives in the banc repository.

#![no_std]
