use kaspa_consensus_core::{
    hashing::{
        sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash},
        sighash_type::SIG_HASH_ALL,
    },
    tx::{MutableTransaction, Transaction, TransactionId, TransactionOutpoint},
};
use secp256k1::{Keypair, Secp256k1, SecretKey};

pub type PlaygroundResult<T> = Result<T, Box<dyn std::error::Error>>;

pub fn demo_outpoint(byte: u8, index: u32) -> TransactionOutpoint {
    TransactionOutpoint { transaction_id: TransactionId::from_bytes([byte; 32]), index }
}

pub fn demo_keypair(byte: u8) -> Keypair {
    let secret_key = SecretKey::from_slice(&[byte; 32]).expect("demo secret key is valid");
    Keypair::from_secret_key(&Secp256k1::new(), &secret_key)
}

pub fn sign_input<T: AsRef<Transaction>>(tx: &MutableTransaction<T>, input_idx: usize, keypair: &Keypair) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let sig_hash = calc_schnorr_signature_hash(&tx.as_verifiable(), input_idx, SIG_HASH_ALL, &reused_values);
    let message = secp256k1::Message::from_digest(sig_hash.as_bytes());
    let signature = keypair.sign_schnorr(message);
    let mut encoded = signature.as_ref().to_vec();
    encoded.push(SIG_HASH_ALL.to_u8());
    encoded
}
