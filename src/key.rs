use anyhow::Result;
use data_encoding::BASE32_NOPAD;

const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const KEY_PREFIX: &str = "sk-aip-";

pub struct NewKey {
    pub full: String,
    pub prefix: String,
    pub hash: [u8; 32],
}

pub fn generate() -> Result<NewKey> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)?;
    let encoded = BASE32_NOPAD.encode(&random);
    let body: String = encoded.bytes().map(crockford_char).collect();
    let full = format!("{KEY_PREFIX}{body}");
    let prefix = full.chars().skip(KEY_PREFIX.len()).take(8).collect();
    let hash = hash(&full);
    Ok(NewKey {
        full,
        prefix,
        hash,
    })
}

pub fn hash(key: &str) -> [u8; 32] {
    *blake3::hash(key.as_bytes()).as_bytes()
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn crockford_char(byte: u8) -> char {
    let index = match byte {
        b'A'..=b'Z' => (byte - b'A') as usize,
        _ => 26 + (byte - b'2') as usize,
    };
    CROCKFORD_ALPHABET[index] as char
}
