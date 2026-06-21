use anyhow::Result;
use fastcdc::v2020::FastCDC;

pub const FASTCDC_MIN: u32 = 64 * 1024;
pub const FASTCDC_AVG: u32 = 1024 * 1024;
pub const FASTCDC_MAX: u32 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPiece {
    pub offset: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct FastCdcChunker;

impl FastCdcChunker {
    pub fn id(&self) -> &'static str {
        "fastcdc"
    }

    pub fn config_fingerprint(&self) -> &'static str {
        "fastcdc-64k-1m-8m"
    }

    pub fn split(&self, bytes: &[u8]) -> Result<Vec<ChunkPiece>> {
        let mut pieces = Vec::new();

        for entry in FastCDC::new(bytes, FASTCDC_MIN, FASTCDC_AVG, FASTCDC_MAX) {
            let start = entry.offset;
            let end = start + entry.length;
            pieces.push(ChunkPiece {
                offset: start,
                bytes: bytes[start..end].to_vec(),
            });
        }

        if pieces.is_empty() && !bytes.is_empty() {
            pieces.push(ChunkPiece {
                offset: 0,
                bytes: bytes.to_vec(),
            });
        }

        Ok(pieces)
    }
}
