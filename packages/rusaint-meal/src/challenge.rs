//! soongguri.com 의 nginx testcookie 확인 페이지.
//!
//! 일부 IP(해외 데이터센터 등)에는 메뉴 대신 "자동등록방지를 위해 보안절차를 거치고 있습니다" 페이지가 옵니다.
//! 페이지에 들어 있는 key(a)·iv(b)·암호문(c)을 AES-128-CBC 로 풀어 `CUPID` 쿠키로 보내면 원래 페이지가 옵니다.
//! 브라우저가 하는 계산을 그대로 따라 할 뿐이며, 쿠키는 하루 동안 다시 씁니다.

use std::sync::LazyLock;

use aes::Aes128;
use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
use regex_lite::Regex;

pub(crate) const COOKIE_NAME: &str = "CUPID";

static NUMBERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\b([abc])=toNumbers\("([0-9a-fA-F]{32})"\)"#).unwrap());

/// 확인 페이지인지
pub(crate) fn is_challenge(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    text.contains("slowAES") && text.contains(COOKIE_NAME)
}

fn hex16(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// 확인 페이지에서 `CUPID` 쿠키 값을 계산합니다. 확인 페이지가 아니면 `None`.
pub(crate) fn solve(body: &[u8]) -> Option<String> {
    if !is_challenge(body) {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let (mut key, mut iv, mut data) = (None, None, None);
    for c in NUMBERS.captures_iter(&text) {
        let value = hex16(&c[2]);
        match &c[1] {
            "a" => key = value,
            "b" => iv = value,
            "c" => data = value,
            _ => {}
        }
    }
    let (key, iv, data) = (key?, iv?, data?);
    // 블록 하나짜리 CBC 복호화: AES 복호화 후 IV 와 XOR, 패딩 제거 없음 (slowAES.decrypt 와 같음)
    let mut block = GenericArray::from(data);
    Aes128::new(&GenericArray::from(key)).decrypt_block(&mut block);
    Some(
        block
            .iter()
            .zip(iv)
            .map(|(b, v)| format!("{:02x}", b ^ v))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<script>function toNumbers(t){}var a=toNumbers("8f0ba2456ce5e67beee929afded51ef2"),b=toNumbers("333113c0bc54ee048e0555026c90022a"),c=toNumbers("feb4014b468f703f1980b6ec1f482d10"),now=new Date;document.cookie="CUPID="+toHex(slowAES.decrypt(c,2,a,b))</script>자동등록방지를 위해 보안절차를 거치고 있습니다."#;

    #[test]
    fn solves_testcookie_page() {
        // VM 에서 openssl 로 계산하고 실제로 메뉴가 열리는 것을 확인한 값
        assert_eq!(
            solve(PAGE.as_bytes()).as_deref(),
            Some("4e1f9a584cafe5d38c44c337b46f6461")
        );
    }

    #[test]
    fn ignores_normal_pages() {
        assert!(!is_challenge("<td class=\"menu_nm\">중식1</td>".as_bytes()));
        assert_eq!(solve(b"<html></html>"), None);
    }
}
