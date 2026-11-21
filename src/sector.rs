//! Logical sector units are explicit; map LBAs must never be reinterpreted.
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SectorSize(usize);

impl SectorSize {
    pub(crate) const LEGACY: Self = Self(512);
    pub(crate) fn new(bytes: usize) -> io::Result<Self> {
        if !(512..=65536).contains(&bytes) || !bytes.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                message!(
                    "Unsupported logical sector size: {bytes} bytes",
                    "未対応の論理セクタサイズです: {bytes} バイト"
                ),
            ));
        }
        Ok(Self(bytes))
    }
    pub(crate) fn bytes(self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_logical_sector_units() {
        for size in [512, 1024, 4096, 65536] {
            assert_eq!(SectorSize::new(size).unwrap().bytes(), size);
        }
        for size in [0, 256, 513, 768, 131072] {
            assert!(SectorSize::new(size).is_err());
        }
    }
}
