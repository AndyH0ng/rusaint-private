//! 숭실대학교 KICA증명(certi.ssu.ac.kr) 증명서 발급 클라이언트입니다.
//!
//! 로그인 흐름 (2026-11 실측): SmartID 가 돌려주는 `sToken`/`sIdno` 를 `/servlet/WMLOGIN` 에 넘기면
//! 서버가 중간 폼(`COMMAND=PORTALWAITING`, `COMMAND=SSOLOGINNEW`)을 한두 번 더 제출하게 합니다.
//! 화면에 로그아웃 링크가 보이면 세션이 열린 것으로 봅니다.
//!
//! 이 크레이트는 학교와의 통신과 화면 파싱만 담당합니다. 결제 승인 상태, 주문 보관,
//! 문서 복호화와 PDF 렌더링은 각각 다른 계층의 책임입니다.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;
use rusaint_sso::{SsoCredentials, SsoError};
use url::Url;

pub mod model;
pub mod parse;
pub mod payment;
pub mod report;

pub use model::Document;
pub use report::{Rendered, Report};

const BASE_URL: &str = "https://certi.ssu.ac.kr";
const HOST: &str = "certi.ssu.ac.kr";
const LOGIN_PATH: &str = "/servlet/WMLOGIN";
const BASKET_PATH: &str = "/servlet/WMBASKET?COMMAND=BASKETLIST";
const PRINT_CHECK_PATH: &str = "/servlet/WMBASKET?COMMAND=EXECHECK";
const BASKET_SERVLET: &str = "/servlet/WMBASKET";
const VERIFY_PATH: &str = "/servlet/WMVERIFY?COMMAND=GETPRINTVIEWEXE";
/// 학교가 내려 주는 문서 응답 크기 상한
const MAX_DOCUMENT: usize = 32 * 1024 * 1024;
/// 로그인 중간 폼이 지목할 수 있는 주소. 그 밖의 주소는 절차가 바뀐 것으로 보고 멈춥니다.
const LOGIN_STEPS: [&str; 2] = [
    "/servlet/WMLOGIN?COMMAND=PORTALWAITING",
    "/servlet/WMLOGIN?COMMAND=SSOLOGINNEW",
];
/// 로그인 중간 폼 제출 횟수 상한
const MAX_LOGIN_STEPS: usize = 4;
/// 숭실대학교 기관 코드. 학교 화면이 모든 요청에 함께 보냅니다.
const INSTITUTION: &str = "021163";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 연결을 재사용하는 시간. 미국 리전에서 학교까지 왕복이 길어, 연결이 식으면
/// TCP·TLS 를 다시 맺는 데만 왕복 3번이 듭니다. 기본값(90초)보다 길게 둡니다.
const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
/// 중간 장비가 조용한 연결을 끊지 않도록 보냅니다.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const USER_AGENT: &str = concat!(
    "rusaint-certi/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/EATSTEAK/rusaint)"
);

#[derive(Debug, thiserror::Error)]
pub enum CertiError {
    /// 요청 오류. 주소는 떼어 낸 뒤 담습니다(아래 `From` 구현).
    #[error("요청 실패: {0}")]
    Request(reqwest::Error),
    /// SmartID 가 계정 정보를 거부했거나 비어 있음. 계정 잠금을 막기 위해 다시 시도하지 않습니다.
    #[error("통합 로그인 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    #[error("증명서 로그인에 실패했습니다: {0}")]
    Login(String),
    /// 화면 구조가 실측과 달라 더 진행하면 안 되는 경우
    #[error("증명서 화면을 읽을 수 없습니다: {0}")]
    Protocol(String),
    /// 복호화·렌더링 구현이 실패한 경우
    #[error("증명서 문서를 만들 수 없습니다: {0}")]
    Report(String),
    /// PDF 는 만들었지만 학교가 원본으로 확인해 주지 않은 경우. 이 PDF 는 내보내지 않습니다.
    #[error("{0}")]
    Verification(String),
}

/// 로그인 주소에는 SmartID `sToken` 이 들어갑니다. 변환을 한 곳에 두고 주소를 떼어 내므로
/// `?` 로 올라오는 오류도 주소를 담지 않습니다.
impl From<reqwest::Error> for CertiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Request(error.without_url())
    }
}

