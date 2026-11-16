//! 문서 수신에 필요한 실행 인자 해석과, 복호화·렌더링을 맡기는 경계입니다.
//!
//! 복호화와 PDF 렌더링은 이 크레이트가 하지 않습니다. 호출자가 [`Report`] 구현을 넘기고,
//! 이 크레이트는 학교와의 통신 순서만 책임집니다.

use std::collections::BTreeMap;
use std::future::Future;

use serde::Serialize;

use crate::CertiError;

/// 실행 인자가 가리켜야 하는 문서 서버 경로. 다른 주소가 오면 진행하지 않습니다.
pub(crate) const DOCUMENT_PATHS: [(&str, &str); 3] = [
    ("URLFile", "/wm1.0/jsp/report/sendfile.jsp"),
    ("URLCheck", "/wm1.0/jsp/report/senddocno.jsp"),
    ("URLPost", "/wm1.0/jsp/report/printcomplete.jsp"),
];

/// 복호화한 실행 인자에서 읽은 문서 수신 정보
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    /// 출력 세션 식별자
    pub tpid: String,
    /// 문서 번호. 보관함에서 고른 문서와 같아야 합니다.
    pub document_number: String,
    pub receive_type: String,
    pub receive_target: String,
}

/// 실행 인자(`PREVIEWENC` 응답을 복호화한 문자열)를 읽습니다.
///
/// 형태는 `http://127.0.0.1:PORT/SHOWREPORT?KEY=VALUE|KEY=VALUE|…` 입니다.
/// 문서 서버 주소 세 개가 실측한 경로와 같은지 확인하고, 다르면 오류로 멈춥니다.
pub(crate) fn parse_launch(plain: &[u8]) -> Result<Launch, CertiError> {
    // 실제 실행 인자는 CP949 다. EUC-KR 디코더는 CP949 를 포함하고 ASCII 와 호환되므로,
    // 우리가 쓰는 ASCII 매개변수는 한글이 섞여 있어도 그대로 읽힙니다.
    let text = match std::str::from_utf8(plain) {
        Ok(text) => std::borrow::Cow::Borrowed(text),
        Err(_) => encoding_rs::EUC_KR.decode(plain).0,
    };
    let query = text
        .split_once('?')
        .map(|(_, query)| query)
        .ok_or_else(|| CertiError::Protocol("실행 인자에 매개변수가 없습니다".into()))?;
    let values: BTreeMap<&str, &str> = query
        .split('|')
        .filter_map(|item| item.split_once('='))
        .collect();
    for (key, path) in DOCUMENT_PATHS {
        let expected = format!("{}{path}", crate::HOST);
        if values.get(key) != Some(&expected.as_str()) {
            return Err(CertiError::Protocol(
                "확인하지 않은 문서 서버 주소입니다".into(),
            ));
        }
    }
    let field = |key: &str| {
        values
            .get(key)
            .filter(|v| !v.is_empty())
            .map(|v| (*v).to_owned())
    };
    Ok(Launch {
        tpid: field("TPID")
            .ok_or_else(|| CertiError::Protocol("출력 세션 식별자가 없습니다".into()))?,
        document_number: field("MINNO")
            .ok_or_else(|| CertiError::Protocol("실행 인자에 문서 번호가 없습니다".into()))?,
        receive_type: field("RECEIVE_TYPE").unwrap_or_default(),
        receive_target: field("RECEIVE_TARGET").unwrap_or_default(),
    })
}

/// 문서확인번호는 16자리 영숫자입니다.
pub(crate) fn document_code(raw: &str) -> Result<String, CertiError> {
    let code = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if code.len() == 16 && code.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(code.to_owned())
    } else {
        Err(CertiError::Protocol(
            "문서확인번호를 발급받지 못했습니다".into(),
        ))
    }
}

