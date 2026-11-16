//! KICA증명 자료 구조

use serde::Serialize;

/// 일반 출력 보관함에 있는 발급된 증명서 1건
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Document {
    /// 학교의 문서 번호(MINNO). 출력·원본확인에서 이 값으로 문서를 지목합니다.
    pub document_id: String,
    /// 증명서 종류 이름
    pub name: String,
    /// 신청한 통수
    pub copies: u32,
    /// 이미 출력한 통수
    pub printed: u32,
    /// 국문 증명서인지
    pub korean: bool,
    /// 출력 구분(화면의 preview 인자 그대로). 출력 요청에 다시 보냅니다.
    pub print_kind: String,
    /// 출력 가능 표시(화면의 preview 인자 그대로)
    pub printable: String,
}

impl Document {
    /// 남은 출력 매수가 있는지
    pub fn available(&self) -> bool {
        self.printed < self.copies
    }

    /// 남은 출력 매수
    pub fn remaining(&self) -> u32 {
        self.copies.saturating_sub(self.printed)
    }
}
