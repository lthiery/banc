//! Interface control document (ICD) for the banc HIL test framework.
//!
//! Two endpoint families live here, both domain-neutral:
//!
//! - [`node`]: minimal node management every banc-speaking device implements
//!   (identify, reset). Ping comes from `postcard_rpc::standard_icd`.
//! - [`assistant`]: the v0 surface of the reference assistant — GPIO, pin-edge
//!   monitoring, UART, SPI/I2C controller transactions, and timestamped edge
//!   capture. All timing data carries assistant-local timestamps; the host
//!   never asserts timing from its own wall clock. Note the two edge paths
//!   differ in rigor: live [`PinEdgeTopic`] monitoring is
//!   best-effort (task-wake timestamps, edges may coalesce), while buffered
//!   [`assistant::CaptureChunk`] is the hardware-backed, overflow-reporting
//!   path for timing assertions.
//!
//! Consumers building combined firmware (their own endpoints + banc's) must
//! list banc's endpoints in their single `endpoints!`/`define_dispatch!`
//! table. postcard-rpc keys are structural (path + schema hash), so
//! re-declaring marker types against the wire types and [`paths`] constants
//! exported here yields identical keys to the ones banc-host uses.

#![no_std]
#![forbid(unsafe_code)]

use postcard_rpc::{TopicDirection, endpoints, topics};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to paths or schemas. Hosts compare this
/// against [`node::Identity::protocol_version`] before running suites.
pub const PROTOCOL_VERSION: u32 = 0;

/// Canonical path strings, exported so consumer ICD tables can reference them
/// instead of retyping (a typo would silently change the key).
pub mod paths {
    pub const NODE_IDENTIFY: &str = "banc/node/identify";
    pub const NODE_RESET: &str = "banc/node/reset";

    pub const GPIO_CONFIG: &str = "banc/assistant/gpio/config";
    pub const GPIO_SET: &str = "banc/assistant/gpio/set";
    pub const GPIO_READ: &str = "banc/assistant/gpio/read";
    pub const PIN_MONITOR: &str = "banc/assistant/pin/monitor";
    pub const PIN_EDGE: &str = "banc/assistant/pin/edge";
    pub const UART_CONFIG: &str = "banc/assistant/uart/config";
    pub const UART_TX: &str = "banc/assistant/uart/tx";
    pub const UART_RX: &str = "banc/assistant/uart/rx";
    pub const SPI_TRANSFER: &str = "banc/assistant/spi/transfer";
    pub const I2C_TRANSACTION: &str = "banc/assistant/i2c/transaction";
    pub const CAPTURE_CONTROL: &str = "banc/assistant/capture/control";
    pub const CAPTURE_READ: &str = "banc/assistant/capture/read";
}

/// Node-management types: implemented by every banc node regardless of role.
pub mod node {
    use super::*;

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NodeRole {
        /// The banc reference assistant firmware.
        Assistant,
        /// A consumer-defined node (its ICD describes what it does).
        Custom,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct Identity {
        pub role: NodeRole,
        pub protocol_version: u32,
        /// Stable per-device ID (e.g. flash unique ID). Also surfaced as the
        /// USB serial string so hosts can route before connecting.
        pub unique_id: u64,
        pub fw_name: heapless::String<32>,
        pub fw_version: heapless::String<16>,
    }
}

/// Reference-assistant v0 types.
pub mod assistant {
    use super::*;

