//! Reference assistant firmware for the banc HIL test framework.
//!
//! Target: Raspberry Pi Pico 2 (RP2350A), embassy + postcard-rpc over USB.
//!
//! Implemented in this skeleton:
//! - node management: identify (real), reset (real, delayed `sys_reset`);
//! - GPIO config/set/read over a fixed table of GPIO 2..=9 mapped to banc
//!   pins 0..=7;
//! - best-effort pin-edge monitoring publishing [`PinEdgeTopic`] with
//!   assistant-local microsecond timestamps (see the handler for why this is
//!   functional-only, not a timing reference).
//!
//! Honest stubs (`Err(Error::Unsupported)`, wired in Phase 2): UART, SPI,
//! I2C, edge capture.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use banc_icd::{
    CaptureControlEndpoint, CaptureReadEndpoint, ENDPOINT_LIST, GpioConfigEndpoint,
    GpioReadEndpoint, GpioSetEndpoint, I2cTransactionEndpoint, IdentifyEndpoint, PROTOCOL_VERSION,
    PinEdgeTopic, PinMonitorEndpoint, ResetEndpoint, SpiTransferEndpoint, TOPICS_IN_LIST,
    TOPICS_OUT_LIST, UartConfigEndpoint, UartTxEndpoint,
    assistant::{
        AckResult, CaptureChunkResult, CaptureControl, CaptureRead, CaptureStatusResult, Chunk,
        ChunkResult, Error as IcdError, GpioConfig, GpioRead, GpioSet, I2cTransaction,
        Level as IcdLevel, LevelResult, PinEvent, PinMode, PinMonitor, Pull as IcdPull,
        SpiTransfer, UartConfig,
    },
    node::{Identity, NodeRole},
};
use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_rp::{
    bind_interrupts,
    gpio::{Flex, Pull},
    otp,
    peripherals::USB,
    usb,
};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Instant, Timer};
use embassy_usb::{Config, UsbDevice};
use postcard_rpc::{
    define_dispatch,
    header::VarHeader,
    server::{
        Dispatch, Sender, Server, SpawnContext,
        impls::embassy_usb_v0_5::{
            PacketBuffers, USB_FS_MAX_PACKET_SIZE,
            dispatch_impl::{
                WireRxBuf, WireRxImpl, WireSpawnImpl, WireStorage, WireTxImpl, spawn_fn,
            },
        },
    },
};
use static_cell::{ConstStaticCell, StaticCell};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

