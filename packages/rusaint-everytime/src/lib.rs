//! 에브리타임(everytime.kr) 비공식 클라이언트입니다. **강의평 조회 전용**입니다.
//!
//! 에타 로그인은 reCAPTCHA Enterprise 와 새 기기 2단계 인증(앱 승인)을 거치므로 서버가 대신 로그인하지 않습니다.
//! 사용자가 자기 브라우저에서 로그인(로그인 유지 체크 시 30일)한 뒤 받은 세션 쿠키 `etsid` 를 넘겨받아
//! 메모리에만 두고 씁니다. 세션이 끝나면 되살리지 않고 다시 넣어 달라고 알립니다.
//! 강의평을 읽기만 하며, 글쓰기·시간표·게시판 등 다른 기능은 호출하지 않습니다.

mod html;
mod model;

use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;

pub use model::*;

const PUBLIC: &str = "https://everytime.kr";
const API: &str = "https://api.everytime.kr";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
/// 강의평 본문을 한 번에 가져오는 최대 개수
pub const MAX_ARTICLES: u32 = 20;
/// 세션 쿠키 이름
pub const SESSION_COOKIE: &str = "etsid";

/// 에브리타임 API 오류
#[derive(Debug, thiserror::Error)]
pub enum EverytimeError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("응답 형식이 예상과 다릅니다: {0}")]
    Decode(String),
    #[error(
        "에브리타임 세션이 끝났어요. 브라우저에서 에타에 다시 로그인(로그인 유지 체크)하고, 커넥터를 다시 연결해 새 쿠키를 넣어 주세요."
    )]
    SessionExpired,
    #[error("에브리타임 쿠키 형식이 올바르지 않아요: {0}")]
    InvalidCookie(String),
    #[error("{0}")]
    NotFound(String),
}

/// 에브리타임 클라이언트. 사용자가 넘긴 세션 쿠키는 내부 쿠키 저장소(메모리)에만 보관됩니다.
pub struct EverytimeClient {
    http: reqwest::Client,
    campus_id: Mutex<Option<i64>>,
    /// 한 번 만료가 확인되면 같은 쿠키로 더 요청하지 않습니다.
    expired: Mutex<bool>,
}

/// 붙여넣은 값에서 `etsid` 값만 꺼냅니다. `etsid=…`, 앞뒤 공백·따옴표, 세미콜론 뒤 속성을 허용합니다.
pub fn parse_session_cookie(input: &str) -> Result<String, EverytimeError> {
    let mut value = input.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    // "etsid=값; Domain=…" 처럼 통째로 붙여넣은 경우
    if let Some(pos) = value.find(&format!("{SESSION_COOKIE}=")) {
        value = &value[pos + SESSION_COOKIE.len() + 1..];
    }
    let value = value.split(';').next().unwrap_or("").trim();
    if value.is_empty() {
        return Err(EverytimeError::InvalidCookie("값이 비어 있어요".into()));
    }
    if value.len() > 1024 {
        return Err(EverytimeError::InvalidCookie("값이 너무 길어요".into()));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b',' && b != b'"' && b != b'\\')
    {
        return Err(EverytimeError::InvalidCookie(
            "쿠키 값에 쓸 수 없는 문자가 있어요".into(),
        ));
    }
    Ok(value.to_string())
}

impl EverytimeClient {
    /// 브라우저에서 받은 세션 쿠키(`etsid` 값 또는 `etsid=…`)로 클라이언트를 만듭니다. 아직 서버에 확인하지는 않습니다.
    pub fn from_session(cookie: &str) -> Result<Self, EverytimeError> {
        let value = parse_session_cookie(cookie)?;
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let url: reqwest::Url = PUBLIC.parse().expect("고정 주소");
        jar.add_cookie_str(
            &format!("{SESSION_COOKIE}={value}; Domain=.everytime.kr; Path=/; Secure; HttpOnly"),
            &url,
        );
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .cookie_provider(jar)
            // 리다이렉트를 따라가지 않아야 세션 만료(로그인 화면으로 3xx)를 알아챌 수 있습니다.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            campus_id: Mutex::new(None),
            expired: Mutex::new(false),
        })
    }

    /// 세션이 살아 있는지 확인합니다(강의실 검색 화면을 열어 캠퍼스를 읽음).
    pub async fn verify(&self) -> Result<(), EverytimeError> {
        self.campus().await.map(|_| ())
    }

    async fn check_alive(&self) -> Result<(), EverytimeError> {
        if *self.expired.lock().await {
            return Err(EverytimeError::SessionExpired);
        }
        Ok(())
    }

    async fn mark_expired(&self) -> EverytimeError {
        *self.expired.lock().await = true;
        EverytimeError::SessionExpired
    }

    /// 현재 캠퍼스 id 를 `/lecture/search` 초기 상태에서 한 번 읽어 둡니다.
    async fn campus(&self) -> Result<i64, EverytimeError> {
        if let Some(id) = *self.campus_id.lock().await {
            return Ok(id);
        }
        self.check_alive().await?;
        let res = self
            .http
            .get(format!("{PUBLIC}/lecture/search"))
            .header(reqwest::header::REFERER, format!("{PUBLIC}/lecture"))
            .send()
            .await?;
        if !res.status().is_success() {
            eprintln!(
                "everytime campus: status={} location={}",
                res.status().as_u16(),
                location(&res).as_deref().unwrap_or("-")
            );
            return Err(self.mark_expired().await);
        }
        let html = res.text().await?;
        let Some(id) = Self::parse_campus_id(&html) else {
            // 로그인하지 않은 상태로도 200 을 주는 경우가 있어, 캠퍼스가 없으면 세션이 없는 것으로 봅니다.
            eprintln!(
                "everytime campus: 캠퍼스 없음 len={} login_link={}",
                html.len(),
                html.contains("account.everytime.kr/login")
            );
            return Err(self.mark_expired().await);
        };
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
        self.check_alive().await?;
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
        let status = res.status();
        if !status.is_success() {
            eprintln!(
                "everytime api {path}: status={} location={}",
                status.as_u16(),
                location(&res).as_deref().unwrap_or("-")
            );
            if status.is_redirection() || status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(self.mark_expired().await);
            }
            return Err(EverytimeError::Decode(format!("HTTP {}", status.as_u16())));
        }
        let text = res.text().await?;
        serde_json::from_str(&text).map_err(|e| {
            eprintln!(
                "everytime api {path}: JSON 아님 len={} snippet={:?}",
                text.len(),
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(160)
                    .collect::<String>()
            );
            EverytimeError::Decode(e.to_string())
        })
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

fn location(res: &reqwest::Response) -> Option<String> {
    res.headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
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
    fn parses_pasted_session_cookie() {
        assert_eq!(parse_session_cookie("s%3Aabc.def").unwrap(), "s%3Aabc.def");
        assert_eq!(
            parse_session_cookie(" etsid=s%3Aabc.def; Domain=.everytime.kr; Path=/ ").unwrap(),
            "s%3Aabc.def"
        );
        assert_eq!(parse_session_cookie("\"s%3Aabc\"").unwrap(), "s%3Aabc");
        assert!(parse_session_cookie("  ").is_err());
        assert!(parse_session_cookie("a b").is_err());
    }

    #[test]
    fn id_helpers_accept_number_or_string() {
        assert_eq!(as_id(Some(&Value::from(12345))), Some("12345".into()));
        assert_eq!(as_id(Some(&Value::from("678"))), Some("678".into()));
        assert_eq!(as_f64(Some(&Value::from("4.1"))), Some(4.1));
        assert_eq!(as_i64(Some(&Value::from(3))), Some(3));
    }
}