    /// Max payload per UART/SPI/I2C transaction chunk in v0.
    pub const CHUNK: usize = 64;
    /// Max pin events returned per capture-read chunk.
    pub const EVENTS_PER_CHUNK: usize = 16;

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Level {
        Low,
        High,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Pull {
        None,
        Up,
        Down,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PinMode {
        Input(Pull),
        Output,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        /// Pin/bus index not wired up on this assistant.
        Unsupported,
        /// Pin/bus is claimed by another function (e.g. capture running).
        Busy,
        /// The peripheral reported a fault (NAK, framing error, overrun...).
        Hardware,
        /// Request out of range (bad length, bad offset).
        Invalid,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GpioConfig {
        pub pin: u8,
        pub mode: PinMode,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GpioSet {
        pub pin: u8,
        pub level: Level,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GpioRead {
        pub pin: u8,
    }

    /// One observed edge, timestamped by the assistant's own clock
    /// (microseconds since assistant boot).
    ///
    /// Best-effort. When carried by [`crate::PinEdgeTopic`] the timestamp is taken
    /// after the monitor task wakes, so it includes scheduler and interrupt
    /// latency, and edges arriving faster than the task runs can coalesce or
    /// be missed. Good for functional GPIO observation ("did the line
    /// toggle"), not for rigorous timing. Hardware-backed timing lives in the
    /// buffered capture path ([`CaptureChunk`]), where events are stamped in
    /// the capture engine and overflow is reported rather than silently
    /// dropped.
    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PinEvent {
        pub pin: u8,
        pub level: Level,
        pub timestamp_us: u64,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PinMonitor {
        pub pin: u8,
        /// true: publish a best-effort [`crate::PinEdgeTopic`] per observed edge
        /// (edges may coalesce under load, see [`PinEvent`]); false: stop.
        pub enable: bool,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UartConfig {
        pub baud: u32,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct Chunk {
        pub data: heapless::Vec<u8, CHUNK>,
    }

    /// UART bytes as received, stamped with the assistant-local time of the
    /// first byte in the chunk.
    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct UartRxChunk {
        pub timestamp_us: u64,
        pub data: heapless::Vec<u8, CHUNK>,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct SpiTransfer {
        /// Bytes to clock out; the same number of bytes is clocked in.
        pub write: heapless::Vec<u8, CHUNK>,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct I2cTransaction {
        pub addr: u8,
        pub write: heapless::Vec<u8, CHUNK>,
        /// Bytes to read after the write phase (0 = write-only).
        pub read_len: u8,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CaptureAction {
        /// Arm edge capture on a pin; events accumulate assistant-side.
        Start,
        /// Disarm; captured events stay retrievable until the next Start.
        Stop,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CaptureControl {
        pub pin: u8,
        pub action: CaptureAction,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CaptureStatus {
        /// Events currently buffered.
        pub count: u32,
        /// Events dropped because the buffer filled. Nonzero means the
        /// capture is incomplete; hosts should fail timing assertions
        /// rather than reason over a gappy record.
        pub dropped: u32,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CaptureRead {
        /// Event index to read from (chunked retrieval).
        pub offset: u32,
    }

    #[derive(Serialize, Deserialize, Schema, Debug, Clone, PartialEq, Eq)]
    pub struct CaptureChunk {
        pub events: heapless::Vec<PinEvent, EVENTS_PER_CHUNK>,
        /// Events remaining after this chunk.
        pub remaining: u32,
    }

    // The endpoints! macro takes one token tree per type column, so the
    // Result response types go through aliases.
    pub type AckResult = Result<(), Error>;
    pub type LevelResult = Result<Level, Error>;
    pub type ChunkResult = Result<Chunk, Error>;
    pub type CaptureStatusResult = Result<CaptureStatus, Error>;
    pub type CaptureChunkResult = Result<CaptureChunk, Error>;
}

use assistant::*;
use node::*;

endpoints! {
    list = ENDPOINT_LIST;
    | EndpointTy            | RequestTy       | ResponseTy                      | Path                             |
    | ----------            | ---------       | ----------                      | ----                             |
    | IdentifyEndpoint      | ()              | Identity                        | "banc/node/identify"             |
    | ResetEndpoint         | ()              | ()                              | "banc/node/reset"                |
    | GpioConfigEndpoint    | GpioConfig      | AckResult                       | "banc/assistant/gpio/config"     |
    | GpioSetEndpoint       | GpioSet         | AckResult                       | "banc/assistant/gpio/set"        |
    | GpioReadEndpoint      | GpioRead        | LevelResult                     | "banc/assistant/gpio/read"       |
    | PinMonitorEndpoint    | PinMonitor      | AckResult                       | "banc/assistant/pin/monitor"     |
    | UartConfigEndpoint    | UartConfig      | AckResult                       | "banc/assistant/uart/config"     |
    | UartTxEndpoint        | Chunk           | AckResult                       | "banc/assistant/uart/tx"         |
    | SpiTransferEndpoint   | SpiTransfer     | ChunkResult                     | "banc/assistant/spi/transfer"    |
    | I2cTransactionEndpoint| I2cTransaction  | ChunkResult                     | "banc/assistant/i2c/transaction" |
    | CaptureControlEndpoint| CaptureControl  | CaptureStatusResult             | "banc/assistant/capture/control" |
    | CaptureReadEndpoint   | CaptureRead     | CaptureChunkResult              | "banc/assistant/capture/read"    |
}

topics! {
    list = TOPICS_OUT_LIST;
    direction = TopicDirection::ToClient;
    | TopicTy               | MessageTy       | Path                             |
    | -------               | ---------       | ----                             |
    | PinEdgeTopic          | PinEvent        | "banc/assistant/pin/edge"        |
    | UartRxTopic           | UartRxChunk     | "banc/assistant/uart/rx"         |
}

topics! {
    list = TOPICS_IN_LIST;
    direction = TopicDirection::ToServer;
    | TopicTy               | MessageTy       | Path                             |
    | -------               | ---------       | ----                             |
}
