//! Netease crypto primitives (weapi / eapi) ported 1:1 from
//! `folia-major/node_modules/@neteasecloudmusicapienhanced/api/util/crypto.js`.
//!
//! Constants and algorithm order must stay byte-identical to the JS reference,
//! because the Netease server decrypts these payloads on its side.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit};
use aes::Aes128;
use base64::Engine as _;
use block_padding::Pkcs7;
use md5::Md5;
use rand::RngCore;
use rsa::hazmat::rsa_encrypt;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPublicKey;
use sha2::Digest;

pub const IV: &[u8; 16] = b"0102030405060708";
pub const PRESET_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";
pub const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
pub const PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDgtQn2JZ34ZC28NWYpAUd98iZ3\n7BUrX/aKzmFbt7clFSs6sXqHauqKWqdtLkF2KexO40H1YTX8z2lSgBBOAxLsvakl\nV8k4cBFK9snQXE9/DDaFt6Rr7iVZMldczhC0JNgTz+SHXT6CBHuX3e9SdB1Ua44o\nncaTWz7OBGLbCiK45wIDAQAB\n-----END PUBLIC KEY-----";

const BASE62: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128EcbEnc = ecb::Encryptor<Aes128>;
type Aes128EcbDec = ecb::Decryptor<Aes128>;

/// AES-128-CBC with PKCS7 padding.
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let key = GenericArray::clone_from_slice(key);
    let iv = GenericArray::clone_from_slice(iv);
    Aes128CbcEnc::new(&key, &iv).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// AES-128-ECB with PKCS7 padding.
pub fn aes_ecb_encrypt(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let key = GenericArray::clone_from_slice(key);
    Aes128EcbEnc::new(&key).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// AES-128-ECB with PKCS7 unpadding.
pub fn aes_ecb_decrypt(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let key = GenericArray::clone_from_slice(key);
    Aes128EcbDec::new(&key)
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|e| format!("AES-ECB unpad failed: {e}"))
}

/// Raw RSA encryption (forge `encrypt(str, 'NONE')`, no padding), hex-encoded.
///
/// Uses `rsa::hazmat::rsa_encrypt` (m^e mod n) and left-pads to the modulus
/// length, matching node-forge's fixed 128-byte ciphertext.
pub fn rsa_raw_encrypt_hex(public_key_pem: &str, message: &[u8]) -> Result<String, String> {
    let key = RsaPublicKey::from_public_key_pem(public_key_pem)
        .map_err(|e| format!("invalid RSA public key: {e}"))?;
    let m = rsa::BigUint::from_bytes_be(message);
    let c = rsa_encrypt(&key, &m).map_err(|e| format!("RSA encrypt failed: {e}"))?;
    let mut bytes = c.to_bytes_be();
    let modulus_len = ((key.n().bits() + 7) / 8) as usize;
    if bytes.len() < modulus_len {
        let mut padded = vec![0u8; modulus_len - bytes.len()];
        padded.append(&mut bytes);
        bytes = padded;
    }
    Ok(hex::encode(bytes))
}

/// Lowercase hex MD5 digest.
pub fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

/// Random 16-char secret key drawn from base62 (mirrors `crypto.js`).
pub fn random_secret_key() -> String {
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| BASE62[(rng.next_u64() % 62) as usize] as char)
        .collect()
}

pub struct WeapiResult {
    pub params: String,
    pub enc_sec_key: String,
}

/// weapi: `params = AES-CBC(AES-CBC(text, presetKey, iv), secretKey, iv)`,
/// `encSecKey = rawRSA(reverse(secretKey), publicKey)`.
///
/// The inner AES-CBC output is base64-encoded BEFORE it is encrypted again by
/// the outer layer (mirrors `crypto.js`: the inner `aesEncrypt` returns the
/// base64 string, and that string is the plaintext of the outer encryption).
/// Encrypting the raw ciphertext bytes instead changes `params` entirely and
/// the upstream cannot decrypt it (volc-dcdn then answers an empty 200).
pub fn weapi_encrypt(text: &str, secret_key: &str) -> WeapiResult {
    let inner = aes_cbc_encrypt(PRESET_KEY, IV, text.as_bytes());
    let inner_b64 = base64::engine::general_purpose::STANDARD.encode(&inner);
    let outer = aes_cbc_encrypt(secret_key.as_bytes(), IV, inner_b64.as_bytes());
    let params = base64::engine::general_purpose::STANDARD.encode(outer);
    let reversed: String = secret_key.chars().rev().collect();
    let enc_sec_key =
        rsa_raw_encrypt_hex(PUBLIC_KEY_PEM, reversed.as_bytes()).expect("raw RSA cannot fail");
    WeapiResult {
        params,
        enc_sec_key,
    }
}

