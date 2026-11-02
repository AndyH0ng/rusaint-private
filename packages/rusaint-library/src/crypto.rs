//! oasis 로그인 화면의 비밀번호 암호화를 그대로 재현합니다.
//!
//! 사이트 JS(`cryptoService.encrypt`)는 상수 키 문자열을 PBKDF2-HMAC-SHA1(5000회, 128비트)로
//! 늘린 키와 고정 IV로 AES-128-CBC(PKCS#7) 암호화한 뒤 base64로 보냅니다. 키·salt·IV가 모두
//! 공개된 상수라 보안 효과는 없고, 서버가 이 형식을 요구하기 때문에 맞춰 줄 뿐입니다.

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// 사이트 설정 `AUTH.AES_ENCRYPT_KEY`
const KEY_MATERIAL: &[u8] = b"M2M2YjcyMmU2OTZlNjU2YjJlNjM2OTcwNjU3MjNl";
const SALT: &[u8] = b"kr.inek.encrypte";
const IV: &[u8; 16] = b"[kr:inek:solved]";
const ITERATIONS: u32 = 5000;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

pub(crate) fn encrypt_password(password: &str) -> String {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(KEY_MATERIAL, SALT, ITERATIONS, &mut key);
    let ciphertext = Aes128CbcEnc::new(&key.into(), IV.into())
        .encrypt_padded_vec_mut::<Pkcs7>(password.as_bytes());
    STANDARD.encode(ciphertext)
}

#[cfg(test)]
mod tests {
    use super::encrypt_password;

    /// `openssl enc -aes-128-cbc` 로 독립적으로 만든 값과 비교합니다.
    #[test]
    fn matches_openssl_vectors() {
        assert_eq!(encrypt_password("test1234"), "9ap9Es/bXgxt+zYBZknfEA==");
        assert_eq!(
            encrypt_password("P@ssw0rd!한글"),
            "m8RaKUj66DuOqsN0Mo9ffg=="
        );
    }
}
