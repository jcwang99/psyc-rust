use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefToken {
    pub value: String,
}

impl RefToken {
    pub fn new(value: String) -> Self {
        Self { value }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefVersion {
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedRef {
    pub bytes: Vec<u8>,
}

impl EncryptedRef {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRef {
    pub version: RefVersion,
    pub value: EncryptedRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasResult {
    pub applied: bool,
    pub current: Option<StoredRef>,
}

pub trait RefStore {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>>;
    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult>;
}
