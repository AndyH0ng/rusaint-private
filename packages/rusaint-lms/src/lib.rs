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
        let token = self.issue_token().await?;
        *self.session.lock().await = Some(Session {
            token,
            issued: Instant::now(),
        });
        Ok(())
    }

    async fn issue_token(&self) -> Result<String, LmsError> {
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
        self.follow(&http, from_cc, "https://lms.ssu.ac.kr/")
            .await?;

        let canvas = Url::parse(CANVAS_BASE_URL).expect("static url");
        jar.cookies(&canvas)
            .and_then(|header| header.to_str().ok().map(str::to_owned))
            .and_then(|header| {
                header
                    .split("; ")
                    .find_map(|pair| pair.strip_prefix("xn_api_token=").map(str::to_owned))
            })
            .filter(|token| !token.is_empty())
            .ok_or_else(|| LmsError::Login("canvas 가 xn_api_token 을 발급하지 않았습니다".into()))
    }

    /// 리다이렉트를 최대 몇 번까지 직접 따라가며 쿠키를 모읍니다.
    async fn follow(
        &self,
        http: &reqwest::Client,
        mut url: Url,
        referer: &str,
    ) -> Result<(), LmsError> {
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
                    None => return Ok(()),
                }
                continue;
            }
            if !status.is_success() {
                return Err(LmsError::Status {
                    status: status.as_u16(),
                    url: url.path().to_string(),
                });
            }
            return Ok(());
        }
        Ok(())
    }

    async fn token(&self, force: bool) -> Result<String, LmsError> {
        let mut session = self.session.lock().await;
        let fresh = session
            .as_ref()
            .is_some_and(|s| s.issued.elapsed() < TOKEN_LIFETIME);
        if force || !fresh {
            *session = Some(Session {
                token: self.issue_token().await?,
                issued: Instant::now(),
            });
        }
        Ok(session.as_ref().expect("set above").token.clone())
    }

    /// LearningX API GET. 401 이면 한 번 다시 로그인합니다.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, LmsError> {
        for attempt in 0..2 {
            let token = self.token(attempt == 1).await?;
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

    #[tokio::test]
    async fn empty_credentials_are_never_sent() {
        let client = LmsClient::new("20240000", "").unwrap();
        assert!(matches!(
            client.login().await,
            Err(LmsError::CredentialUnusable(_))
        ));
    }
}
