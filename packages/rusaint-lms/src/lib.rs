//! 숭실대학교 LMS(canvas.ssu.ac.kr, LearningX) 클라이언트입니다.
//!
//! 로그인 흐름 (2026-11 브라우저에서 실측):
//! 1. SmartID 로그인으로 `sToken` 발급 ([`rusaint::obtain_ssu_sso_token`])
//! 2. `lms.ssu.ac.kr/xn-sso/gw-cb.php?sToken=&sIdno=` → 302 `lms.ssu.ac.kr/login/callback?result=…`
//! 3. `canvas.ssu.ac.kr/learningx/login/from_cc?result=…` 가 `xn_api_token`(JWT, 2시간) 쿠키를 발급
//! 4. LearningX API 는 `Authorization: Bearer <xn_api_token>` 이 있어야 응답합니다 (쿠키만으로는 401)

mod model;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{AUTHORIZATION, LOCATION, REFERER};
use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use url::Url;

pub use model::*;

const LMS_GATEWAY_URL: &str = "https://lms.ssu.ac.kr/xn-sso/gw-cb.php";
const CANVAS_BASE_URL: &str = "https://canvas.ssu.ac.kr";
const COMMONS_BASE_URL: &str = "https://commons.ssu.ac.kr";
const USER_AGENT: &str = concat!(
    "rusaint-lms/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// `xn_api_token` 은 2시간짜리라 여유를 두고 갱신합니다.
const TOKEN_LIFETIME: Duration = Duration::from_secs(110 * 60);
const MAX_REDIRECTS: usize = 5;

/// LMS 오류
#[derive(Debug, thiserror::Error)]
pub enum LmsError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("응답 형식이 예상과 다릅니다: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("파일 저장 실패: {0}")]
    Io(#[from] std::io::Error),
    /// SmartID 가 계정 정보를 거부했거나 비어 있음. 계정 잠금을 막기 위해 다시 시도하지 않습니다.
    #[error("통합 로그인 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    #[error("LMS 로그인 단계 실패: {0}")]
    Login(String),
    #[error("LMS 가 HTTP {status} 을(를) 반환했습니다: {url}")]
    Status { status: u16, url: String },
    #[error("자료 다운로드 주소를 찾을 수 없습니다: {0}")]
    NoDownload(String),
}

struct Session {
    token: String,
    issued: Instant,
    /// Canvas 웹 세션 쿠키를 가진 클라이언트. Canvas 기본 API(공지 등)는 Bearer 토큰을 받지 않습니다.
    canvas: Option<reqwest::Client>,
}

/// LMS 클라이언트. 처음 API 를 호출할 때 로그인하고, 토큰이 만료되면 다시 로그인합니다.
pub struct LmsClient {
    student_id: String,
    password: String,
    session: Mutex<Option<Session>>,
    rejected: Mutex<Option<String>>,
    api: reqwest::Client,
}

impl LmsClient {
    pub fn new(
        student_id: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, LmsError> {
        let api = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            student_id: student_id.into(),
            password: password.into(),
            session: Mutex::new(None),
            rejected: Mutex::new(None),
            api,
        })
    }

    pub fn student_id(&self) -> &str {
        &self.student_id
    }

    /// 로그인해서 새 `xn_api_token` 을 받습니다.
    pub async fn login(&self) -> Result<(), LmsError> {
        *self.session.lock().await = Some(self.open_session().await?);
        Ok(())
    }

    async fn open_session(&self) -> Result<Session, LmsError> {
        if self.student_id.trim().is_empty() || self.password.is_empty() {
            return Err(LmsError::CredentialUnusable(
                "학번 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        if let Some(reason) = self.rejected.lock().await.clone() {
            return Err(LmsError::CredentialUnusable(reason));
        }

        let s_token = match rusaint::obtain_ssu_sso_token(&self.student_id, &self.password).await {
            Ok(token) => token,
            Err(rusaint::SsuSsoError::CantFindToken(message)) => {
                // SmartID 가 경고창으로 거부한 경우(비밀번호 오류 등). 반복하면 계정이 잠길 수 있습니다.
                *self.rejected.lock().await = Some(message.clone());
                return Err(LmsError::CredentialUnusable(message));
            }
            Err(e) => return Err(LmsError::Login(format!("SmartID: {e}"))),
        };

        let jar = Arc::new(Jar::default());
        let gateway = Url::parse(LMS_GATEWAY_URL).expect("static url");
        for (name, value) in [
            ("sToken", s_token.as_str()),
            ("sIdno", self.student_id.as_str()),
        ] {
            jar.add_cookie_str(
                &format!("{name}={value}; Domain=.ssu.ac.kr; Path=/"),
                &gateway,
            );
        }
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .cookie_provider(jar.clone())
            .redirect(Policy::none())
            .build()?;

        // 2. 게이트웨이 → callback?result=…
        let response = http
            .get(gateway.clone())
            .query(&[
                ("sToken", s_token.as_str()),
                ("sIdno", self.student_id.as_str()),
            ])
            .send()
            .await?;
        let callback = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| gateway.join(loc).ok())
            .ok_or_else(|| {
                LmsError::Login(format!(
                    "gw-cb.php 가 리다이렉트하지 않았습니다 (HTTP {})",
                    response.status()
                ))
            })?;
        let result = callback
            .query_pairs()
            .find(|(k, _)| k == "result")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| LmsError::Login("callback 주소에 result 가 없습니다".into()))?;
        // 브라우저와 같은 순서로 LMS 쪽 세션도 만들어 둡니다.
        self.follow(&http, callback, "https://smartid.ssu.ac.kr/")
            .await?;

        // 3. canvas 가 xn_api_token 을 발급
        let mut from_cc =
            Url::parse(&format!("{CANVAS_BASE_URL}/learningx/login/from_cc")).expect("static url");
        from_cc.query_pairs_mut().append_pair("result", &result);
        let from_cc_page = self
            .follow(&http, from_cc.clone(), "https://lms.ssu.ac.kr/")
            .await?;

        let canvas = Url::parse(CANVAS_BASE_URL).expect("static url");
        let token = jar
            .cookies(&canvas)
            .and_then(|header| header.to_str().ok().map(str::to_owned))
            .and_then(|header| {
                header
                    .split("; ")
                    .find_map(|pair| pair.strip_prefix("xn_api_token=").map(str::to_owned))
            })
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                LmsError::Login("canvas 가 xn_api_token 을 발급하지 않았습니다".into())
            })?;

        // 4. from_cc 페이지가 자동 제출하는 폼(학번 + 1회용 값)으로 Canvas 웹 세션을 엽니다.
        let canvas_client = match login_form(&from_cc_page) {
            Some(mut fields) => {
                // 비밀번호 칸은 페이지의 loginCryption(암호문, 개인키) 가 채웁니다.
                if let Some(password) = login_cryption_password(&from_cc_page) {
                    for (name, value) in &mut fields {
                        if name == "pseudonym_session[password]" {
                            *value = password.clone();
                        }
                    }
                }
                let response = http
                    .post(format!("{CANVAS_BASE_URL}/login/canvas"))
                    .header(REFERER, from_cc.as_str())
                    .header(reqwest::header::ORIGIN, CANVAS_BASE_URL)
                    .form(&fields)
                    .send()
                    .await?;
                if response.status().is_redirection() || response.status().is_success() {
                    Some(
                        reqwest::Client::builder()
                            .user_agent(USER_AGENT)
                            .timeout(REQUEST_TIMEOUT)
                            .cookie_provider(jar.clone())
                            .build()?,
                    )
                } else {
                    None
                }
            }
            None => None,
        };

        Ok(Session {
            token,
            issued: Instant::now(),
            canvas: canvas_client,
        })
    }

    /// 리다이렉트를 최대 몇 번까지 직접 따라가며 쿠키를 모읍니다.
    async fn follow(
        &self,
        http: &reqwest::Client,
        mut url: Url,
        referer: &str,
    ) -> Result<String, LmsError> {
        for _ in 0..MAX_REDIRECTS {
            let response = http
                .get(url.clone())
                .header(REFERER, referer)
                .send()
                .await?;
            let status = response.status();
            if status.is_redirection() {
                let next = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|loc| url.join(loc).ok());
                match next {
                    Some(next) => url = next,
                    None => return Ok(String::new()),
                }
                continue;
            }
            if !status.is_success() {
                return Err(LmsError::Status {
                    status: status.as_u16(),
                    url: url.path().to_string(),
                });
            }
            return Ok(response.text().await?);
        }
        Ok(String::new())
    }

    /// 유효한 세션의 (토큰, Canvas 세션 클라이언트)를 돌려줍니다.
    async fn session(&self, force: bool) -> Result<(String, Option<reqwest::Client>), LmsError> {
        let mut session = self.session.lock().await;
        let fresh = session
            .as_ref()
            .is_some_and(|s| s.issued.elapsed() < TOKEN_LIFETIME);
        if force || !fresh {
            *session = Some(self.open_session().await?);
        }
        let s = session.as_ref().expect("set above");
        Ok((s.token.clone(), s.canvas.clone()))
    }

    /// LearningX API GET. 401 이면 한 번 다시 로그인합니다.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, LmsError> {
        for attempt in 0..2 {
            let (token, _) = self.session(attempt == 1).await?;
            let response = self
                .api
                .get(format!("{CANVAS_BASE_URL}{path}"))
                .query(query)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                continue;
            }
            if !response.status().is_success() {
                return Err(LmsError::Status {
                    status: response.status().as_u16(),
                    url: path.to_string(),
                });
            }
            return Ok(serde_json::from_slice(&response.bytes().await?)?);
        }
        unreachable!("loop returns on second attempt")
    }

    /// 수강 학기 목록. `default` 가 true 인 것이 현재 학기입니다.
    pub async fn terms(&self) -> Result<Vec<Term>, LmsError> {
        let path = format!("/learningx/api/v1/users/{}/terms", self.student_id);
        let page: TermPage = self
            .get(
                &path,
                &[("include_invited_course_contained", "true".into())],
            )
            .await?;
        Ok(page.enrollment_terms)
    }

    /// 학기의 수강 강의
    pub async fn courses(&self, term_id: u64) -> Result<Vec<Course>, LmsError> {
        self.get(
            "/learningx/api/v1/learn_activities/courses",
            &[("term_ids[]", term_id.to_string())],
        )
        .await
    }

    /// 학기의 강의별 할 일(미제출 과제·퀴즈, 미시청 영상 등)
    pub async fn todos(&self, term_id: u64) -> Result<Vec<CourseTodos>, LmsError> {
        let page: TodoPage = self
            .get(
                "/learningx/api/v1/learn_activities/to_dos",
                &[("term_ids[]", term_id.to_string())],
            )
            .await?;
        Ok(page.to_dos)
    }

    /// 강의의 주차별 모듈과 자료
    pub async fn modules(&self, course_id: u64) -> Result<Vec<Module>, LmsError> {
        self.get(
            &format!("/learningx/api/v1/courses/{course_id}/modules"),
            &[("include_detail", "true".into())],
        )
        .await
    }

    /// Canvas 기본 API GET (웹 세션 쿠키). 401 이면 한 번 다시 로그인합니다.
    async fn canvas_get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, LmsError> {
        for attempt in 0..2 {
            let (_, canvas) = self.session(attempt == 1).await?;
            let canvas = canvas.ok_or_else(|| {
                LmsError::Login("Canvas 웹 세션을 열지 못했습니다 (/login/canvas)".into())
            })?;
            let response = canvas
                .get(format!("{CANVAS_BASE_URL}{path}"))
                .query(query)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                continue;
            }
            if !response.status().is_success() {
                return Err(LmsError::Status {
                    status: response.status().as_u16(),
                    url: path.to_string(),
                });
            }
            // Canvas 는 JSON 앞에 `while(1);` 을 붙이기도 합니다.
            let body = response.text().await?;
            return Ok(serde_json::from_str(body.trim_start_matches("while(1);"))?);
        }
        unreachable!("loop returns on second attempt")
    }

    /// 강의 공지 목록 (최신순). 본문은 [`Self::announcement`] 로 가져옵니다.
    pub async fn announcements(&self, course_id: u64) -> Result<Vec<Announcement>, LmsError> {
        self.canvas_get(
            &format!("/api/v1/courses/{course_id}/discussion_topics"),
            &[
                ("only_announcements", "true".into()),
                ("per_page", "30".into()),
            ],
        )
        .await
    }

    /// 강의 공지 하나와 본문(텍스트)
    pub async fn announcement(
        &self,
        course_id: u64,
        topic_id: u64,
    ) -> Result<Announcement, LmsError> {
        let mut topic: Announcement = self
            .canvas_get(
                &format!("/api/v1/courses/{course_id}/discussion_topics/{topic_id}"),
                &[],
            )
            .await?;
        topic.message = topic.message.as_deref().map(html_to_text);
        Ok(topic)
    }

    /// 과목 게시판 목록 (LearningX 게시판)
    pub async fn boards(&self, course_id: u64) -> Result<Vec<Board>, LmsError> {
        self.get(
            &format!("/learningx/api/v1/learningx_board/courses/{course_id}/boards"),
            &[],
        )
        .await
    }

    /// 게시판 글 목록. 한 페이지에 서버가 정한 개수만큼 오며 본문은 텍스트로 바꿉니다.
    pub async fn posts(
        &self,
        course_id: u64,
        board_id: u64,
        page: u32,
    ) -> Result<PostPage, LmsError> {
        let mut page: PostPage = self
            .get(
                &format!(
                    "/learningx/api/v1/learningx_board/courses/{course_id}/boards/{board_id}/posts"
                ),
                &[("page", page.max(1).to_string())],
            )
            .await?;
        for post in &mut page.items {
            post.content = post.content.as_deref().map(html_to_text);
        }
        Ok(page)
    }

    /// 게시글 댓글
    pub async fn comments(
        &self,
        course_id: u64,
        board_id: u64,
        post_id: u64,
    ) -> Result<Vec<Comment>, LmsError> {
        let mut comments: Vec<Comment> = self
            .get(
                &format!(
                    "/learningx/api/v1/learningx_board/courses/{course_id}/boards/{board_id}/posts/{post_id}/comments"
                ),
                &[],
            )
            .await?;
        for comment in &mut comments {
            comment.content = comment.content.as_deref().map(html_to_text);
        }
        Ok(comments)
    }

    /// commons 자료 파일을 `dest` 에 저장하고 바이트 수를 돌려줍니다.
    /// 실측상 commons 는 로그인 없이 `content_id` 만으로 내려받을 수 있습니다.
    pub async fn download(&self, content_id: &str, dest: &Path) -> Result<u64, LmsError> {
        let xml = self
            .api
            .get(format!(
                "{COMMONS_BASE_URL}/viewer/ssplayer/uniplayer_support/content.php"
            ))
            .query(&[("content_id", content_id)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let uri = xml_text(&xml, "content_download_uri")
            .filter(|u| !u.is_empty())
            .ok_or_else(|| LmsError::NoDownload(content_id.to_string()))?;
        let url = Url::parse(COMMONS_BASE_URL)
            .expect("static url")
            .join(&uri.replace("&amp;", "&"))
            .map_err(|_| LmsError::NoDownload(content_id.to_string()))?;
        if url.host_str() != Some("commons.ssu.ac.kr") {
            return Err(LmsError::NoDownload(format!(
                "{content_id}: 예상하지 못한 주소"
            )));
        }

        let mut response = self.api.get(url).send().await?.error_for_status()?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut written = 0u64;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        tokio::fs::rename(&partial, dest).await?;
        Ok(written)
    }
}

/// from_cc 페이지의 `/login/canvas` 자동 제출 폼에서 (name, value) 를 꺼냅니다.
fn login_form(html: &str) -> Option<Vec<(String, String)>> {
    let form_start = html.find("/login/canvas")?;
    let form = &html[form_start..];
    let form = &form[..form.find("</form>").unwrap_or(form.len())];
    let fields: Vec<(String, String)> = form
        .split("<input")
        .skip(1)
        .filter_map(|tag| {
            let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
            Some((attr(tag, "name")?, attr(tag, "value").unwrap_or_default()))
        })
        .collect();
    fields
        .iter()
        .any(|(k, _)| k == "pseudonym_session[unique_id]")
        .then_some(fields)
}

/// 사이트의 `window.loginCryption(cipher, privateKey)` 와 같이 RSA(PKCS#1 v1.5)로 1회용 비밀번호를 풉니다.
/// 개인키는 페이지에 그대로 들어 있습니다 (learningx/js/login-cryption.js, JSEncrypt).
fn login_cryption_password(html: &str) -> Option<String> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rsa::pkcs1::DecodeRsaPrivateKey;

    let call = &html[html.find("loginCryption(\"")? + "loginCryption(\"".len()..];
    let cipher = &call[..call.find('"')?];
    let key = &call[call.find("-----BEGIN RSA PRIVATE KEY-----")?..];
    let key = &key[..key.find("-----END RSA PRIVATE KEY-----")?];
    let der: String = key
        .trim_start_matches("-----BEGIN RSA PRIVATE KEY-----")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let private_key = rsa::RsaPrivateKey::from_pkcs1_der(&STANDARD.decode(der).ok()?).ok()?;
    let plain = private_key
        .decrypt(rsa::Pkcs1v15Encrypt, &STANDARD.decode(cipher).ok()?)
        .ok()?;
    String::from_utf8(plain).ok()
}

