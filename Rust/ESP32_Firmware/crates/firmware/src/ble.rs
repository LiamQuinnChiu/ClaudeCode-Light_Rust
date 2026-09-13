//! BLE 外设：灯效服务与 OTA 服务。
//!
//! 广播名 `CursorLight`，广播包带灯效服务 128 位 UUID，设备名放扫描响应。
//! 灯效特征值为模式字符串；OTA 特征用于无线升级。
//! 断开后由广播循环自动重新广播。

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use esp_storage::FlashStorage;
use heapless::{String as HString, Vec as HVec};
use light_core::modes::Mode;
use log::{info, warn};
use trouble_host::prelude::*;

use crate::LedChannel;
use crate::led::LedCmd;
use crate::ota::{CtrlOutcome, OtaMachine};

pub const BLE_DEVICE_NAME: &str = "CursorLight";

/// 灯效服务 UUID（广播包用字节序）。
const SERVICE_UUID_128: [u8; 16] = [
    0xb8, 0xb7, 0xe0, 0x01, 0x7a, 0x6b, 0x4f, 0x4f, 0x9a, 0x8b, 0x11, 0xc0, 0xff, 0xee, 0x00, 0x01,
];

static GAP_APPEARANCE: BluetoothUuid16 = BluetoothUuid16::new(0x0000);

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

const MODE_VALUE_MAX: usize = 16;
const OTA_CTRL_MAX: usize = 96;
const OTA_CHUNK_MAX: usize = 512;
const OTA_STATUS_MAX: usize = 64;

#[gatt_server]
struct Server {
    light: LightService,
    ota: OtaService,
}

#[gatt_service(uuid = "b8b7e001-7a6b-4f4f-9a8b-11c0ffee0001")]
struct LightService {
    #[characteristic(uuid = "b8b7e002-7a6b-4f4f-9a8b-11c0ffee0001", read, write, notify)]
    mode: HString<MODE_VALUE_MAX>,
}

#[gatt_service(uuid = "b8b7e0f0-7a6b-4f4f-9a8b-11c0ffee0001")]
struct OtaService {
    /// `start:<size>:<sha256hex>` / `commit` / `cancel`
    #[characteristic(uuid = "b8b7e0f1-7a6b-4f4f-9a8b-11c0ffee0001", write)]
    ctrl: HVec<u8, OTA_CTRL_MAX>,
    #[characteristic(uuid = "b8b7e0f2-7a6b-4f4f-9a8b-11c0ffee0001", write_without_response)]
    data: HVec<u8, OTA_CHUNK_MAX>,
    /// `ready:512` / `progress:<已收>:<总长>` / `done` / `error:<原因>` / `cancelled`
    #[characteristic(uuid = "b8b7e0f3-7a6b-4f4f-9a8b-11c0ffee0001", read, notify)]
    status: HString<OTA_STATUS_MAX>,
}

/// 启动广播与 GATT 事件循环，永不返回。
pub async fn run<C: Controller>(
    controller: C,
    led_channel: &'static LedChannel,
    mut flash: FlashStorage<'static>,
) {
    let address = Address::random([0x5f, 0x41, 0xc3, 0x01, 0x0e, 0x22]);
    info!("Our address = {}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let host = stack.build();
    let runner = host.runner;
    let mut peripheral = host.peripheral;

    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: BLE_DEVICE_NAME,
        appearance: &GAP_APPEARANCE,
    }))
    .expect("GATT server init failed");

    let mut ota = OtaMachine::new();
    let mut current_mode = Mode::Demo;

    info!("BLE advertising started.");
    info!("Supported modes:");
    info!("demo / thinking / ai / busy / success / error / alarm / off / red / yellow / green");

    let _ = join(ble_task(runner), async {
        loop {
            match advertise(&mut peripheral, &server).await {
                Ok(conn) => {
                    info!("BLE client connected.");
                    notify_mode(&server, &conn, current_mode).await;

                    let _ = gatt_events_task(
                        &server,
                        &conn,
                        led_channel,
                        &mut ota,
                        &mut current_mode,
                        &mut flash,
                    )
                    .await;

                    if ota.is_active() {
                        warn!("连接断开，OTA 中止");
                        ota.on_ctrl(&mut flash, b"cancel");
                        let _ = led_channel.try_send(LedCmd::OtaResume);
                    }
                    info!("BLE client disconnected. Restart advertising.");
                }
                Err(e) => warn!("[adv] error: {:?}", e),
            }
        }
    })
    .await;
}

async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            warn!("[ble_task] error: {:?}, retrying in 1s", e);
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}

