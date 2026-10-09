//! Fernet + XOR wallet decryption, matching `bot_target` and cryptotrading_prod.

use aes::Aes128;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use cbc::{Decryptor, Encryptor};
use cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use std::fs;
use std::path::Path;

const XOR_KEY: &[u8] = b"Djajsl09!2";

type HmacSha256 = Hmac<Sha256>;
type Aes128CbcEnc = Encryptor<Aes128>;
type Aes128CbcDec = Decryptor<Aes128>;

fn xor_bytes(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.is_empty() {
        bail!("XOR key is empty");
    }
    Ok(data
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ key[i % key.len()])
        .collect())
}

pub fn xor_encrypt(private_key: &str) -> Result<String> {
    let out = xor_bytes(private_key.trim().as_bytes(), XOR_KEY)?;
    Ok(STANDARD.encode(out))
}

pub fn xor_decrypt(encrypted: &str) -> Result<String> {
    let raw = STANDARD.decode(encrypted.trim()).context("xor payload is not base64")?;
    let out = xor_bytes(&raw, XOR_KEY)?;
    String::from_utf8(out).context("xor plaintext is not utf-8")
}

fn to_url_safe(b64: &str) -> String {
    b64.replace('+', "-").replace('/', "_")
}

fn from_url_safe(b64: &str) -> Result<Vec<u8>> {
    let padded = b64.replace('-', "+").replace('_', "/");
    STANDARD.decode(padded).context("fernet token is not base64")
}

pub fn generate_fernet_key() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    to_url_safe(&STANDARD.encode(raw))
}

fn split_fernet_key(key_material: &[u8]) -> Result<([u8; 16], [u8; 16])> {
    let owned;
    let raw: &[u8] = if key_material.len() == 32 {
        key_material
    } else {
        owned = from_url_safe(&String::from_utf8_lossy(key_material))?;
        &owned
    };
    if raw.len() != 32 {
        bail!("Fernet key must decode to 32 bytes");
    }
    let mut signing = [0u8; 16];
    let mut encryption = [0u8; 16];
    signing.copy_from_slice(&raw[..16]);
    encryption.copy_from_slice(&raw[16..]);
    Ok((signing, encryption))
}

pub fn fernet_encrypt(key_material: &[u8], message: &str) -> Result<String> {
    let (signing_key, encryption_key) = split_fernet_key(key_material)?;
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ciphertext = Aes128CbcEnc::new(&encryption_key.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(message.as_bytes());
    let mut basic = Vec::with_capacity(1 + 8 + 16 + ciphertext.len());
    basic.push(0x80);
    let timestamp = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs())
    .to_be_bytes();
    basic.extend_from_slice(&timestamp);
    basic.extend_from_slice(&iv);
    basic.extend_from_slice(&ciphertext);
    let mut mac = HmacSha256::new_from_slice(&signing_key).context("hmac key")?;
    mac.update(&basic);
    basic.extend_from_slice(&mac.finalize().into_bytes());
    Ok(to_url_safe(&STANDARD.encode(basic)))
}

pub fn fernet_decrypt(key_material: &[u8], token: &str) -> Result<String> {
    let (signing_key, encryption_key) = split_fernet_key(key_material)?;
    let data = from_url_safe(token.trim())?;
    if data.len() < 1 + 8 + 16 + 32 {
        bail!("Invalid Fernet token");
    }
    if data[0] != 0x80 {
        bail!("Invalid Fernet token version");
    }
    let (basic, mac) = data.split_at(data.len() - 32);
    let mut verifier = HmacSha256::new_from_slice(&signing_key).context("hmac key")?;
    verifier.update(basic);
    let expected = verifier.finalize().into_bytes();
    if !bool::from(expected.ct_eq(mac)) {
        bail!("Failed to decrypt sig (wrong key file?)");
    }
    let iv: [u8; 16] = basic[9..25].try_into().unwrap();
    let plain = Aes128CbcDec::new(&encryption_key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&basic[25..])
        .map_err(|_| anyhow::anyhow!("Failed to decrypt sig (wrong key file?)"))?;
    String::from_utf8(plain).context("decrypted private key is not utf-8")
}

pub fn encrypt_trading_private_key(plain_base58: &str, key_file_path: &Path) -> Result<String> {
    let key = if key_file_path.exists() {
        let raw = fs::read(key_file_path).with_context(|| format!("read {}", key_file_path.display()))?;
        String::from_utf8_lossy(&raw).trim().as_bytes().to_vec()
    } else {
        if let Some(parent) = key_file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let generated = generate_fernet_key();
        fs::write(key_file_path, generated.as_bytes())?;
        generated.into_bytes()
    };
    fernet_encrypt(&key, &xor_encrypt(plain_base58)?)
}

pub fn decrypt_trading_private_key(encrypted: &str, key_file_path: &Path) -> Result<String> {
    let raw = fs::read(key_file_path)
        .with_context(|| format!("Missing wallet key file: {}", key_file_path.display()))?;
    // Key files often end with a trailing newline from editors / scp.
    let key = String::from_utf8_lossy(&raw);
    let plain_xor = fernet_decrypt(key.trim().as_bytes(), encrypted)?;
    xor_decrypt(&plain_xor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fernet_round_trip_matches_xor_wrapper() {
        let key = generate_fernet_key();
        let secret = "4".repeat(64);
        let token = fernet_encrypt(key.as_bytes(), &xor_encrypt(&secret).unwrap()).unwrap();
        let plain = xor_decrypt(&fernet_decrypt(key.as_bytes(), &token).unwrap()).unwrap();
        assert_eq!(plain, secret);
    }
}