/// eapi: `digest = md5("nobody"+url+"use"+text+"md5forencrypt")`,
/// `data = url+"-36cd479b6b5-"+text+"-36cd479b6b5-"+digest`,
/// `params = AES-ECB(data, eapiKey)` hex uppercase.
pub fn eapi_encrypt(url: &str, text: &str) -> String {
    let digest = md5_hex(format!("nobody{url}use{text}md5forencrypt").as_bytes());
    let data = format!("{url}-36cd479b6b5-{text}-36cd479b6b5-{digest}");
    hex::encode(aes_ecb_encrypt(EAPI_KEY, data.as_bytes())).to_uppercase()
}

/// eapi response decrypt: AES-ECB with eapiKey, optionally gzip-decompressed.
pub fn eapi_res_decrypt(ciphertext_hex: &str) -> Result<String, String> {
    let bytes = hex::decode(ciphertext_hex).map_err(|e| format!("hex decode failed: {e}"))?;
    let decrypted = aes_ecb_decrypt(EAPI_KEY, &bytes)?;
    if decrypted.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(&decrypted[..]);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut out)
            .map_err(|e| format!("gzip decompress failed: {e}"))?;
        String::from_utf8(out).map_err(|e| format!("invalid utf8 after gzip: {e}"))
    } else {
        String::from_utf8(decrypted).map_err(|e| format!("invalid utf8: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic vectors generated from the reference crypto.js (see
    // util/crypto.js in the reference package). Raw RSA is deterministic, so
    // the encSecKey below is stable for the fixed secret key.

    #[test]
    fn weapi_vector() {
        let text = r#"{"foo":"bar","csrf_token":"","e_r":false}"#;
        let secret = "abcdefghijklmnop";
        let result = weapi_encrypt(text, secret);
        assert_eq!(
            result.params,
            "G2IaolY000zqr8Pw9vx72/ym7vvoUs4TPce0xX54ZyVbauR9xso5OqnrlFSGZUo7F5NUSUH7MScOyW05PUsnUNciKRvDHkZXLDybTNqZyEM="
        );
        assert_eq!(
            result.enc_sec_key,
            "d15a1683c992095d0c234c19966605c5c5964911268bbeda8cb8d08d834913e59d53b32358903a121b5fca784c1f5ae44951fd02524df58ecc98e52cc7cf8689b42c2e93ddf05b0592512d87f5960467e2f086c018849d76014d323500e30f13ef4cafbb0cf5a66731a3f1776c75ca35d0062dac70a3e33245afabcf47938487"
        );
    }

    #[test]
    fn weapi_secret_key_shape() {
        let key = random_secret_key();
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn eapi_vector() {
        let url = "/api/song/lyric/v1";
        let text = r#"{"id":33894312,"cp":false,"tv":0,"lv":0,"rv":0,"kv":0,"yv":0,"ytv":0,"yrv":0,"e_r":false}"#;
        let params = eapi_encrypt(url, text);
        assert_eq!(params, "04AE33D34A93FE3EC22DA8FA305D290AB337D0FE5F36D211DE0D338CC6AA89D0225F21612C19710495E1945C831DFA71D461DFF90077763EC1AEFEEA70B344C419ABCE961F17B6B8EC72B6A226725280E97469E44EE0EE9B82E7B6CB0ADD23232E0EAC4A2DE3CBFFBC95F6348044FFB13565059F160BF53B62B4D2D656A8B657944B8F6260528DA7A0B550FE0510C73D9E3746F50F4E1B58AFF26552EC2DA68F9E43AA7B06D8F22F41C68411024A806D");
    }

    #[test]
    fn eapi_md5_digest_matches_reference() {
        let url = "/api/song/lyric/v1";
        let text = r#"{"id":33894312,"cp":false,"tv":0,"lv":0,"rv":0,"kv":0,"yv":0,"ytv":0,"yrv":0,"e_r":false}"#;
        let message = format!("nobody{url}use{text}md5forencrypt");
        assert_eq!(
            md5_hex(message.as_bytes()),
            "02c5b58f6c971f31fa11f868900afb36"
        );
    }

    #[test]
    fn eapi_res_decrypt_plain() {
        let cipher = "21A8B6EFC2490146B2455D2C1485DE00D76DC6249556D7EE8A9E22F5CD931221FD185157E397F9D2A3D70B5B1A3B2B2E";
        let plain = eapi_res_decrypt(cipher).unwrap();
        assert_eq!(plain, r#"{"code":200,"songs":[{"id":1,"name":"test"}]}"#);
    }

    #[test]
    fn eapi_res_decrypt_gzip() {
        let cipher = "52DA83621FA2E4B846413E043A2BAF55968CE0A484EF8C8407982E04530957098E7675012B4F43040199C27073952F4F01979C475A120DAE5191C64001730F773EDD6465781353680163386A2AA4D343";
        let plain = eapi_res_decrypt(cipher).unwrap();
        assert_eq!(plain, r#"{"code":200,"songs":[{"id":1,"name":"test"}]}"#);
    }

    #[test]
    fn eapi_roundtrip() {
        let url = "/api/cloud/lyric/get";
        let text = r#"{"userId":1,"songId":2,"lv":-1,"kv":-1,"e_r":false}"#;
        let params = eapi_encrypt(url, text);
        assert!(params.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(params.len() % 32, 0);
    }
}
