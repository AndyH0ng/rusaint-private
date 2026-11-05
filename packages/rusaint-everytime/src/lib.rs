//! 에브리타임(everytime.kr) 비공식 클라이언트입니다. **강의평 조회 전용**입니다.
//!
//! 에타 계정은 u-saint SSO와 무관합니다. 아이디·비밀번호로 로그인해 받은 세션 쿠키를 메모리에만 두고,
//! 끊기면 한 번 다시 로그인합니다. 강의평을 읽기만 하며, 글쓰기·시간표·게시판 등 다른 기능은 호출하지 않습니다.
//!
//! 요청·응답 형식은 2026년 기준 공개된 비공식 구현을 참고했습니다. 학교 서버가 아니라 민간 서비스이므로
//! 캡차·추가 인증이 뜨면 우회하지 않고 오류를 돌려줍니다.

mod html;
mod model;

use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;

pub use model::*;

const PUBLIC: &str = "https://everytime.kr";
const API_HOST: &str = "api.everytime.kr";
const API: &str = "https://api.everytime.kr";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 강의평 본문을 한 번에 가져오는 최대 개수
pub const MAX_ARTICLES: u32 = 20;

/// 에브리타임 API 오류
#[derive(Debug, thiserror::Error)]
pub enum EverytimeError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("응답 형식이 예상과 다릅니다: {0}")]
    Decode(String),
    #[error("에브리타임 로그인에 실패했어요. 아이디와 비밀번호를 확인해 주세요.")]
    LoginFailed,
    #[error(
        "에브리타임 추가 인증이 필요해요. everytime.kr 에서 직접 한 번 로그인한 뒤 다시 시도해 주세요."
    )]
    SecondaryAuthRequired,
    /// 아이디·비밀번호가 비어 있거나 이미 서버에서 거부됨. 계정 잠금을 막기 위해 다시 보내지 않습니다.
    #[error("에브리타임 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    #[error("{0}")]
    NotFound(String),
}

/// 에브리타임 클라이언트. 세션 쿠키는 내부 쿠키 저장소(메모리)에만 보관됩니다.
pub struct EverytimeClient {
    http: reqwest::Client,
    userid: String,
    password: String,
    logged_in: Mutex<bool>,
    campus_id: Mutex<Option<i64>>,
    /// 서버가 계정 정보를 거부한 사유. 한 번 거부되면 이 클라이언트로는 다시 로그인하지 않습니다.
    rejected: Mutex<Option<String>>,
}

impl EverytimeClient {
    /// 아직 로그인하지 않은 클라이언트를 만듭니다. 첫 요청 때 로그인합니다.
    pub fn new(
        userid: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, EverytimeError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .cookie_store(true)
            .user_agent("Mozilla/5.0")
            .build()?;
        Ok(Self {
            http,
            userid: userid.into(),
            password: password.into(),
            logged_in: Mutex::new(false),
            campus_id: Mutex::new(None),
            rejected: Mutex::new(None),
        })
    }

