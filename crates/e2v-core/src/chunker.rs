use anyhow::Result;
use fastcdc::v2020::FastCDC;

pub const FASTCDC_MIN: u32 = 64 * 1024;
pub const FASTCDC_AVG: u32 = 1024 * 1024;
pub const FASTCDC_MAX: u32 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpan {
    pub offset: usize,
    pub length: usize,
}

impl ChunkSpan {
    pub fn end(&self) -> usize {
        self.offset + self.length
    }
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

    pub fn split_spans(&self, bytes: &[u8]) -> Result<Vec<ChunkSpan>> {
        let mut spans = Vec::new();

        for entry in FastCDC::new(bytes, FASTCDC_MIN, FASTCDC_AVG, FASTCDC_MAX) {
            spans.push(ChunkSpan {
                offset: entry.offset,
                length: entry.length,
            });
        }

        if spans.is_empty() && !bytes.is_empty() {
            spans.push(ChunkSpan {
                offset: 0,
                length: bytes.len(),
            });
        }

        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::FastCdcChunker;

    #[test]
    fn split_spans_cover_input_without_gaps_or_overlaps() {
        let chunker = FastCdcChunker;
        let bytes = vec![b'a'; (super::FASTCDC_AVG as usize * 2) + 17];

        let spans = chunker.split_spans(&bytes).unwrap();

        assert!(!spans.is_empty());
        assert_eq!(spans.first().unwrap().offset, 0);
        assert_eq!(spans.last().unwrap().end(), bytes.len());
        let total_len = spans.iter().map(|span| span.length).sum::<usize>();
        assert_eq!(total_len, bytes.len());

        for window in spans.windows(2) {
            assert_eq!(window[0].end(), window[1].offset);
        }
    }
}
