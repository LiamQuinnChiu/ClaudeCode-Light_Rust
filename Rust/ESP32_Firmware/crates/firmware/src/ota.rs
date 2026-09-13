//! 固件端 OTA：接收分块 → 写入非当前槽 → SHA-256 校验 → 切换 otadata → 重启。
//!
//! 写入的目标始终是当前未运行的槽，因此传输中断或校验失败都不会影响正在运行的固件。
//! 校验通过后擦除 otadata 扇区并一次性写入两个条目（目标槽新序号、另一槽旧序号）。

use core::fmt::Write as _;

use embedded_storage::nor_flash::NorFlash;
use embedded_storage::{ReadStorage, Storage};
use embassy_time::Instant;
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PartitionEntry, PartitionTable, PartitionType,
};
use esp_storage::FlashStorage;
use heapless::String as HString;
use log::{info, warn};
use sha2::{Digest, Sha256};

use light_core::otadata::OtaSelectEntry;

const PARTITION_TABLE_MAX_LEN: usize = 0xC00;
const WRITE_ALIGN: u32 = 4;
const SECTOR_SIZE: u32 = 4096;
const OTADATA_ENTRY_LEN: usize = 32;
const IDLE_TIMEOUT_MS: u64 = 10_000;
const PROGRESS_STEP: u32 = 32 * 1024;

/// OTA 访问密码。start 命令格式：`start:<size>:<sha256>:<password>`
/// 密码为空则跳过验证（向后兼容旧格式）。
const OTA_PASSWORD: &str = "cursorlight-ota";

pub const STATUS_MAX: usize = 64;

/// `on_ctrl()` 的处理结果。
#[derive(Debug, PartialEq, Eq)]
pub enum CtrlOutcome {
    Started,
    Committed,
    Aborted,
    Rejected,
}

pub struct OtaMachine {
    active: bool,
    dst_offset: u32,
    dst_size: u32,
    /// 目标槽在 otadata 中的条目下标（0 或 1）。
    dst_index: usize,
    next_addr: u32,
    erased_upto: u32,
    total: u32,
    received: u32,
    /// 不足 4 字节的尾部缓冲。
    pending: [u8; WRITE_ALIGN as usize],
    pending_len: usize,
    sha: Sha256,
    expected: [u8; 32],
    status: HString<STATUS_MAX>,
    last_activity: Instant,
    progress_mark: u32,
}