    /// 로그인해서 세션 쿠키를 채웁니다. 실패(아이디·비밀번호 불일치)는 다시 시도하지 않도록 기록합니다.
    pub async fn login(&self) -> Result<(), EverytimeError> {
        if self.userid.trim().is_empty() || self.password.is_empty() {
            return Err(EverytimeError::CredentialUnusable(
                "아이디 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        if let Some(reason) = self.rejected.lock().await.clone() {
            return Err(EverytimeError::CredentialUnusable(reason));
        }
        let form = urlencode(&[
            ("userid", self.userid.clone()),
            ("password", self.password.clone()),
            ("redirect", "/".to_string()),
        ]);
        let body = self
            .http
            .post(format!("{PUBLIC}/user/login"))
            .header(reqwest::header::ORIGIN, PUBLIC)
            .header(reqwest::header::REFERER, format!("{PUBLIC}/login"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await?
            .text()
            .await?;
        // 로그인 실패 페이지는 alert 뒤 history.go(-1) 로 되돌립니다.
        if body.contains("history.go(-1)") {
            let reason = "아이디 또는 비밀번호가 올바르지 않습니다".to_string();
            *self.rejected.lock().await = Some(reason);
            return Err(EverytimeError::LoginFailed);
        }
        *self.logged_in.lock().await = true;
        Ok(())
    }

    async fn ensure_login(&self) -> Result<(), EverytimeError> {
        if *self.logged_in.lock().await {
            return Ok(());
        }
        self.login().await
    }

    /// 현재 캠퍼스 id 를 `/lecture/search` 초기 상태에서 한 번 읽어 둡니다.
    async fn campus(&self) -> Result<i64, EverytimeError> {
        if let Some(id) = *self.campus_id.lock().await {
            return Ok(id);
        }
        self.ensure_login().await?;
        let res = self
            .http
            .get(format!("{PUBLIC}/lecture/search"))
            .header(reqwest::header::REFERER, format!("{PUBLIC}/lecture"))
            .send()
            .await?;
        if res.url().host_str() != Some("everytime.kr") {
            return Err(EverytimeError::SecondaryAuthRequired);
        }
        let html = res.text().await?;
        let id = Self::parse_campus_id(&html)
            .ok_or_else(|| EverytimeError::Decode("현재 캠퍼스를 확인하지 못했어요".into()))?;
        *self.campus_id.lock().await = Some(id);
        Ok(id)
    }

    fn parse_campus_id(html: &str) -> Option<i64> {
        let re =
            Regex::new(r#"(?s)<script[^>]+id="__INITIAL_STATE__"[^>]*>(.*?)</script>"#).ok()?;
        let json = re.captures(html)?.get(1)?.as_str().trim();
        let value: Value = serde_json::from_str(json).ok()?;
        find_campus_id(&value)
    }

    /// API(application/x-www-form-urlencoded 전송, JSON 응답)를 호출합니다.
    async fn api_json(
        &self,
        path: &str,
        form: &[(&str, String)],
        referer: &str,
    ) -> Result<Value, EverytimeError> {
        self.ensure_login().await?;
        let res = self
            .http
            .post(format!("{API}{path}"))
            .header(reqwest::header::ORIGIN, PUBLIC)
            .header(reqwest::header::REFERER, referer)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(urlencode(form))
            .send()
            .await?;
        // 로그인·추가 인증이 필요하면 api 가 아닌 로그인 화면으로 리다이렉트됩니다.
        if res.url().host_str() != Some(API_HOST) {
            return Err(EverytimeError::SecondaryAuthRequired);
        }
        let text = res.text().await?;
        serde_json::from_str(&text).map_err(|e| EverytimeError::Decode(e.to_string()))
    }

    /// 강의평이 등록된 과목을 검색합니다. `field` 는 "name"(과목명) 또는 "professor"(교수명)입니다.
    pub async fn search_lectures(
        &self,
        keyword: &str,
        field: &str,
        limit: u32,
    ) -> Result<Vec<LectureHit>, EverytimeError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Err(EverytimeError::NotFound("검색어를 입력해 주세요".into()));
        }
        let field = match field {
            "professor" => "professor",
            _ => "name",
        };
        let limit = limit.clamp(1, 100);
        let campus = self.campus().await?;
        let value = self
            .api_json(
                "/find/lecture/list/keyword",
                &[
                    ("campusId", campus.to_string()),
                    ("field", field.to_string()),
                    ("keyword", keyword.to_string()),
                    ("limit", limit.to_string()),
                    ("offset", "0".to_string()),
                ],
                &format!("{PUBLIC}/lecture/search"),
            )
            .await?;
        let lectures = value
            .get("lectures")
            .and_then(Value::as_array)
            .ok_or_else(|| EverytimeError::Decode("과목 검색 결과를 읽지 못했어요".into()))?;
        let mut hits = Vec::new();
        for item in lectures.iter().take(limit as usize) {
            let (Some(id), Some(name)) = (as_id(item.get("id")), as_text(item.get("name"))) else {
                continue;
            };
            hits.push(LectureHit {
                id,
                name,
                professor: as_text(item.get("professor")),
                rate: as_f64(item.get("rate")),
            });
        }
        if hits.is_empty() {
            return Err(EverytimeError::NotFound(format!(
                "'{keyword}' 로 등록된 강의평이 없어요"
            )));
        }
        Ok(hits)
    }

    /// 한 과목(에타 강의 id)의 강의평을 가져옵니다. 최신 본문 최대 [`MAX_ARTICLES`] 개와 집계를 돌려줍니다.
    pub async fn lecture_reviews(&self, lecture_id: &str) -> Result<LectureReview, EverytimeError> {
        let lecture_id = lecture_id.trim();
        if lecture_id.is_empty() {
            return Err(EverytimeError::NotFound("강의 id 를 입력해 주세요".into()));
        }
        let referer = format!("{PUBLIC}/lecture/view/{lecture_id}");
        let list = self
            .api_json(
                "/find/lecture/article/list",
                &[
                    ("lectureId", lecture_id.to_string()),
                    ("limit", MAX_ARTICLES.to_string()),
                    ("offset", "0".to_string()),
                    ("sort", "id".to_string()),
                ],
                &referer,
            )
            .await?;
        let rate = list.get("rate");
        let count = rate.and_then(|r| as_i64(r.get("count"))).unwrap_or(0);
        let average = rate.and_then(|r| as_f64(r.get("average")));
        let mut articles = Vec::new();
        if let Some(items) = list.get("articles").and_then(Value::as_array) {
            for item in items.iter().take(MAX_ARTICLES as usize) {
                let Some(text) = item.get("text").and_then(Value::as_str) else {
                    continue;
                };
                articles.push(ReviewArticle {
                    year: as_i64(item.get("year")),
                    semester: as_i64(item.get("semester")),
                    text: html::to_text(text),
                    rate: as_f64(item.get("rate")),
                    posvote: as_i64(item.get("posvote")),
                });
            }
        }

        let overview = self
            .api_json("/find/lecture", &[("id", lecture_id.to_string())], &referer)
            .await?;
        let mut statistics = Vec::new();
        if let Some(review) = overview.get("review") {
            for group in ["subjectiveDetails", "objectiveDetails"] {
                let Some(details) = review.get(group).and_then(Value::as_array) else {
                    continue;
                };
                for detail in details {
                    let items = detail
                        .get("items")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|it| {
                                    Some(ReviewStatItem {
                                        text: as_text(it.get("text"))?,
                                        count: as_i64(it.get("count")).unwrap_or(0),
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    statistics.push(ReviewStat {
                        name: as_text(detail.get("name")),
                        count: as_i64(detail.get("count")).unwrap_or(0),
                        items,
                    });
                }
            }
        }

        Ok(LectureReview {
            lecture_id: lecture_id.to_string(),
            average,
            count,
            articles,
            statistics,
        })
    }

    /// 과목명/교수명으로 검색해 가장 잘 맞는 과목의 강의평을 한 번에 가져옵니다.
    pub async fn find_reviews(
        &self,
        keyword: &str,
        field: &str,
    ) -> Result<(LectureHit, LectureReview), EverytimeError> {
        let hits = self.search_lectures(keyword, field, 1).await?;
        let hit = hits.into_iter().next().ok_or_else(|| {
            EverytimeError::NotFound(format!("'{keyword}' 로 등록된 강의평이 없어요"))
        })?;
        let review = self.lecture_reviews(&hit.id).await?;
        Ok((hit, review))
    }
}

/// application/x-www-form-urlencoded 본문을 만듭니다(공백은 `+`, 그 밖의 비영숫자는 %HH).
fn urlencode(pairs: &[(&str, String)]) -> String {
    fn encode(s: &str, out: &mut String) {
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode(k, &mut out);
        out.push('=');
        encode(v, &mut out);
    }
    out
}

/* ---- JSON 값 보조 함수 (id 가 숫자/문자 어느 쪽이어도 처리) ---- */

fn as_id(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn as_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 초기 상태 JSON 어딘가에 있는 campusId 를 찾습니다.
fn find_campus_id(value: &Value) -> Option<i64> {
    match value {
        Value::Object(map) => {
            if let Some(id) = map.get("campusId").and_then(|v| as_i64(Some(v))) {
                return Some(id);
            }
            map.values().find_map(find_campus_id)
        }
        Value::Array(arr) => arr.iter().find_map(find_campus_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_campus_id_from_initial_state() {
        let html = r#"<html><head>
            <script id="__INITIAL_STATE__" type="application/json">{"lecture":{"campusId":15,"foo":"bar"}}</script>
            </head></html>"#;
        assert_eq!(EverytimeClient::parse_campus_id(html), Some(15));
    }

    #[test]
    fn id_helpers_accept_number_or_string() {
        assert_eq!(as_id(Some(&Value::from(12345))), Some("12345".into()));
        assert_eq!(as_id(Some(&Value::from("678"))), Some("678".into()));
        assert_eq!(as_f64(Some(&Value::from("4.1"))), Some(4.1));
        assert_eq!(as_i64(Some(&Value::from(3))), Some(3));
    }
}