/// 모든 요청에 함께 보내는 조회 조건
fn common_fields() -> BTreeMap<&'static str, String> {
    [
        ("GIWANNO", INSTITUTION),
        ("LOGIN_GBN", "2"),
        ("LOGIN_QRY_TYPE", "P"),
        ("GP", "2"),
        ("INNER_QRY_YN", "N"),
        ("POST_SERVICE_YN", "Y"),
        ("BROWSER", "NETSCAPE"),
        ("OS", "win"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// 일반 출력(인터넷 발급) 보관함 화면의 메뉴 위치
fn native_basket_fields() -> BTreeMap<&'static str, String> {
    let mut fields = common_fields();
    for (key, value) in [
        ("SERVICE_TYPE", "INTERNET"),
        ("LEFTNUM", "2"),
        ("SUBNUM", "1"),
        ("TOP", "SERVICE"),
    ] {
        fields.insert(key, value.to_owned());
    }
    fields
}

/// KICA증명 세션을 관리하는 클라이언트
pub struct CertiClient {
    sso: Arc<SsoCredentials>,
    /// SmartID 가 세션에 함께 묶는 접속 주소. 서버의 공개 IP 를 넣습니다.
    address: IpAddr,
    session: tokio::sync::Mutex<Option<reqwest::Client>>,
    ordering: tokio::sync::Mutex<()>,
    approval_http: tokio::sync::Mutex<Option<reqwest::Client>>,
}

impl CertiClient {
    /// 통합 로그인 계정과, 학교가 이 서버를 보는 공개 IP 로 클라이언트를 만듭니다.
    pub fn new(sso: Arc<SsoCredentials>, address: IpAddr) -> Self {
        Self {
            sso,
            address,
            session: tokio::sync::Mutex::new(None),
            ordering: tokio::sync::Mutex::new(()),
            approval_http: tokio::sync::Mutex::new(None),
        }
    }

    /// `sAddr` 쿠키 값. SmartID 는 구분 기호를 이스케이프한 형태로 넣습니다.
    fn address_cookie(&self) -> String {
        self.address
            .to_string()
            .replace('.', "%2E")
            .replace(':', "%3A")
    }

    async fn open_session(&self) -> Result<reqwest::Client, CertiError> {
        let token = self.sso.token().await.map_err(|e| match e {
            SsoError::CredentialUnusable(reason) => CertiError::CredentialUnusable(reason),
            SsoError::Failed(reason) => CertiError::Login(reason),
        })?;
        let student_id = self.sso.student_id();
        let base = Url::parse(BASE_URL).expect("static url");
        let jar = Arc::new(Jar::default());
        for (name, value) in [("sToken", token.clone()), ("sAddr", self.address_cookie())] {
            // Secure: 평문 요청에는 토큰을 싣지 않습니다.
            jar.add_cookie_str(
                &format!("{name}={value}; Domain={HOST}; Path=/; Secure"),
                &base,
            );
        }
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE)
            .tcp_keepalive(TCP_KEEPALIVE)
            .cookie_provider(jar.clone())
            // 학교 밖으로 나가거나 평문으로 내려가는 리디렉션은 따라가지 않습니다.
            // 쿠키에 인증 토큰이 들어 있어 HTTP 로 한 번만 새도 토큰이 평문으로 나갑니다.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if school_url(attempt.url()) && attempt.previous().len() < 10 {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        let mut html = http
            .get(format!("{BASE_URL}{LOGIN_PATH}"))
            .query(&[("sToken", token.as_str()), ("sIdno", student_id)])
            .header(reqwest::header::REFERER, "https://smartid.ssu.ac.kr/")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        for _ in 0..MAX_LOGIN_STEPS {
            if parse::logged_in(&html) {
                *self.approval_http.lock().await = Some(
                    reqwest::Client::builder()
                        .user_agent(USER_AGENT)
                        .timeout(REQUEST_TIMEOUT)
                        .pool_idle_timeout(POOL_IDLE)
                        .tcp_keepalive(TCP_KEEPALIVE)
                        .cookie_provider(jar.clone())
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?,
                );
                return Ok(http);
            }
            let form = parse::first_form(&html).ok_or_else(|| {
                CertiError::Login(
                    "증명서 로그인이 만료되었거나 거부됐습니다. 커넥터를 다시 연결해 주세요".into(),
                )
            })?;
            let step = match form.fields.get("SUBMITURL") {
                Some(target) if !target.is_empty() => login_step(target)?,
                // 토큰은 받았지만 다음 단계를 폼에 적어 주지 않는 화면
                _ if html.contains("var error_code = 0;") => LOGIN_STEPS[1],
                _ => {
                    return Err(CertiError::Login(
                        "증명서 로그인이 만료되었거나 거부됐습니다. 커넥터를 다시 연결해 주세요"
                            .into(),
                    ));
                }
            };
            html = http
                .post(format!("{BASE_URL}{step}"))
                .form(&form.fields)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
        }
        Err(CertiError::Login("로그인 단계가 끝나지 않았습니다".into()))
    }

    /// 로그인된 세션으로 HTML 화면을 가져옵니다. 세션이 끊겼으면 한 번 다시 로그인합니다.
    ///
    /// 로그아웃 링크가 있는 화면에만 쓸 수 있습니다. JSON·문서 응답에는 [`Self::post_bytes`] 를 씁니다.
    async fn screen(
        &self,
        path: &str,
        fields: &BTreeMap<&'static str, String>,
    ) -> Result<String, CertiError> {
        for attempt in 0..2 {
            let http = {
                let mut session = self.session.lock().await;
                if session.is_none() || attempt == 1 {
                    *session = Some(self.open_session().await?);
                }
                session.clone().expect("set above")
            };
            let html = http
                .post(format!("{BASE_URL}{path}"))
                .form(fields)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            if parse::logged_in(&html) {
                return Ok(html);
            }
            if attempt == 0 {
                *self.session.lock().await = None;
            }
        }
        Err(CertiError::Login(
            "다시 로그인해도 증명서 화면이 열리지 않습니다".into(),
        ))
    }

    /// 로그인된 세션으로 요청하고 본문을 바이트로 받습니다. 문서처럼 텍스트가 아닌 응답에 씁니다.
    async fn post_bytes(
        &self,
        path: &str,
        fields: &BTreeMap<&'static str, String>,
    ) -> Result<Vec<u8>, CertiError> {
        let http = {
            let mut session = self.session.lock().await;
            if session.is_none() {
                *session = Some(self.open_session().await?);
            }
            session.clone().expect("set above")
        };
        let body = http
            .post(format!("{BASE_URL}{path}"))
            .form(fields)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if body.len() > MAX_DOCUMENT {
            return Err(CertiError::Protocol("학교 응답 크기를 넘었습니다".into()));
        }
        Ok(body.to_vec())
    }

    /// 보관함의 발급된 증명서 1건을 받아 PDF 까지 만듭니다. 출력 매수는 아직 쓰지 않습니다.
    ///
    /// 반환한 PDF 와 문서확인번호를 먼저 보관한 뒤 [`Self::complete_print`] 를 부르세요.
    /// 중복 실행을 막는 책임은 호출자에게 있습니다.
    pub async fn render_pdf<R: Report>(
        &self,
        document: &Document,
        report: &R,
    ) -> Result<Rendered, CertiError> {
        if !document.available() {
            return Err(CertiError::Protocol("남은 출력 매수가 없습니다".into()));
        }
        let number = document.document_id.clone();
        let check = self
            .post_bytes(
                PRINT_CHECK_PATH,
                &fields([
                    ("MINNO", number.clone()),
                    ("GB", document.print_kind.clone()),
                    ("AB", document.printable.clone()),
                ]),
            )
            .await?;
        // 팝업 화면이라 로그아웃 링크가 없습니다. 세션 확인은 앞선 보관함 조회가 합니다.
        let tpid = print_session(&String::from_utf8_lossy(&check))?;

        let envelope = self
            .post_bytes(
                BASKET_SERVLET,
                &fields([
                    ("COMMAND", "PREVIEWENC".into()),
                    ("TYPE", "INTERNET".into()),
                    ("JOBNE", document.name.clone()),
                    ("MINNO", number.clone()),
                    ("TPID", tpid.clone()),
                    ("CNT", document.copies.to_string()),
                    ("RECEICNT", document.printed.to_string()),
                    (
                        "KORYN",
                        if document.korean {
                            "Y".into()
                        } else {
                            "N".into()
                        },
                    ),
                    ("PRINTGB", document.print_kind.clone()),
                    ("PRINTABLE", document.printable.clone()),
                    ("RECTYPE", "WEB".into()),
                    ("TOP", "50".into()),
                    ("LEFT", "50".into()),
                ]),
            )
            .await?;
        // PREVIEWENC 는 JSON 으로 답합니다.
        let encrypted_launch = launch_envelope(&String::from_utf8_lossy(&envelope))?;
        let plain = report
            .decrypt_launch(&encrypted_launch)
            .await
            .map_err(CertiError::Report)?;
        let launch = report::parse_launch(&plain)?;
        if launch.document_number != number {
            return Err(CertiError::Protocol(
                "받은 문서 번호가 보관함과 다릅니다".into(),
            ));
        }

        let encrypted = self
            .post_bytes(
                path_of("URLFile"),
                &fields([
                    ("TPID", launch.tpid.clone()),
                    ("MIN_NO", number.clone()),
                    ("GIWAN_NO", INSTITUTION.into()),
                ]),
            )
            .await?;
        let prepared = report
            .decrypt_document(&encrypted, &number)
            .await
            .map_err(CertiError::Report)?;

        let issued_code = self
            .post_bytes(
                path_of("URLCheck"),
                &fields([
                    ("MIN_NO", number.clone()),
                    ("RECEIVE_TYPE", launch.receive_type.clone()),
                    ("RECEIVE_TARGET", launch.receive_target.clone()),
                ]),
            )
            .await?;
        let code = report::document_code(&String::from_utf8_lossy(&issued_code))?;

        let pdf = report
            .render(&prepared, &code)
            .await
            .map_err(CertiError::Report)?;
        if !pdf.starts_with(b"%PDF-") {
            return Err(CertiError::Report("렌더링 결과가 PDF 가 아닙니다".into()));
        }
        Ok(Rendered {
            pdf,
            document_code: code,
            document_number: number,
            name: document.name.clone(),
            session: launch.tpid,
            receive_type: launch.receive_type,
        })
    }

    /// 학교에 출력 완료를 알립니다. **이 호출이 남은 출력 매수를 하나 소비합니다.**
    ///
    /// 학교는 이 통지 뒤에야 문서를 원본으로 확인해 줍니다. 호출자는 이 함수를 부르기 전에
    /// PDF 와 "통지 시도" 상태를 디스크에 남겨, 실패해도 다시 출력하지 않고 확인만 재시도해야 합니다.
    pub async fn complete_print(
        &self,
        rendered: &Rendered,
        print_model: &str,
    ) -> Result<(), CertiError> {
        self.post_bytes(
            path_of("URLPost"),
            &fields([
                ("TPID", rendered.session.clone()),
                ("SYSTEM_IP", self.address.to_string()),
                ("P_MODEL", print_model.to_owned()),
                ("MIN_DOC_NO", rendered.document_code.clone()),
                ("RECEIVE_TYPE", rendered.receive_type.clone()),
            ]),
        )
        .await?;
        Ok(())
    }

    /// 학교가 이 문서확인번호를 원본으로 확인해 주는지 봅니다. 몇 번이든 다시 물어도 됩니다.
    pub async fn verified_original(
        &self,
        document_code: &str,
        document_number: &str,
    ) -> Result<bool, CertiError> {
        let body = self
            .post_bytes(
                VERIFY_PATH,
                &fields([
                    ("DOC_NO", document_code.to_owned()),
                    ("GIWANNO", INSTITUTION.into()),
                ]),
            )
            .await?;
        Ok(verified(&body, document_number))
    }

    /// 일반 출력 보관함에 있는 발급된 증명서 목록(읽기 전용)
    pub async fn documents(&self) -> Result<Vec<Document>, CertiError> {
        let html = self.screen(BASKET_PATH, &native_basket_fields()).await?;
        Ok(parse::documents(&html))
    }
}