async fn advertise<'values, 'server, C: Controller>(
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv_data = [0u8; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids128(&[SERVICE_UUID_128]),
        ],
        &mut adv_data[..],
    )?;

    let mut scan_data = [0u8; 31];
    let scan_len = AdStructure::encode_slice(
        &[AdStructure::CompleteLocalName(BLE_DEVICE_NAME.as_bytes())],
        &mut scan_data[..],
    )?;

    let advertiser = peripheral
        .advertise(
            &AdvertisementParameters::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..len],
                scan_data: &scan_data[..scan_len],
            },
        )
        .await?;

    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    Ok(conn)
}

/// 处理读写事件，并在空闲时驱动 OTA 超时检查。
async fn gatt_events_task<P: PacketPool>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    led_channel: &LedChannel,
    ota: &mut OtaMachine,
    current_mode: &mut Mode,
    flash: &mut FlashStorage<'_>,
) -> Result<(), Error> {
    let mode_handle = server.light.mode.handle;
    let ctrl_handle = server.ota.ctrl.handle;
    let data_handle = server.ota.data.handle;

    loop {
        match select(conn.next(), Timer::after(Duration::from_secs(1))).await {
            Either::First(GattConnectionEvent::Disconnected { reason }) => {
                info!("[gatt] disconnected: {:?}", reason);
                return Ok(());
            }
            Either::First(GattConnectionEvent::Gatt { event }) => match event {
                GattEvent::Read(e) => {
                    if let Ok(reply) = e.accept() {
                        reply.send().await;
                    }
                }
                GattEvent::Write(e) => {
                    let handle = e.handle();

                    if handle == mode_handle {
                        let mut raw: HVec<u8, MODE_VALUE_MAX> = HVec::new();
                        let _ = raw.extend_from_slice(e.data());
                        match e.accept() {
                            Ok(reply) => reply.send().await,
                            Err(err) => warn!("[gatt] mode write rejected: {:?}", err),
                        }
                        match core::str::from_utf8(&raw).ok().and_then(Mode::from_str) {
                            Some(m) => {
                                // 先发送 LED 命令，成功后再更新 current_mode，
                                // 避免通道满时状态不同步
                                if led_channel.try_send(LedCmd::Mode(m)).is_ok() {
                                    *current_mode = m;
                                } else {
                                    warn!("LED channel full, mode {} dropped", m.as_str());
                                }
                                notify_mode(server, conn, m).await;
                            }
                            None => warn!("Unknown mode: {:?}", raw.as_slice()),
                        }
                    } else if handle == ctrl_handle {
                        let outcome = ota.on_ctrl(flash, e.data());
                        if let Ok(reply) = e.accept() {
                            reply.send().await;
                        }
                        handle_ctrl_outcome(outcome, server, conn, led_channel, ota).await;
                    } else if handle == data_handle {
                        let progress = ota.on_data(flash, e.data());
                        if let Ok(reply) = e.accept() {
                            reply.send().await;
                        }
                        if progress {
                            notify_status(server, conn, ota).await;
                        }
                    } else if let Ok(reply) = e.accept() {
                        reply.send().await;
                    }
                }
                other => {
                    if let Ok(reply) = other.accept() {
                        reply.send().await;
                    }
                }
            },
            Either::First(_) => {}
            Either::Second(_) => {
                if ota.on_tick() {
                    warn!("OTA idle timeout");
                    notify_status(server, conn, ota).await;
                    let _ = led_channel.try_send(LedCmd::OtaResume);
                }
            }
        }
    }
}

async fn handle_ctrl_outcome<P: PacketPool>(
    outcome: CtrlOutcome,
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    led_channel: &LedChannel,
    ota: &OtaMachine,
) {
    match outcome {
        CtrlOutcome::Started => {
            let _ = led_channel.try_send(LedCmd::OtaSuspend);
            notify_status(server, conn, ota).await;
        }
        CtrlOutcome::Committed => {
            notify_status(server, conn, ota).await;
            Timer::after(Duration::from_millis(500)).await;
            info!("OTA: rebooting into new partition");
            esp_hal::system::software_reset();
        }
        CtrlOutcome::Aborted | CtrlOutcome::Rejected => {
            let _ = led_channel.try_send(LedCmd::OtaResume);
            notify_status(server, conn, ota).await;
        }
    }
}

async fn notify_mode<P: PacketPool>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    mode: Mode,
) {
    let mut value: HString<MODE_VALUE_MAX> = HString::new();
    let _ = value.push_str(mode.as_str());
    if let Err(e) = server.light.mode.notify(conn, &value).await {
        warn!("notify mode failed: {:?}", e);
    }
}

async fn notify_status<P: PacketPool>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    ota: &OtaMachine,
) {
    let text = core::str::from_utf8(ota.status()).unwrap_or("error:utf8");
    let mut value: HString<OTA_STATUS_MAX> = HString::new();
    let _ = value.push_str(text);
    if let Err(e) = server.ota.status.notify(conn, &value).await {
        warn!("notify status failed: {:?}", e);
    }
}