/// REPORTX 암호문 복호화와 FastReport 문서 렌더링을 맡는 구현.
///
/// 네트워크를 쓰지 않는 순수 변환이어야 합니다. 구현은 호출자(서버)가 제공합니다.
pub trait Report {
    /// `PREVIEWENC` 응답을 복호화해 실행 인자 문자열을 돌려줍니다.
    fn decrypt_launch(
        &self,
        encrypted: &[u8],
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send;

    /// 문서를 복호화하고 압축을 풀어 FastReport 문서를 돌려줍니다. 암호는 문서 번호입니다.
    fn decrypt_document(
        &self,
        encrypted: &[u8],
        document_number: &str,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send;

    /// FastReport 문서를 PDF 로 렌더링합니다. 문서확인번호를 문서에 반영합니다.
    fn render(
        &self,
        document: &[u8],
        document_code: &str,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

/// 렌더링까지 끝났지만 아직 학교에 출력 완료를 알리지 않은 증명서.
///
/// 이 단계까지는 출력 매수를 쓰지 않습니다. 호출자는 이 PDF 와 문서확인번호를 먼저 보관한 뒤
/// [`crate::CertiClient::complete_print`] 를 불러야 합니다. 통지 뒤에 오류가 나도 PDF 를 잃지 않습니다.
#[derive(Debug, Clone, Serialize)]
pub struct Rendered {
    /// 렌더링한 PDF
    #[serde(skip)]
    pub pdf: Vec<u8>,
    /// 학교가 발급한 16자리 문서확인번호
    pub document_code: String,
    /// 보관함의 문서 번호
    pub document_number: String,
    /// 증명서 종류 이름
    pub name: String,
    /// 출력 완료 통지에 그대로 돌려보내는 출력 세션 식별자
    pub session: String,
    /// 출력 완료 통지에 그대로 돌려보내는 수신 구분
    pub receive_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실측한 실행 인자와 같은 형태
    pub(crate) fn launch_text(document_number: &str) -> String {
        let paths = DOCUMENT_PATHS
            .iter()
            .map(|(key, path)| format!("{key}=certi.ssu.ac.kr{path}"))
            .collect::<Vec<_>>()
            .join("|");
        format!(
            "http://127.0.0.1:65432/SHOWREPORT?TPID=session-1|MINNO={document_number}|\
             RECEIVE_TYPE=WEB|RECEIVE_TARGET=|{paths}|CNT=1"
        )
    }

    /// 실제 실행 인자는 CP949 다. UTF-8 로는 읽히지 않아도 ASCII 매개변수는 그대로 읽혀야 합니다.
    #[test]
    fn reads_cp949_launch_parameters() {
        let text = launch_text("2026ABCD") + "|JOBNE=";
        let mut bytes = text.into_bytes();
        // "장학금" (CP949)
        bytes.extend_from_slice(&[0xC0, 0xE5, 0xC7, 0xD0, 0xB1, 0xDD]);
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "UTF-8 로는 깨져야 합니다"
        );
        let launch = parse_launch(&bytes).expect("CP949 실행 인자");
        assert_eq!(launch.document_number, "2026ABCD");
        assert_eq!(launch.tpid, "session-1");
    }

    #[test]
    fn reads_launch_parameters() {
        let launch = parse_launch(launch_text("2026ABCD").as_bytes()).expect("launch");
        assert_eq!(launch.tpid, "session-1");
        assert_eq!(launch.document_number, "2026ABCD");
        assert_eq!(launch.receive_type, "WEB");
        assert_eq!(launch.receive_target, "");
    }

    #[test]
    fn rejects_unknown_document_server() {
        let text = launch_text("2026ABCD").replace(
            "URLFile=certi.ssu.ac.kr/wm1.0/jsp/report/sendfile.jsp",
            "URLFile=evil.example/wm1.0/jsp/report/sendfile.jsp",
        );
        assert!(parse_launch(text.as_bytes()).is_err());
    }

    #[test]
    fn rejects_missing_fields() {
        for broken in [
            launch_text("2026ABCD").replace("MINNO=2026ABCD|", ""),
            launch_text("2026ABCD").replace("TPID=session-1|", ""),
            "no-query-at-all".to_owned(),
        ] {
            assert!(parse_launch(broken.as_bytes()).is_err());
        }
    }

    #[test]
    fn document_code_must_be_sixteen_alphanumerics() {
        assert_eq!(
            document_code("0123456789ABCDEF\0\r\n ").expect("code"),
            "0123456789ABCDEF"
        );
        for broken in ["", "short", "0123456789ABCDE!", "0123456789ABCDEF0"] {
            assert!(
                document_code(broken).is_err(),
                "{broken} 는 거부해야 합니다"
            );
        }
    }
}