/// 공통 조회 조건에 이번 요청의 값을 더합니다.
fn fields<const N: usize>(values: [(&'static str, String); N]) -> BTreeMap<&'static str, String> {
    let mut fields = common_fields();
    fields.extend(values);
    fields
}

/// 실행 인자가 가리키는 문서 서버 경로
fn path_of(key: &str) -> &'static str {
    report::DOCUMENT_PATHS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, path)| *path)
        .expect("문서 서버 경로는 상수입니다")
}

/// `EXECHECK` 응답의 `opener.preview1('…','…','TPID'…)` 에서 출력 세션 식별자를 읽습니다.
fn print_session(html: &str) -> Result<String, CertiError> {
    let rest = html
        .split_once("opener.preview1(")
        .map(|(_, rest)| rest)
        .ok_or_else(|| CertiError::Protocol("출력 세션 식별자를 찾지 못했습니다".into()))?;
    let argument = rest
        .split('\'')
        .nth(5)
        .filter(|value| !value.is_empty() && !value.contains(','))
        .ok_or_else(|| CertiError::Protocol("출력 세션 식별자를 읽을 수 없습니다".into()))?;
    Ok(argument.to_owned())
}

/// `PREVIEWENC` 응답(JSON `res`)의 base64 를 바이트로 바꿉니다.
fn launch_envelope(body: &str) -> Result<Vec<u8>, CertiError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| CertiError::Protocol("실행 인자 응답이 JSON 이 아닙니다".into()))?;
    let encoded = value
        .get("res")
        .and_then(serde_json::Value::as_str)
        .filter(|res| !res.is_empty())
        .ok_or_else(|| CertiError::Protocol("실행 인자 응답에 res 가 없습니다".into()))?;
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|_| CertiError::Protocol("실행 인자 응답을 해독할 수 없습니다".into()))
}