impl OtaMachine {
    pub fn new() -> Self {
        OtaMachine {
            active: false,
            dst_offset: 0,
            dst_size: 0,
            dst_index: 0,
            next_addr: 0,
            erased_upto: 0,
            total: 0,
            received: 0,
            pending: [0; WRITE_ALIGN as usize],
            pending_len: 0,
            sha: Sha256::new(),
            expected: [0; 32],
            status: HString::new(),
            last_activity: Instant::now(),
            progress_mark: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn status(&self) -> &[u8] {
        self.status.as_bytes()
    }

    pub fn on_ctrl(&mut self, flash: &mut FlashStorage<'_>, data: &[u8]) -> CtrlOutcome {
        let cmd = match core::str::from_utf8(data) {
            Ok(s) => trim_ascii(s),
            Err(_) => {
                self.set_status("error:utf8");
                return CtrlOutcome::Rejected;
            }
        };
        info!("OTA ctrl: {}", cmd);

        if let Some(rest) = cmd.strip_prefix("start:") {
            return self.start(flash, rest);
        }
        match cmd {
            "commit" => self.commit(flash),
            "cancel" => {
                self.active = false;
                self.pending_len = 0;
                self.set_status("cancelled");
                CtrlOutcome::Aborted
            }
            _ => {
                self.set_status("error:cmd");
                CtrlOutcome::Rejected
            }
        }
    }

    /// 返回 `true` 表示建议推送一次状态通知。
    pub fn on_data(&mut self, flash: &mut FlashStorage<'_>, data: &[u8]) -> bool {
        if !self.active || data.is_empty() {
            return false;
        }
        self.last_activity = Instant::now();
        self.received = self.received.saturating_add(data.len() as u32);
        self.sha.update(data);

        if self.write_bytes(flash, data).is_err() {
            self.active = false;
            self.set_status("error:flash");
            return true;
        }

        if self.received.saturating_sub(self.progress_mark) >= PROGRESS_STEP {
            self.progress_mark = self.received;
            self.status.clear();
            let _ = write!(self.status, "progress:{}:{}", self.received, self.total);
            return true;
        }
        false
    }

    /// 空闲超时则中止。
    pub fn on_tick(&mut self) -> bool {
        if self.active && self.last_activity.elapsed().as_millis() >= IDLE_TIMEOUT_MS {
            self.active = false;
            self.pending_len = 0;
            self.set_status("error:timeout");
            true
        } else {
            false
        }
    }

    fn set_status(&mut self, s: &str) {
        self.status.clear();
        let _ = self.status.push_str(s);
    }

    fn start(&mut self, flash: &mut FlashStorage<'_>, rest: &str) -> CtrlOutcome {
        let mut parts = rest.split(':');
        let size: u32 = match parts.next().and_then(|s| s.parse::<u32>().ok()) {
            Some(v) => v,
            None => {
                self.set_status("error:size");
                return CtrlOutcome::Rejected;
            }
        };
        let expected = match parts.next().and_then(hex_to_32) {
            Some(v) => v,
            None => {
                self.set_status("error:sha");
                return CtrlOutcome::Rejected;
            }
        };

        // 密码验证（第三段，可选）
        if !OTA_PASSWORD.is_empty() {
            let pwd = parts.next().unwrap_or("");
            if pwd != OTA_PASSWORD {
                warn!("OTA: authentication failed (wrong password)");
                self.set_status("error:auth");
                return CtrlOutcome::Rejected;
            }
        }

        let mut table_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let table = match partitions::read_partition_table(flash, &mut table_buf) {
            Ok(t) => t,
            Err(e) => {
                warn!("OTA: partition table read failed: {:?}", e);
                self.set_status("error:table");
                return CtrlOutcome::Rejected;
            }
        };

        let (target, index) = match select_target_slot(&table) {
            Some(v) => v,
            None => {
                warn!("OTA: no writable OTA slot");
                self.set_status("error:slot");
                return CtrlOutcome::Rejected;
            }
        };

        if size == 0 || size > target.len() {
            self.set_status("error:toolarge");
            return CtrlOutcome::Rejected;
        }

        self.active = true;
        self.dst_offset = target.offset();
        self.dst_size = target.len();
        self.dst_index = index;
        self.next_addr = target.offset();
        self.erased_upto = target.offset();
        self.total = size;
        self.received = 0;
        self.pending_len = 0;
        self.progress_mark = 0;
        self.sha = Sha256::new();
        self.expected = expected;
        self.last_activity = Instant::now();
        self.set_status("ready:512");

        info!(
            "OTA start: slot ota_{} offset={:#x} size={}",
            index, self.dst_offset, size
        );
        CtrlOutcome::Started
    }

    fn commit(&mut self, flash: &mut FlashStorage<'_>) -> CtrlOutcome {
        if !self.active {
            self.set_status("error:idle");
            return CtrlOutcome::Rejected;
        }

        // 尾部补足到 4 字节对齐后落盘
        if self.pending_len > 0 {
            for i in self.pending_len..WRITE_ALIGN as usize {
                self.pending[i] = 0xFF;
            }
            let tail = self.pending;
            if self.raw_write(flash, &tail).is_err() {
                self.active = false;
                self.set_status("error:flash");
                return CtrlOutcome::Aborted;
            }
            self.pending_len = 0;
        }

        if self.received != self.total {
            warn!("OTA: length mismatch {} != {}", self.received, self.total);
            self.active = false;
            self.set_status("error:size");
            return CtrlOutcome::Aborted;
        }

        let got = self.sha.clone().finalize();
        if got.as_slice() != self.expected {
            warn!("OTA: SHA-256 mismatch");
            self.active = false;
            self.set_status("error:hash");
            return CtrlOutcome::Aborted;
        }
        info!("OTA: verified, switching partition");

        match self.switch_slot(flash) {
            Ok(()) => {
                self.active = false;
                self.set_status("done");
                CtrlOutcome::Committed
            }
            Err(reason) => {
                warn!("OTA: otadata switch failed: {}", reason);
                self.active = false;
                self.set_status("error:otadata");
                CtrlOutcome::Aborted
            }
        }
    }

    /// 先补齐 4 字节对齐再整块写，尾部留到下次。
    fn write_bytes(&mut self, flash: &mut FlashStorage<'_>, mut data: &[u8]) -> Result<(), ()> {
        if self.pending_len > 0 {
            while self.pending_len < WRITE_ALIGN as usize && !data.is_empty() {
                self.pending[self.pending_len] = data[0];
                self.pending_len += 1;
                data = &data[1..];
            }
            if self.pending_len == WRITE_ALIGN as usize {
                let word = self.pending;
                self.raw_write(flash, &word)?;
                self.pending_len = 0;
            }
        }

        let aligned = data.len() as u32 / WRITE_ALIGN * WRITE_ALIGN;
        if aligned > 0 {
            self.raw_write(flash, &data[..aligned as usize])?;
            data = &data[aligned as usize..];
        }

        for b in data {
            if self.pending_len < WRITE_ALIGN as usize {
                self.pending[self.pending_len] = *b;
                self.pending_len += 1;
            }
        }
        Ok(())
    }

    /// 按需擦除扇区后写入，地址必须 4 字节对齐。
    fn raw_write(&mut self, flash: &mut FlashStorage<'_>, bytes: &[u8]) -> Result<(), ()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let len = bytes.len() as u32;
        if self.next_addr + len > self.dst_offset + self.dst_size {
            warn!("OTA: write out of range at {:#x}", self.next_addr);
            return Err(());
        }

        while self.erased_upto < self.next_addr + len {
            if self.erased_upto + SECTOR_SIZE > self.dst_offset + self.dst_size {
                warn!("OTA: erase out of range at {:#x}", self.erased_upto);
                return Err(());
            }
            NorFlash::erase(flash, self.erased_upto, self.erased_upto + SECTOR_SIZE).map_err(|e| {
                warn!("OTA: erase failed at {:#x}: {:?}", self.erased_upto, e);
            })?;
            self.erased_upto += SECTOR_SIZE;
        }

        Storage::write(flash, self.next_addr, bytes).map_err(|e| {
            warn!("OTA: write failed at {:#x}: {:?}", self.next_addr, e);
        })?;
        self.next_addr += len;
        Ok(())
    }

    fn switch_slot(&self, flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
        let mut table_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let table =
            partitions::read_partition_table(flash, &mut table_buf).map_err(|_| "partition table")?;

        let otadata = table
            .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
            .map_err(|_| "otadata lookup")?
            .ok_or("otadata missing")?;

        let mut raw = [0u8; OTADATA_ENTRY_LEN * 2];
        ReadStorage::read(flash, otadata.offset(), &mut raw).map_err(|_| "otadata read")?;

        let entry_at = |i: usize| -> Option<OtaSelectEntry> {
            let start = i * OTADATA_ENTRY_LEN;
            let mut buf = [0u8; OTADATA_ENTRY_LEN];
            buf.copy_from_slice(&raw[start..start + OTADATA_ENTRY_LEN]);
            OtaSelectEntry::from_bytes(&buf)
        };

        let max_seq = entry_at(0)
            .into_iter()
            .chain(entry_at(1))
            .map(|e| e.ota_seq)
            .max()
            .unwrap_or(0);
        let new_seq = match max_seq.checked_add(1) {
            Some(v) if v != u32::MAX => v,
            _ => 1,
        };

        let other_index = 1 - self.dst_index;
        let mut entries = [0xFFu8; OTADATA_ENTRY_LEN * 2];
        entries[self.dst_index * OTADATA_ENTRY_LEN..][..OTADATA_ENTRY_LEN]
            .copy_from_slice(&OtaSelectEntry::new(new_seq).to_bytes());
        if let Some(old) = entry_at(other_index) {
            entries[other_index * OTADATA_ENTRY_LEN..][..OTADATA_ENTRY_LEN]
                .copy_from_slice(&OtaSelectEntry::new(old.ota_seq).to_bytes());
        }

        NorFlash::erase(flash, otadata.offset(), otadata.offset() + SECTOR_SIZE)
            .map_err(|_| "otadata erase")?;
        Storage::write(flash, otadata.offset(), &entries).map_err(|_| "otadata write")?;

        info!(
            "OTA: otadata updated ota_{} seq={} (other seq={:?})",
            self.dst_index,
            new_seq,
            entry_at(other_index).map(|e| e.ota_seq)
        );
        Ok(())
    }
}

/// 选择当前未运行的 OTA 槽，返回条目与 otadata 下标。
fn select_target_slot<'a>(table: &PartitionTable<'a>) -> Option<(PartitionEntry<'a>, usize)> {
    let booted_offset = table.booted_partition().ok().flatten()?.offset();

    let slots = [AppPartitionSubType::Ota0, AppPartitionSubType::Ota1];
    for (index, sub) in slots.iter().enumerate() {
        let entry = table
            .find_partition(PartitionType::App(*sub))
            .ok()
            .flatten()?;
        if entry.offset() != booted_offset {
            return Some((entry, index));
        }
    }
    None
}

fn trim_ascii(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}
