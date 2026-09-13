//! 分区 0x2000 字节，两个条目位于偏移 0 与 32。
//! 条目为 `{u32 seq, u8 label[20], u32 crc, u32 version}`，
//! CRC 为 zlib CRC-32（反射、初值 0xFFFFFFFF、末尾异或 0xFFFFFFFF），覆盖前 24 字节。

/// otadata 中的单个槽条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct OtaSelectEntry {
    pub ota_seq: u32,
    pub seq_label: [u8; 20],
    pub crc: u32,
    pub version: u32,
}

impl OtaSelectEntry {
    pub const SIZE: usize = 32;

    pub fn new(seq: u32) -> Self {
        let mut e = OtaSelectEntry {
            ota_seq: seq,
            seq_label: [0u8; 20],
            crc: 0,
            version: u32::MAX,
        };
        e.crc = Self::crc_of(e.ota_seq, &e.seq_label);
        e
    }

    pub fn crc_of(seq: u32, label: &[u8; 20]) -> u32 {
        let mut hasher = Crc32::new();
        hasher.update(&seq.to_le_bytes());
        hasher.update(label);
        hasher.finalize()
    }

    pub fn is_valid(&self) -> bool {
        self.crc == Self::crc_of(self.ota_seq, &self.seq_label)
    }

    /// 从 32 字节解析并校验，无效（含全 0xFF）返回 `None`。
    pub fn from_bytes(raw: &[u8; 32]) -> Option<Self> {
        let entry = OtaSelectEntry {
            ota_seq: u32::from_le_bytes(raw[0..4].try_into().ok()?),
            seq_label: raw[4..24].try_into().ok()?,
            crc: u32::from_le_bytes(raw[24..28].try_into().ok()?),
            version: u32::from_le_bytes(raw[28..32].try_into().ok()?),
        };
        if entry.is_valid() { Some(entry) } else { None }
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[0..4].copy_from_slice(&self.ota_seq.to_le_bytes());
        out[4..24].copy_from_slice(&self.seq_label);
        out[24..28].copy_from_slice(&self.crc.to_le_bytes());
        out[28..32].copy_from_slice(&self.version.to_le_bytes());
        out
    }
}

/// 反射式 CRC-32（多项式 0xEDB88320）。
pub struct Crc32 {
    crc: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Crc32 { crc: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        let mut crc = self.crc;
        while !data.is_empty() {
            crc ^= data[0] as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
            data = &data[1..];
        }
        self.crc = crc;
    }

    pub fn finalize(&self) -> u32 {
        self.crc ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn entry_roundtrip_and_validity() {
        let e = OtaSelectEntry::new(3);
        assert!(e.is_valid());
        assert_eq!(e.version, u32::MAX);

        let mut bad = e;
        bad.seq_label[0] ^= 0xFF;
        assert!(!bad.is_valid());

        let b = e.to_bytes();
        assert_eq!(&b[0..4], &3u32.to_le_bytes());
        assert_eq!(&b[24..28], &e.crc.to_le_bytes());
    }

    #[test]
    fn entry_bytes_roundtrip() {
        let e = OtaSelectEntry::new(7);
        assert_eq!(OtaSelectEntry::from_bytes(&e.to_bytes()), Some(e));
        assert_eq!(OtaSelectEntry::from_bytes(&[0xFF; 32]), None);
    }
}