/// 원본확인 응답. 성공 표시와 문서 번호가 모두 맞아야 참입니다.
fn verified(body: &[u8], document_number: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|value| {
        value.get("SUCC").and_then(serde_json::Value::as_str) == Some("Y")
            && value.get("MIN_NO").and_then(serde_json::Value::as_str) == Some(document_number)
    })
}

/// 학교의 HTTPS 기본 포트 주소인지. 호스트만 보면 같은 호스트의 평문 주소가 통과합니다.
fn school_url(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some(HOST) && url.port().is_none()
}

/// 로그인 중간 폼이 지목한 주소를 확인합니다. 학교 안의 알려진 단계만 허용합니다.
fn login_step(target: &str) -> Result<&'static str, CertiError> {
    let path = match Url::parse(target) {
        Ok(url) => {
            if !school_url(&url) {
                return Err(CertiError::Protocol(
                    "로그인 절차가 학교의 HTTPS 주소가 아닌 곳을 가리킵니다".into(),
                ));
            }
            let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
            format!("{}{query}", url.path())
        }
        Err(_) => target.to_owned(),
    };
    LOGIN_STEPS
        .into_iter()
        .find(|step| *step == path)
        .ok_or_else(|| CertiError::Protocol("확인하지 않은 로그인 단계입니다".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_cookie_escapes_separators() {
        let client = CertiClient::new(
            SsoCredentials::from_token("20240000", "token"),
            "203.0.113.7".parse().expect("ip"),
        );
        assert_eq!(client.address_cookie(), "203%2E0%2E113%2E7");
    }

    #[test]
    fn login_steps_are_limited_to_known_paths() {
        assert_eq!(
            login_step("/servlet/WMLOGIN?COMMAND=SSOLOGINNEW").expect("known"),
            LOGIN_STEPS[1]
        );
        assert_eq!(
            login_step("https://certi.ssu.ac.kr/servlet/WMLOGIN?COMMAND=PORTALWAITING")
                .expect("known"),
            LOGIN_STEPS[0]
        );
        for target in [
            "https://evil.example/servlet/WMLOGIN?COMMAND=SSOLOGINNEW",
            // 같은 호스트라도 평문이면 쿠키의 토큰이 노출됩니다.
            "http://certi.ssu.ac.kr/servlet/WMLOGIN?COMMAND=SSOLOGINNEW",
            "https://certi.ssu.ac.kr:8443/servlet/WMLOGIN?COMMAND=SSOLOGINNEW",
            "/servlet/WMPAY?COMMAND=NEWPAYMENT",
            "/wm1.0/jsp/pay/lg/smart/newcertres.jsp",
        ] {
            assert!(login_step(target).is_err(), "{target} 는 거부해야 합니다");
        }
    }

    #[test]
    fn only_school_https_urls_are_followed() {
        assert!(school_url(
            &Url::parse("https://certi.ssu.ac.kr/servlet/WMBASKET").expect("url")
        ));
        for url in [
            "http://certi.ssu.ac.kr/servlet/WMBASKET",
            "https://certi.ssu.ac.kr:8443/servlet/WMBASKET",
            "https://other.ssu.ac.kr/servlet/WMBASKET",
            "https://evil.example/servlet/WMBASKET",
        ] {
            assert!(
                !school_url(&Url::parse(url).expect("url")),
                "{url} 는 따라가면 안 됩니다"
            );
        }
    }

    /// 로그인 주소에는 토큰이 들어가므로 오류 메시지에 주소가 남지 않아야 합니다.
    /// 변환은 `request()` 한 곳에만 있고 `CertiError` 에는 `From<reqwest::Error>` 가 없습니다.
    #[tokio::test]
    async fn request_errors_drop_the_url_with_the_token() {
        // 로컬의 닫힌 포트로 보내 연결 오류를 만듭니다. 외부 통신은 하지 않습니다.
        let failed = reqwest::Client::new()
            .get("http://127.0.0.1:1/servlet/WMLOGIN?sToken=SECRET-TOKEN&sIdno=20240000")
            .send()
            .await
            .expect_err("연결은 실패해야 합니다");
        assert!(failed.url().is_some(), "원래 오류에는 주소가 있습니다");
        let scrubbed = CertiError::from(failed);
        let message = scrubbed.to_string();
        for leak in ["SECRET-TOKEN", "sToken", "20240000", "127.0.0.1"] {
            assert!(
                !message.contains(leak),
                "{leak} 가 메시지에 남았습니다: {message}"
            );
        }
        assert!(matches!(&scrubbed, CertiError::Request(e) if e.url().is_none()));
    }

    #[test]
    fn reads_print_session_from_exe_check() {
        let html =
            "<script>opener.preview1('장학금지급증명서','2026ABCD','session-77','1');</script>";
        assert_eq!(print_session(html).expect("tpid"), "session-77");
        for broken in [
            "<script>window.close();</script>",
            "<script>opener.preview1('a','b','');</script>",
        ] {
            assert!(
                print_session(broken).is_err(),
                "{broken} 는 거부해야 합니다"
            );
        }
    }

    #[test]
    fn decodes_launch_envelope() {
        assert_eq!(
            launch_envelope(r#"{"res":"aGVsbG8="}"#).expect("decoded"),
            b"hello"
        );
        for broken in [
            r#"{"res":""}"#,
            r#"{"other":"aGVsbG8="}"#,
            "<html>",
            r#"{"res":"!!!"}"#,
        ] {
            assert!(
                launch_envelope(broken).is_err(),
                "{broken} 는 거부해야 합니다"
            );
        }
    }

    #[test]
    fn verification_needs_success_and_matching_document() {
        assert!(verified(br#"{"SUCC":"Y","MIN_NO":"2026ABCD"}"#, "2026ABCD"));
        for body in [
            br#"{"SUCC":"N","MIN_NO":"2026ABCD"}"#.as_slice(),
            br#"{"SUCC":"Y","MIN_NO":"2026OTHER"}"#.as_slice(),
            br#"{"SUCC":"Y"}"#.as_slice(),
            b"<html>error</html>".as_slice(),
        ] {
            assert!(!verified(body, "2026ABCD"));
        }
    }

    #[test]
    fn document_paths_are_resolved_from_one_table() {
        assert_eq!(path_of("URLFile"), "/wm1.0/jsp/report/sendfile.jsp");
        assert_eq!(path_of("URLPost"), "/wm1.0/jsp/report/printcomplete.jsp");
    }

    #[test]
    fn basket_fields_carry_institution_and_menu() {
        let fields = native_basket_fields();
        assert_eq!(fields.get("GIWANNO").map(String::as_str), Some(INSTITUTION));
        assert_eq!(
            fields.get("SERVICE_TYPE").map(String::as_str),
            Some("INTERNET")
        );
        assert_eq!(fields.get("LEFTNUM").map(String::as_str), Some("2"));
    }
}