/// 태그 안의 `name="value"` (작은따옴표 포함) 를 읽습니다.
fn attr(tag: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let key = format!("{name}={quote}");
        if let Some(start) = tag.find(&key).map(|i| i + key.len()) {
            let end = start + tag[start..].find(quote)?;
            return Some(html_escape::decode_html_entities(&tag[start..end]).into_owned());
        }
    }
    None
}

/// 게시글·공지 HTML 을 읽기 좋은 텍스트로 바꿉니다.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let Some(gt) = rest[lt..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[lt + 1..lt + gt]
            .trim_start_matches('/')
            .to_ascii_lowercase();
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        if matches!(
            name,
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4"
        ) {
            out.push('\n');
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(rest);
    let decoded = html_escape::decode_html_entities(&out).replace('\u{a0}', " ");
    let mut lines: Vec<&str> = Vec::new();
    for line in decoded.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

/// `<tag>값</tag>` 의 첫 값을 꺼냅니다 (CDATA 포함).
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    let value = xml[start..end].trim();
    let value = value
        .strip_prefix("<![CDATA[")
        .and_then(|v| v.strip_suffix("]]>"))
        .unwrap_or(value);
    Some(value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_text_reads_plain_and_cdata() {
        let xml = "<content><title><![CDATA[1주차 자료]]></title><content_download_uri>/index.php?a=1&amp;b=2</content_download_uri></content>";
        assert_eq!(xml_text(xml, "title").as_deref(), Some("1주차 자료"));
        assert_eq!(
            xml_text(xml, "content_download_uri").as_deref(),
            Some("/index.php?a=1&amp;b=2")
        );
        assert_eq!(xml_text(xml, "missing"), None);
    }

    #[test]
    fn parses_canvas_login_form() {
        let html = r#"<form id="f" action="https://canvas.ssu.ac.kr/login/canvas" method="post">
            <input type="hidden" name="utf8" value="&#x2713;">
            <input type='hidden' name='pseudonym_session[unique_id]' value='20240000'>
            <input type="hidden" name="pseudonym_session[password]" value="abc">
            <input type="hidden" name="after_login_url" value="">
        </form><input name="outside" value="x">"#;
        let fields = login_form(html).unwrap();
        assert_eq!(fields.len(), 4);
        assert!(fields.contains(&("utf8".into(), "✓".into())));
        assert!(fields.contains(&("pseudonym_session[unique_id]".into(), "20240000".into())));
        assert!(login_form("<form action=\"/other\"></form>").is_none());
    }

    #[test]
    fn decrypts_login_cryption_like_jsencrypt() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        use rsa::pkcs1::EncodeRsaPrivateKey;

        let mut rng = rsa::rand_core::OsRng;
        let key = rsa::RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let cipher = rsa::RsaPublicKey::from(&key)
            .encrypt(&mut rng, rsa::Pkcs1v15Encrypt, b"one-time-secret")
            .unwrap();
        // 페이지처럼 줄바꿈 없이 이어 붙인 PEM
        let pem: String = key
            .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .lines()
            .collect();
        let html = format!(
            "<script>window.loginCryption(\"{}\", \"{}\");</script>",
            STANDARD.encode(cipher),
            pem
        );
        assert_eq!(
            login_cryption_password(&html).as_deref(),
            Some("one-time-secret")
        );
        assert_eq!(login_cryption_password("<script></script>"), None);
    }

    #[test]
    fn html_to_text_keeps_paragraphs() {
        let html = "<p>안녕하세요 교수님!<br>질문이 있습니다 &amp; 확인 부탁드립니다.</p><p><br></p><p>감사합니다</p>";
        assert_eq!(
            html_to_text(html),
            "안녕하세요 교수님!\n질문이 있습니다 & 확인 부탁드립니다.\n\n감사합니다"
        );
    }

    #[tokio::test]
    async fn empty_credentials_are_never_sent() {
        let client = LmsClient::new("20240000", "").unwrap();
        assert!(matches!(
            client.login().await,
            Err(LmsError::CredentialUnusable(_))
        ));
    }
}
