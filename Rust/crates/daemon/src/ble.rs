// BLE 通信模块 — 管理与 ESP32 的蓝牙连接
//
// 核心职责：
// 1. 扫描并连接 ESP32 设备
// 2. 保持长连接，自动重连（指数退避）
// 3. 发送灯效模式指令（Mode → BLE characteristic write）
// 4. 带 debounce 的发送逻辑，避免高频重复写入
//
// 移植自 ble_daemon.py 的 PersistentBLE 类

use btleplug::api::{
    Central, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::time::sleep;
use uuid::Uuid;

use crate::types::{BleResult, Mode};
use crate::log;

// ---------------------------------------------------------------------------
// 常量 — 与 ble_daemon.py 对齐
// ---------------------------------------------------------------------------

/// BLE 设备名（ESP32 广播名）
pub const DEVICE_NAME: &str = "CursorLight";

/// BLE GATT Service UUID
pub const SERVICE_UUID: &str = "b8b7e001-7a6b-4f4f-9a8b-11c0ffee0001";

/// BLE Mode Characteristic UUID — 写入模式字符串的目标特征值
pub const MODE_CHAR_UUID: &str = "b8b7e002-7a6b-4f4f-9a8b-11c0ffee0001";

// ---- 连接配置 ----

/// 单次 BLE 连接超时（秒）— Windows WinRT 首次连接常 >4s
pub const BLE_TIMEOUT_SECS: u64 = 12;

/// 断线重连起始间隔（毫秒）— 断开后先等 2s，给两端 BLE 栈稳定时间
pub const RECONNECT_DELAY_MIN_MS: u64 = 2000;

/// 断线重连最大间隔（毫秒）— 指数退避上限
pub const RECONNECT_DELAY_MAX_MS: u64 = 15000;

/// 设备完全搜不到时的重扫间隔（毫秒）— 不密集扫描，避免打挂 ESP32 BLE 栈
pub const SCAN_RETRY_INTERVAL_MS: u64 = 20000;

/// BLE 扫描持续时间（秒）
const SCAN_DURATION_SECS: u64 = 8;

/// GATT 写入超时（秒）
const WRITE_TIMEOUT_SECS: u64 = 3;

/// 连接后等待 GATT 服务就绪的延迟
const GATT_READY_DELAY_MS: u64 = 200;

/// 连接重试次数
const CONNECT_MAX_RETRIES: u32 = 2;

/// 连续失败后强制重扫的阈值
const FORCE_RESCAN_FAILURES: u32 = 3;

/// BLE 任务空闲等待上限（毫秒）。
///
/// 主循环是事件驱动的：有新期望模式时立刻唤醒；只有空闲/重连时才等到这个上限。
/// 旧实现固定 sleep(150ms) 轮询，每条命令平均白等 75ms。
const IDLE_WAIT_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// Debounce 配置 — 从 ble_daemon.py DEBOUNCE_MS 移植
// ---------------------------------------------------------------------------

/// 根据模式返回 debounce 间隔（毫秒）
/// 不同模式有不同的防抖时间：alarm 需要快速响应，其他模式尽量快
pub fn debounce_ms(mode: Mode) -> u64 {
    // 注意：debounce 会直接叠加到端到端延迟上，所以必须低于 BLE 一次写入的
    // 物理耗时（实测 gatt 约 100ms）。这里取 0~120ms：
    // 既能压掉同一次 hook 里背靠背的重复切换，又不会让人眼察觉到延迟。
    match mode {
        Mode::Alarm => 0,                // 警灯：立刻点亮，零等待
        Mode::Red | Mode::Yellow => 60,  // 错误/警告
        Mode::Green | Mode::Off => 120,  // 稳定状态
        _ => 90,                         // thinking / busy / success / error
    }
}

/// alarm 退出时的 debounce = 0（立即发送，零等待）
pub const ALARM_EXIT_DEBOUNCE_MS: u64 = 0;

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 解析 SERVICE_UUID 为 Uuid
fn service_uuid() -> Uuid {
    Uuid::parse_str(SERVICE_UUID).expect("invalid SERVICE_UUID")
}

/// 解析 MODE_CHAR_UUID 为 Uuid
fn mode_char_uuid() -> Uuid {
    Uuid::parse_str(MODE_CHAR_UUID).expect("invalid MODE_CHAR_UUID")
}

/// 当前时间戳（毫秒）
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 指数退避计算：min(RECONNECT_DELAY_MIN * 2^failures, RECONNECT_DELAY_MAX)
fn backoff_ms(failures: u32) -> u64 {
    let delay = RECONNECT_DELAY_MIN_MS.saturating_mul(1u64 << failures.min(20));
    delay.min(RECONNECT_DELAY_MAX_MS)
}

// ---------------------------------------------------------------------------
// BleDaemon — BLE 长连接守护（移植自 Python PersistentBLE）
// ---------------------------------------------------------------------------

/// BLE 守护进程：长连接 + 自动重连 + debounce 发送
///
/// 核心状态：
/// - peripheral: 当前连接的 BLE 设备
/// - address: 缓存的设备地址，用于快速重连
/// - connected: 连接状态标志
/// - consecutive_failures: 连续失败计数，控制退避策略
/// - last_sent_mode / last_sent_ts: debounce 状态
pub struct BleDaemon {
    manager: Manager,
    adapter: Option<Adapter>,
    peripheral: Option<Peripheral>,
    address: Option<btleplug::api::BDAddr>,
    connected: bool,
    consecutive_failures: u32,
    last_sent_mode: Option<Mode>,
    last_sent_ts: u64,
}

impl BleDaemon {
    /// 创建新的 BleDaemon 实例
    ///
    /// 初始化 BLE Manager，但不立即连接。
    /// 后续调用 connect() 或 connect_cached() 建立连接。
    pub async fn new() -> anyhow::Result<Self> {
        let manager = Manager::new().await?;
        Ok(Self {
            manager,
            adapter: None,
            peripheral: None,
            address: None,
            connected: false,
            consecutive_failures: 0,
            last_sent_mode: None,
            last_sent_ts: 0,
        })
    }

    /// 获取默认 BLE adapter
    async fn get_adapter(&mut self) -> anyhow::Result<Adapter> {
        if let Some(ref adapter) = self.adapter {
            return Ok(adapter.clone());
        }
        let adapters = self.manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No BLE adapter found"))?;
        self.adapter = Some(adapter.clone());
        Ok(adapter)
    }

    /// 扫描并连接到 CursorLight
    ///
    /// 1. 用 btleplug 扫描 SCAN_DURATION_SECS 秒
    /// 2. 找到 name=="CursorLight" 的设备
    /// 3. 连接 → 发现服务 → 找到 mode characteristic
    /// 返回是否连接成功
    pub async fn connect(&mut self) -> bool {
        log::info(&format!("Scanning for {} ...", DEVICE_NAME));

        let adapter = match self.get_adapter().await {
            Ok(a) => a,
            Err(e) => {
                log::error(&format!("BLE adapter error: {}", e));
                return false;
            }
        };

        // 启动扫描
        if let Err(e) = adapter.start_scan(ScanFilter::default()).await {
            log::error(&format!("BLE scan start error: {}", e));
            return false;
        }

        // 等待扫描期间收集结果
        let scan_deadline =
            std::time::Instant::now() + Duration::from_secs(SCAN_DURATION_SECS);
        let mut found_addr: Option<btleplug::api::BDAddr> = None;

        while std::time::Instant::now() < scan_deadline {
            // 遍历已发现的 peripherals，按名称匹配
            if let Ok(peripherals) = adapter.peripherals().await {
                for p in peripherals {
                    if let Ok(Some(props)) = p.properties().await {
                        if let Some(ref name) = props.local_name {
                            if name == DEVICE_NAME {
                                found_addr = Some(props.address);
                                break;
                            }
                        }
                    }
                }
            }
            if found_addr.is_some() {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }

        // 停止扫描
        let _ = adapter.stop_scan().await;

        let addr = match found_addr {
            Some(a) => a,
            None => {
                log::info("Device not found");
                return false;
            }
        };

        self.address = Some(addr);
        log::info(&format!("Found: {}", addr));

        self.do_connect(addr).await
    }

    /// 用缓存地址直连（跳过扫描）
    ///
    /// 当已有设备地址时，直接发起连接，比扫描快得多。
    /// 用于断线重连场景。
    pub async fn connect_cached(&mut self, addr: btleplug::api::BDAddr) -> bool {
        self.address = Some(addr);
        self.do_connect(addr).await
    }

    /// 内部连接实现（移植自 Python _do_connect）
    ///
    /// 1. 通过 adapter 找到指定地址的 peripheral
    /// 2. 建立 BLE 连接
    /// 3. 发现 GATT 服务
    /// 4. 验证 mode characteristic 存在
    /// 最多重试 CONNECT_MAX_RETRIES 次
    async fn do_connect(&mut self, addr: btleplug::api::BDAddr) -> bool {
        let adapter = match self.get_adapter().await {
            Ok(a) => a,
            Err(e) => {
                log::error(&format!("BLE adapter error: {}", e));
                return false;
            }
        };

        for attempt in 0..CONNECT_MAX_RETRIES {
            log::info(&format!(
                "BLE do_connect attempt {}/{} to {}",
                attempt + 1,
                CONNECT_MAX_RETRIES,
                addr
            ));

            // 扫描一下确保设备可见（btleplug 需要先扫描才能发现 peripheral）
            let _ = adapter.start_scan(ScanFilter::default()).await;
            sleep(Duration::from_secs(2)).await;
            let _ = adapter.stop_scan().await;

            // 按地址查找 peripheral
            let peripherals = match adapter.peripherals().await {
                Ok(p) => p,
                Err(e) => {
                    log::error(&format!("BLE enumerate error: {}", e));
                    continue;
                }
            };

            log::info(&format!(
                "BLE scan found {} peripherals, looking for {}",
                peripherals.len(),
                addr
            ));

            let mut target = None;
            for p in peripherals {
                if let Ok(Some(props)) = p.properties().await {
                    if props.address == addr {
                        target = Some(p);
                        break;
                    }
                }
            }

            let peripheral = match target {
                Some(p) => p,
                None => {
                    log::info(&format!("ESP32 {} not found in scan results", addr));
                    if attempt < CONNECT_MAX_RETRIES - 1 {
                        sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            };

            // 建立 BLE 连接（带超时）
            log::info(&format!("ESP32 connecting to {} ...", addr));
            match tokio::time::timeout(
                Duration::from_secs(BLE_TIMEOUT_SECS),
                peripheral.connect(),
            )
            .await
            {
                Ok(Ok(())) => {
                    log::info(&format!("ESP32 link established to {}", addr));
                }
                Ok(Err(e)) => {
                    log::error(&format!(
                        "ESP32 connect error (attempt {}/{}): {}",
                        attempt + 1,
                        CONNECT_MAX_RETRIES,
                        e
                    ));
                    if attempt < CONNECT_MAX_RETRIES - 1 {
                        sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
                Err(_) => {
                    log::error(&format!(
                        "ESP32 connect timeout after {}s (attempt {}/{})",
                        BLE_TIMEOUT_SECS,
                        attempt + 1,
                        CONNECT_MAX_RETRIES
                    ));
                    if attempt < CONNECT_MAX_RETRIES - 1 {
                        sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            }

            // 等待 GATT 服务就绪（Windows BLE 缓存可能需要时间）
            sleep(Duration::from_millis(GATT_READY_DELAY_MS)).await;

            // 验证特征值存在 — 发现服务并检查 mode characteristic
            log::info("ESP32 discovering GATT services ...");
            match peripheral.discover_services().await {
                Ok(_) => {
                    let chars = peripheral.characteristics();
                    let mode_char = mode_char_uuid();
                    let has_mode_char = chars.iter().any(|c| c.uuid == mode_char);

                    log::info(&format!(
                        "ESP32 GATT: {} services discovered, {} characteristics total, mode_char={} present={}",
                        peripheral.services().len(),
                        chars.len(),
                        mode_char,
                        has_mode_char
                    ));

                    if !has_mode_char {
                        log::info(&format!(
                            "ESP32 GATT mode characteristic not found, retry {}/{}",
                            attempt + 1,
                            CONNECT_MAX_RETRIES
                        ));
                        let _ = peripheral.disconnect().await;
                        if attempt < CONNECT_MAX_RETRIES - 1 {
                            sleep(Duration::from_millis(500)).await;
                        }
                        continue;
                    }
                }
                Err(e) => {
                    log::error(&format!(
                        "ESP32 service discovery error, retry {}/{}: {}",
                        attempt + 1,
                        CONNECT_MAX_RETRIES,
                        e
                    ));
                    let _ = peripheral.disconnect().await;
                    if attempt < CONNECT_MAX_RETRIES - 1 {
                        sleep(Duration::from_millis(500)).await;
                    }
                    continue;
                }
            }

            // 连接成功
            self.peripheral = Some(peripheral);
            self.connected = true;
            log::info(&format!("ESP32 fully connected: {} (ready for GATT writes)", addr));
            return true;
        }

        self.connected = false;
        log::error(&format!("ESP32 connection failed after {} attempts to {}", CONNECT_MAX_RETRIES, addr));
        false
    }

    /// GATT 写入模式字符串
    ///
    /// 将 mode 转为 UTF-8 字节，写入 MODE_CHAR_UUID 特征值。
    /// 使用 WriteType::WithResponse 确保写入确认。
    /// 写入失败时主动断开残留连接，避免半开连接。
    ///
    /// 日志输出写入详情（模式名、hex 字节、UUID、耗时），方便排查 ESP32 通信问题。
    pub async fn write_mode(&mut self, mode: Mode) -> BleResult {
        let peripheral = match self.peripheral.as_ref() {
            Some(p) => p,
            None => {
                log::error(&format!("BLE write skipped: no peripheral (mode={})", mode));
                return BleResult::Disconnected;
            }
        };

        if !self.connected {
            log::error(&format!("BLE write skipped: not connected (mode={})", mode));
            return BleResult::Disconnected;
        }

        // 查找 mode characteristic
        let mode_char = mode_char_uuid();
        let chars = peripheral.characteristics();
        let characteristic = match chars.iter().find(|c| c.uuid == mode_char) {
            Some(c) => c.clone(),
            None => {
                log::error(&format!(
                    "Mode characteristic {} not found in GATT table ({} characteristics available)",
                    mode_char,
                    chars.len()
                ));
                return BleResult::Error("Mode characteristic not found".into());
            }
        };

        let data = mode.as_str().as_bytes();
        let hex_str: String = data.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");

        // 记录写入详情：模式、字节内容、目标 UUID
        log::info(&format!(
            "BLE GATT write: mode=\"{}\" data=[{}] len={} char={}",
            mode, hex_str, data.len(), mode_char
        ));

        let write_start = std::time::Instant::now();

        // 带超时的 GATT 写入（WithResponse 等待 ESP32 ACK）
        let result = tokio::time::timeout(
            Duration::from_secs(WRITE_TIMEOUT_SECS),
            peripheral.write(&characteristic, data, WriteType::WithResponse),
        )
        .await;

        let elapsed_ms = write_start.elapsed().as_millis();

        match result {
            Ok(Ok(())) => {
                log::info(&format!(
                    "BLE GATT write OK: mode=\"{}\" elapsed={}ms",
                    mode, elapsed_ms
                ));
                BleResult::Ok
            }
            Ok(Err(e)) => {
                log::error(&format!(
                    "BLE GATT write FAILED: mode=\"{}\" elapsed={}ms error={}",
                    mode, elapsed_ms, e
                ));
                self.force_disconnect().await;
                BleResult::Error(e.to_string())
            }
            Err(_) => {
                log::error(&format!(
                    "BLE GATT write TIMEOUT: mode=\"{}\" elapsed={}ms timeout={}s",
                    mode, elapsed_ms, WRITE_TIMEOUT_SECS
                ));
                self.force_disconnect().await;
                BleResult::Error("Write timeout".into())
            }
        }
    }

    /// 读取 ESP32 当前显示的模式（用于连接后对账/诊断）。
    ///
    /// mode 特征值是 `read, write, notify`，固件在 RAM 里保留 current_mode，
    /// 所以重连后读一次就能知道两端是否已经不一致。
    pub async fn read_mode(&mut self) -> Option<Mode> {
        let peripheral = self.peripheral.as_ref()?;
        if !self.connected {
            return None;
        }

        let char_uuid = mode_char_uuid();
        let characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == char_uuid)?;

        let result = tokio::time::timeout(
            Duration::from_secs(WRITE_TIMEOUT_SECS),
            peripheral.read(&characteristic),
        )
        .await;

        match result {
            Ok(Ok(data)) => {
                let text = String::from_utf8_lossy(&data).trim().to_string();
                match Mode::from_str(&text) {
                    Some(m) => Some(m),
                    None => {
                        log::warn(&format!("ESP32 reported unknown mode: {:?}", text));
                        None
                    }
                }
            }
            Ok(Err(e)) => {
                log::warn(&format!("BLE read mode failed: {}", e));
                None
            }
            Err(_) => {
                log::warn("BLE read mode timeout");
                None
            }
        }
    }

    /// 带 debounce 的写入 — 核心方法
    ///
    /// Debounce 逻辑（移植自 ble_daemon.py）：
    /// - 如果 mode == last_sent_mode → 不重复发送，返回 true（视为成功）
    /// - 如果从 alarm 退出 → debounce = ALARM_EXIT_DEBOUNCE_MS（立即发送）
    /// - 如果距离上次发送 < debounce_ms(mode) → 跳过，返回 false
    /// - 发送成功 → 更新 last_sent_mode, last_sent_ts，返回 true
    /// - 发送失败 → 连接断开，返回 false
    pub async fn send_debounced(&mut self, mode: Mode, recv_at: Instant) -> bool {
        // 相同模式不重复发送
        if self.last_sent_mode == Some(mode) {
            return true;
        }

        let now = now_ms();

        // 计算 debounce 间隔
        let debounce = if self.last_sent_mode == Some(Mode::Alarm) && mode != Mode::Alarm {
            // alarm 退出零等待
            ALARM_EXIT_DEBOUNCE_MS
        } else {
            debounce_ms(mode)
        };

        // 检查 debounce 间隔。
        //
        // 注意：这里必须"等满 debounce 再发"，不能直接 return 交给主循环重试。
        // 主循环是事件驱动的（无新事件时最多等 1s），直接返回会让这次模式变化
        // 被拖到 ~1s 之后才发出去（实测 SessionEnd 的 green 花了 898ms）。
        // 内联等满后立刻发，延迟上限就是 debounce 本身（0~120ms）。
        let elapsed = now.saturating_sub(self.last_sent_ts);
        if elapsed < debounce {
            let remaining_ms = debounce - elapsed;
            if remaining_ms > 500 {
                log::info(&format!("Debounce: {} waiting {}ms", mode, remaining_ms));
            }
            sleep(Duration::from_millis(remaining_ms)).await;
        }

        // 发送
        let addr_str = self.address.map(|a| a.to_string()).unwrap_or_else(|| "unknown".into());
        log::info(&format!("BLE send: {} → ESP32 {}", mode, addr_str));
        let write_start = Instant::now();
        let result = self.write_mode(mode).await;
        let gatt_ms = write_start.elapsed().as_millis();
        match result {
            BleResult::Ok => {
                self.last_sent_ts = now;
                self.last_sent_mode = Some(mode);
                self.consecutive_failures = 0;
                // 端到端延迟：命令进入状态机（recv_at）→ ESP32 确认写入。
                // 特意用纯 ASCII，便于脚本解析（控制台非 UTF-8 时 → 会变乱码）。
                log::info(&format!(
                    "Latency: mode={} hook_to_ble={}ms gatt={}ms",
                    mode,
                    recv_at.elapsed().as_millis(),
                    gatt_ms
                ));
                true
            }
            _ => {
                log::error(&format!("BLE fail: {} ESP32 write failed, will reconnect (addr={})", mode, addr_str));
                self.consecutive_failures += 1;
                false
            }
        }
    }

    /// 强制断开连接（内部用，写入失败后调用）
    ///
    /// 忽略断开过程中的错误，确保状态被重置。
    async fn force_disconnect(&mut self) {
        let addr = self.address.map(|a| a.to_string()).unwrap_or_else(|| "unknown".into());
        log::info(&format!("ESP32 force disconnect from {} (cleanup after write failure)", addr));
        if let Some(ref peripheral) = self.peripheral {
            let _ = peripheral.disconnect().await;
        }
        self.connected = false;
        log::info(&format!("ESP32 disconnected (address cached for reconnect: {})", addr));
    }

    /// 断开连接（公开接口）
    pub async fn disconnect(&mut self) {
        let addr = self.address.map(|a| a.to_string()).unwrap_or_else(|| "unknown".into());
        if self.connected {
            log::info(&format!("ESP32 disconnecting from {} ...", addr));
            if let Some(ref peripheral) = self.peripheral {
                let _ = peripheral.disconnect().await;
            }
            log::info(&format!("ESP32 disconnected: {}", addr));
        }
        self.connected = false;
        self.peripheral = None;
    }

    /// 是否已连接
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// 获取缓存地址（用于重连）
    pub fn cached_address(&self) -> Option<btleplug::api::BDAddr> {
        self.address
    }

    /// 重置缓存地址（连续失败后强制重扫）
    pub fn clear_cached_address(&mut self) {
        self.address = None;
    }

    /// 获取连续失败次数
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

// ---------------------------------------------------------------------------
// BLE 守护进程主循环任务
// ---------------------------------------------------------------------------

/// BLE 守护进程主循环
///
/// 接收 mode 命令，维护连接，发送 BLE 数据。
/// 与 main.rs 的状态机通过 watch channel 通信（只保留最新期望模式）。
///
/// 重连逻辑（移植自 ble_daemon.py daemon_loop）：
/// - 有缓存地址：指数退避直连，连续失败 3 次后改走扫描
/// - 无地址：固定 20s 间隔重扫
/// - 退避公式：min(RECONNECT_DELAY_MIN * 2^failures, RECONNECT_DELAY_MAX)
pub async fn ble_task(rx: &mut watch::Receiver<(Mode, Instant)>, mut ble: BleDaemon) {
    log::info("BLE task starting (persistent connection mode)");

    // 1. 初始连接（扫描方式）
    if !ble.connect().await {
        log::info("Initial connection failed, will retry in loop");
    }

    // 2. 主循环
    let mut reconnect_cooldown: u64 = 0;

    loop {
        // ---- 事件驱动等待 ----
        // 有新期望模式时立刻唤醒（旧实现固定 sleep(150ms) 轮询，平均白等 75ms）；
        // 空闲时最多等 IDLE_WAIT_MS，未连接时按重连退避剩余时间等待。
        let wait_ms: u64 = if ble.is_connected() {
            IDLE_WAIT_MS
        } else {
            reconnect_cooldown.saturating_sub(now_ms()).clamp(20, IDLE_WAIT_MS)
        };
        match tokio::time::timeout(Duration::from_millis(wait_ms), rx.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                log::info("Mode channel closed, BLE task exiting");
                return;
            }
            Err(_) => {} // 超时：继续走重连 / 心跳
        }

        // watch 只保留"最新期望模式"：
        // - 断连期间的 hook 事件不会丢（旧实现会把抽出来的命令直接扔掉）
        // - 每次循环都拿到当前真值，重连后必然重发
        let (desired, recv_at) = *rx.borrow_and_update();
        let mut need_resync = false;

        let now = now_ms();

        // ---- BLE 连接管理 ----
        if !ble.is_connected() {
            // ESP32 断开，进入重连流程
            if now >= reconnect_cooldown {
                if let Some(addr) = ble.cached_address() {
                    // 有缓存地址：指数退避直连
                    let failures = ble.consecutive_failures();
                    let backoff = backoff_ms(failures);
                    reconnect_cooldown = now + backoff;

                    log::info(&format!(
                        "Reconnecting to {} (backoff={:.0}s, fails={}) ...",
                        addr,
                        backoff as f64 / 1000.0,
                        failures
                    ));

                    let success = ble.connect_cached(addr).await;
                    if success {
                        ble.consecutive_failures = 0;
                        log::info(&format!("ESP32 reconnected successfully to {}", addr));
                        // 重置 debounce，保证当前模式会被无条件重发
                        ble.last_sent_mode = None;
                        ble.last_sent_ts = 0;
                        need_resync = true;
                    } else {
                        ble.consecutive_failures += 1;
                        log::error(&format!("ESP32 reconnect FAILED to {} (total failures: {})", addr, ble.consecutive_failures()));
                        // 连续失败多次后放弃缓存地址，改走扫描
                        if ble.consecutive_failures() >= FORCE_RESCAN_FAILURES {
                            log::info("Cached address failing repeatedly, forcing re-scan ...");
                            ble.clear_cached_address();
                            ble.consecutive_failures = 0;
                        }
                    }
                } else {
                    // 无地址：固定长间隔重扫（设备断电/未开机时不密集扫描）
                    reconnect_cooldown = now + SCAN_RETRY_INTERVAL_MS;
                    log::info(&format!(
                        "Scanning for device (interval={:.0}s) ...",
                        SCAN_RETRY_INTERVAL_MS as f64 / 1000.0
                    ));
                    let scan_ok = ble.connect().await;
                    if scan_ok {
                        log::info("ESP32 found via full scan and connected");
                        // 重置 debounce，保证当前模式会被无条件重发
                        ble.last_sent_mode = None;
                        ble.last_sent_ts = 0;
                        need_resync = true;
                    } else {
                        log::info("ESP32 scan complete, device not found");
                    }
                }
            }
            // 重连仍失败 → 回到循环顶部按退避等待（节流交给事件驱动等待）
            if !ble.is_connected() {
                continue;
            }
        }

        // ---- 连接建立后的对账 ----
        if need_resync {
            if let Some(remote) = ble.read_mode().await {
                if remote != desired {
                    log::info(&format!(
                        "ESP32 shows {} but daemon wants {} — resyncing",
                        remote, desired
                    ));
                }
            }
        }

        // ---- 发送最新期望模式（debounce 内部会跳过相同模式）----
        let _ = ble.send_debounced(desired, recv_at).await;
    }
}

/// BLE 子系统监督者。
///
/// 初始化失败或任务意外退出都会自动重启 —— 旧实现里 BLE 初始化一失败
/// 就永久降级成"只记日志"，灯再也不会亮，直到人工重启 daemon。
pub async fn ble_supervisor(mut rx: watch::Receiver<(Mode, Instant)>) {
    loop {
        match BleDaemon::new().await {
            Ok(ble) => {
                log::info("BLE subsystem started");
                ble_task(&mut rx, ble).await;
                log::warn("BLE task exited unexpectedly, restarting in 5s");
                sleep(Duration::from_secs(5)).await;
            }
            Err(e) => {
                log::error(&format!("BLE init failed: {} (retry in 30s)", e));
                sleep(Duration::from_secs(30)).await;
            }
        }
    }
}
