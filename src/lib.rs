use kaspa_consensus_core::tx::{TransactionId, TransactionOutpoint};

pub type PlaygroundResult<T> = Result<T, Box<dyn std::error::Error>>;

pub fn demo_outpoint(byte: u8, index: u32) -> TransactionOutpoint {
    TransactionOutpoint { transaction_id: TransactionId::from_bytes([byte; 32]), index }
}
