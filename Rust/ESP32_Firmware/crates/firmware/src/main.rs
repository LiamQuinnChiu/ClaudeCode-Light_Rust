//! CursorLight ESP32-C3 固件。
//!
//! BLE 外设提供灯效服务与 OTA 服务；三路 PWM 驱动红黄绿 LED。
//! 灯效逻辑在 `light-core`，BLE 与灯效之间通过消息通道传递，无共享可变状态。

#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    timer::timg::TimerGroup,
};
use esp_radio::ble::controller::BleConnector;
use log::info;
use static_cell::StaticCell;
use trouble_host::prelude::ExternalController;

mod ble;
mod led;
mod ota;

const FW_VERSION: &str = "0.4.1";

/// BLE 任务到动画任务的消息通道。
pub type LedChannel = Channel<CriticalSectionRawMutex, led::LedCmd, 4>;
static LED_CHANNEL: StaticCell<LedChannel> = StaticCell::new();

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_ints.software_interrupt0);

    info!("CursorLight Rust firmware {}", FW_VERSION);
    info!("Power on. Default mode: demo");
    info!("Common anode BLE enhanced version.");
    info!("BLE device name: {}", ble::BLE_DEVICE_NAME);

    let mut leds = led::Leds::new(
        peripherals.LEDC,
        peripherals.GPIO2,
        peripherals.GPIO3,
        peripherals.GPIO4,
    );
    let led_channel: &'static LedChannel = LED_CHANNEL.init(LedChannel::new());
    spawner.spawn(led::led_task(leds, led_channel).unwrap());

    let connector = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let controller: ExternalController<_, 20> = ExternalController::new(connector);

    // flash 只能初始化一次，交给 BLE 任务里的 OTA 使用
    let flash = esp_storage::FlashStorage::new(peripherals.FLASH);

    ble::run(controller, led_channel, flash).await;
}