// Program metadata for `picotool info`.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"banc-assistant"),
    embassy_rp::binary_info::rp_program_description!(
        c"Reference assistant firmware for the banc HIL test framework"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

/// banc pins 0..=7 map to GPIO 2..=9 (GPIO 0/1 are kept free for the
/// Phase-2 UART peer).
const NUM_PINS: usize = 8;

fn pin_index(banc_pin: u8) -> Option<usize> {
    let idx = usize::from(banc_pin);
    (idx < NUM_PINS).then_some(idx)
}

/// The GPIO table. `None` means the pin is currently owned by a monitor
/// task; GPIO endpoints answer `Busy` for it until monitoring stops.
type PinSlots = Mutex<ThreadModeRawMutex, [Option<Flex<'static>>; NUM_PINS]>;

/// Stop signals for the per-pin monitor tasks.
static MONITOR_STOP: [Signal<ThreadModeRawMutex, ()>; NUM_PINS] =
    [const { Signal::new() }; NUM_PINS];

type AppDriver = usb::Driver<'static, USB>;
type AppStorage = WireStorage<ThreadModeRawMutex, AppDriver, 256, 256, 64, 256>;
type BufStorage = PacketBuffers<1024, 1024>;
type AppTx = WireTxImpl<ThreadModeRawMutex, AppDriver>;
type AppRx = WireRxImpl<AppDriver>;
type AppServer = Server<AppTx, AppRx, WireRxBuf, BancApp>;

static PBUFS: ConstStaticCell<BufStorage> = ConstStaticCell::new(BufStorage::new());
static STORAGE: AppStorage = AppStorage::new();
static PIN_SLOTS: StaticCell<PinSlots> = StaticCell::new();
static USB_SERIAL: StaticCell<heapless::String<16>> = StaticCell::new();

pub struct Context {
    pub unique_id: u64,
    pub pins: &'static PinSlots,
}

pub struct SpawnCtx {
    pub pins: &'static PinSlots,
}

impl SpawnContext for Context {
    type SpawnCtxt = SpawnCtx;
    fn spawn_ctxt(&mut self) -> Self::SpawnCtxt {
        SpawnCtx { pins: self.pins }
    }
}

fn usb_config(serial: &'static str) -> Config<'static> {
    let mut config = Config::new(0x16c0, 0x27dd);
    config.manufacturer = Some("banc");
    config.product = Some("banc-assistant");
    config.serial_number = Some(serial);

    // Required for windows compatibility.
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    config
}

define_dispatch! {
    app: BancApp;
    spawn_fn: spawn_fn;
    tx_impl: AppTx;
    spawn_impl: WireSpawnImpl;
    context: Context;

    endpoints: {
        list: ENDPOINT_LIST;

        | EndpointTy             | kind      | handler                 |
        | ----------             | ----      | -------                 |
        | IdentifyEndpoint       | blocking  | identify_handler        |
        | ResetEndpoint          | spawn     | reset_handler           |
        | GpioConfigEndpoint     | async     | gpio_config_handler     |
        | GpioSetEndpoint        | async     | gpio_set_handler        |
        | GpioReadEndpoint       | async     | gpio_read_handler       |
        | PinMonitorEndpoint     | spawn     | pin_monitor_handler     |
        | UartConfigEndpoint     | blocking  | uart_config_handler     |
        | UartTxEndpoint         | blocking  | uart_tx_handler         |
        | SpiTransferEndpoint    | blocking  | spi_transfer_handler    |
        | I2cTransactionEndpoint | blocking  | i2c_transaction_handler |
        | CaptureControlEndpoint | blocking  | capture_control_handler |
        | CaptureReadEndpoint    | blocking  | capture_read_handler    |
    };
    topics_in: {
        list: TOPICS_IN_LIST;

        | TopicTy                | kind      | handler                 |
        | ----------             | ----      | -------                 |
    };
    topics_out: {
        list: TOPICS_OUT_LIST;
    };
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("banc-assistant start");
    let p = embassy_rp::init(Default::default());

    // RP2350 per-device ID from OTP (rows 0x0..=0x3).
    let unique_id = match otp::get_chipid() {
        Ok(id) => id,
        Err(_) => {
            defmt::warn!("OTP chipid read failed, using 0");
            0
        }
    };
    info!("unique_id: {=u64:016X}", unique_id);

    // GPIO table: banc pins 0..=7 = GPIO 2..=9, all high-Z until configured.
    let pins = PIN_SLOTS.init(Mutex::new([
        Some(Flex::new(p.PIN_2)),
        Some(Flex::new(p.PIN_3)),
        Some(Flex::new(p.PIN_4)),
        Some(Flex::new(p.PIN_5)),
        Some(Flex::new(p.PIN_6)),
        Some(Flex::new(p.PIN_7)),
        Some(Flex::new(p.PIN_8)),
        Some(Flex::new(p.PIN_9)),
    ]));

    // USB serial string = hex of the unique id, so hosts can route before
    // connecting.
    let serial = USB_SERIAL.init(heapless::String::new());
    let _ = write!(serial, "{unique_id:016X}");

    let driver = usb::Driver::new(p.USB, Irqs);
    let pbufs = PBUFS.take();
    let config = usb_config(serial.as_str());

    let (device, tx_impl, rx_impl) = STORAGE.init(
        driver,
        config,
        pbufs.tx_buf.as_mut_slice(),
        USB_FS_MAX_PACKET_SIZE,
    );

    let context = Context { unique_id, pins };
    let dispatcher = BancApp::new(context, spawner.into());
    let vkk = dispatcher.min_key_len();
    let mut server: AppServer = Server::new(
        tx_impl,
        rx_impl,
        pbufs.rx_buf.as_mut_slice(),
        dispatcher,
        vkk,
    );
    spawner.must_spawn(usb_task(device));

    loop {
        // If the host disconnects we get an error here; just keep serving
        // so the next enumeration picks up where we left off.
        let _ = server.run().await;
    }
}

/// Low-level USB management.
#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, AppDriver>) {
    usb.run().await;
}

// --- node management ---

fn identify_handler(context: &mut Context, _header: VarHeader, _rqst: ()) -> Identity {
    info!("identify");
    Identity {
        role: NodeRole::Assistant,
        protocol_version: PROTOCOL_VERSION,
        unique_id: context.unique_id,
        fw_name: heapless::String::try_from("banc-assistant").unwrap_or_default(),
        fw_version: heapless::String::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
    }
}

/// Reply first, give the wire a moment to flush, then reset the chip.
#[embassy_executor::task]
async fn reset_handler(_context: SpawnCtx, header: VarHeader, _rqst: (), sender: Sender<AppTx>) {
    info!("reset requested");
    let _ = sender.reply::<ResetEndpoint>(header.seq_no, &()).await;
    Timer::after_millis(100).await;
    cortex_m::peripheral::SCB::sys_reset();
}

// --- GPIO ---

fn icd_pull(pull: IcdPull) -> Pull {
    match pull {
        IcdPull::None => Pull::None,
        IcdPull::Up => Pull::Up,
        IcdPull::Down => Pull::Down,
    }
}

async fn gpio_config_handler(
    context: &mut Context,
    _header: VarHeader,
    rqst: GpioConfig,
) -> AckResult {
    let idx = pin_index(rqst.pin).ok_or(IcdError::Unsupported)?;
    let mut slots = context.pins.lock().await;
    let pin = slots[idx].as_mut().ok_or(IcdError::Busy)?;
    match rqst.mode {
        PinMode::Input(pull) => {
            pin.set_pull(icd_pull(pull));
            pin.set_as_input();
        }
        PinMode::Output => {
            pin.set_pull(Pull::None);
            pin.set_as_output();
        }
    }
    Ok(())
}

async fn gpio_set_handler(context: &mut Context, _header: VarHeader, rqst: GpioSet) -> AckResult {
    let idx = pin_index(rqst.pin).ok_or(IcdError::Unsupported)?;
    let mut slots = context.pins.lock().await;
    let pin = slots[idx].as_mut().ok_or(IcdError::Busy)?;
    match rqst.level {
        IcdLevel::Low => pin.set_low(),
        IcdLevel::High => pin.set_high(),
    }
    Ok(())
}

async fn gpio_read_handler(
    context: &mut Context,
    _header: VarHeader,
    rqst: GpioRead,
) -> LevelResult {
    let idx = pin_index(rqst.pin).ok_or(IcdError::Unsupported)?;
    let slots = context.pins.lock().await;
    let pin = slots[idx].as_ref().ok_or(IcdError::Busy)?;
    Ok(if pin.is_high() {
        IcdLevel::High
    } else {
        IcdLevel::Low
    })
}

// --- pin-edge monitoring ---

/// Owns the pin for the duration of the monitor (GPIO endpoints answer
/// `Busy` meanwhile), publishes [`PinEdgeTopic`] per observed edge with the
/// assistant-local microsecond timestamp taken right after wake-up.
///
/// Best-effort by construction: `wait_for_any_edge` wakes the task, which
/// then reads the level and stamps the time, so the timestamp carries
/// scheduler and interrupt latency and edges arriving faster than the task
/// runs collapse into one (or are missed). This is fine for functional GPIO
/// observation but is not a timing reference; rigorous timing belongs to the
/// buffered capture path (Phase 2), which stamps in hardware and reports
/// overflow.
///
/// Pool sizing: up to `NUM_PINS` long-running monitors plus headroom for
/// short-lived disable/out-of-range requests hitting the same endpoint.
#[embassy_executor::task(pool_size = NUM_PINS + 2)]
async fn pin_monitor_handler(
    context: SpawnCtx,
    header: VarHeader,
    rqst: PinMonitor,
    sender: Sender<AppTx>,
) {
    let Some(idx) = pin_index(rqst.pin) else {
        let _ = sender
            .reply::<PinMonitorEndpoint>(header.seq_no, &Err(IcdError::Unsupported))
            .await;
        return;
    };

    if !rqst.enable {
        MONITOR_STOP[idx].signal(());
        let _ = sender
            .reply::<PinMonitorEndpoint>(header.seq_no, &Ok(()))
            .await;
        return;
    }

    let taken = context.pins.lock().await[idx].take();
    let Some(mut pin) = taken else {
        // Already monitored (the slot is empty while a monitor owns it).
        let _ = sender
            .reply::<PinMonitorEndpoint>(header.seq_no, &Err(IcdError::Busy))
            .await;
        return;
    };

    MONITOR_STOP[idx].reset();
    if sender
        .reply::<PinMonitorEndpoint>(header.seq_no, &Ok(()))
        .await
        .is_err()
    {
        context.pins.lock().await[idx] = Some(pin);
        return;
    }
    info!("pin {=u8} monitor on", rqst.pin);

    let mut seq: u16 = 0;
    loop {
        match select(MONITOR_STOP[idx].wait(), pin.wait_for_any_edge()).await {
            Either::First(()) => break,
            Either::Second(()) => {
                let event = PinEvent {
                    pin: rqst.pin,
                    level: if pin.is_high() {
                        IcdLevel::High
                    } else {
                        IcdLevel::Low
                    },
                    timestamp_us: Instant::now().as_micros(),
                };
                let _ = sender.publish::<PinEdgeTopic>(seq.into(), &event).await;
                seq = seq.wrapping_add(1);
            }
        }
    }

    info!("pin {=u8} monitor off", rqst.pin);
    context.pins.lock().await[idx] = Some(pin);
}

// --- Phase-2 stubs (honest Unsupported answers) ---

fn uart_config_handler(_context: &mut Context, _header: VarHeader, _rqst: UartConfig) -> AckResult {
    // TODO(phase 2): UART peer on GPIO 0/1 publishing UartRxTopic.
    Err(IcdError::Unsupported)
}

fn uart_tx_handler(_context: &mut Context, _header: VarHeader, _rqst: Chunk) -> AckResult {
    // TODO(phase 2)
    Err(IcdError::Unsupported)
}

fn spi_transfer_handler(
    _context: &mut Context,
    _header: VarHeader,
    _rqst: SpiTransfer,
) -> ChunkResult {
    // TODO(phase 2): SPI controller transactions.
    Err(IcdError::Unsupported)
}

fn i2c_transaction_handler(
    _context: &mut Context,
    _header: VarHeader,
    _rqst: I2cTransaction,
) -> ChunkResult {
    // TODO(phase 2): I2C controller transactions.
    Err(IcdError::Unsupported)
}

fn capture_control_handler(
    _context: &mut Context,
    _header: VarHeader,
    _rqst: CaptureControl,
) -> CaptureStatusResult {
    // TODO(phase 2): buffered edge capture (PIO- or interrupt-driven).
    Err(IcdError::Unsupported)
}

fn capture_read_handler(
    _context: &mut Context,
    _header: VarHeader,
    _rqst: CaptureRead,
) -> CaptureChunkResult {
    // TODO(phase 2)
    Err(IcdError::Unsupported)
}
