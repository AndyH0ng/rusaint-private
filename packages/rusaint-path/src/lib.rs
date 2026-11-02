//! 숭실대학교 SSU-PATH(path.ssu.ac.kr) 비교과 프로그램 클라이언트입니다.
//!
//! 로그인 흐름 (2026-11 실측): 통합 로그인 버튼은 SmartID 로
//! `apiReturnUrl=https://path.ssu.ac.kr/comm/login/user/loginProc.do` 를 넘깁니다.
//! SmartID 가 돌려주는 `sToken`/`sIdno` 를 그 주소에 그대로 넘기면 PATH 세션이 열립니다.
//! 페이지는 서버에서 그린 HTML 이라 화면 구조를 파싱합니다.

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use tokio::sync::Mutex;
use url::Url;

const BASE_URL: &str = "https://path.ssu.ac.kr";
const LOGIN_PROC_PATH: &str = "/comm/login/user/loginProc.do";
const LIST_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtPgmList.do";
const DETAIL_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtPgmInfo.do";
const USER_AGENT: &str = concat!(
    "rusaint-path/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    /// SmartID 가 계정 정보를 거부했거나 비어 있음. 계정 잠금을 막기 위해 다시 시도하지 않습니다.
    #[error("통합 로그인 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    #[error("SSU-PATH 로그인 실패: {0}")]
    Login(String),
}

/// 비교과 프로그램 목록의 한 항목
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Program {
    /// 상세 조회에 쓰는 id (`encSddpbSeq`)
    pub id: String,
    /// 모집중, 모집대기, 종료 등
    pub status: Option<String>,
    pub organization: Option<String>,
    /// 비교과 (비학점) 등
    pub form: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub apply_period: Option<String>,
    pub program_period: Option<String>,
    /// 신청 대상 (학교 전체 또는 학과)
    pub target: Option<String>,
    /// 신청 신분 (학생, 일반인 …)
    pub target_identity: Option<String>,
    pub mileage: Option<u32>,
    pub applicants: Option<u32>,
    pub waiting: Option<u32>,
    pub capacity: Option<u32>,
    /// 정원 − 신청자. 대기자 신청이 가능한 프로그램도 있습니다.
    pub seats_left: Option<i64>,
    pub abilities: Vec<String>,
}

/// 목록 한 페이지
#[derive(Debug, Clone, Serialize)]
pub struct ProgramPage {
    pub page: u32,
    /// 화면에 보이는 마지막 페이지 번호 (페이지 버튼 기준)
    pub last_page: u32,
    pub items: Vec<Program>,
}

/// 목록 검색 조건
#[derive(Debug, Clone, Default)]
pub struct ProgramQuery {
    /// true 면 모집중만
    pub recruiting_only: bool,
    /// 대상 학년 (1~4)
    pub grade: Option<u8>,
    pub keyword: Option<String>,
    pub page: u32,
}

/// PATH 첫 화면의 나의 기본정보
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Profile {
    /// 재학, 휴학 등
    pub status: Option<String>,
    pub department: Option<String>,
    pub grade: Option<u8>,
}

pub struct PathClient {
    student_id: String,
    password: String,
    session: Mutex<Option<reqwest::Client>>,
    rejected: Mutex<Option<String>>,
}

impl PathClient {
    pub fn new(student_id: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            student_id: student_id.into(),
            password: password.into(),
            session: Mutex::new(None),
            rejected: Mutex::new(None),
        }
    }

    async fn open_session(&self) -> Result<reqwest::Client, PathError> {
        if self.student_id.trim().is_empty() || self.password.is_empty() {
            return Err(PathError::CredentialUnusable(
                "학번 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        if let Some(reason) = self.rejected.lock().await.clone() {
            return Err(PathError::CredentialUnusable(reason));
        }
        let s_token = match rusaint::obtain_ssu_sso_token(&self.student_id, &self.password).await {
            Ok(token) => token,
            Err(rusaint::SsuSsoError::CantFindToken(message)) => {
                *self.rejected.lock().await = Some(message.clone());
                return Err(PathError::CredentialUnusable(message));
            }
            Err(e) => return Err(PathError::Login(format!("SmartID: {e}"))),
        };

        let base = Url::parse(BASE_URL).expect("static url");
        let jar = Arc::new(Jar::default());
        for (name, value) in [
            ("sToken", s_token.as_str()),
            ("sIdno", self.student_id.as_str()),
        ] {
            jar.add_cookie_str(&format!("{name}={value}; Domain=.ssu.ac.kr; Path=/"), &base);
        }
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .cookie_provider(jar)
            .build()?;
        let response = http
            .get(format!("{BASE_URL}{LOGIN_PROC_PATH}"))
            .query(&[
                ("rtnUrl", ""),
                ("sToken", s_token.as_str()),
                ("sIdno", self.student_id.as_str()),
            ])
            .header(reqwest::header::REFERER, "https://smartid.ssu.ac.kr/")
            .send()
            .await?;
        if is_login_page(response.url()) {
            return Err(PathError::Login(
                "loginProc.do 가 로그인 화면으로 돌려보냈습니다".into(),
            ));
        }
        Ok(http)
    }

    /// 로그인된 상태로 페이지를 가져옵니다. 세션이 끝나 로그인 화면으로 가면 한 번 다시 로그인합니다.
    async fn page(&self, path: &str, query: &[(&str, String)]) -> Result<String, PathError> {
        for attempt in 0..2 {
            let http = {
                let mut session = self.session.lock().await;
                if session.is_none() || attempt == 1 {
                    *session = Some(self.open_session().await?);
                }
                session.clone().expect("set above")
            };
            let response = http
                .get(format!("{BASE_URL}{path}"))
                .query(query)
                .send()
                .await?
                .error_for_status()?;
            if is_login_page(response.url()) {
                continue;
            }
            return Ok(response.text().await?);
        }
        Err(PathError::Login(
            "다시 로그인해도 로그인 화면으로 돌아갑니다".into(),
        ))
    }

    /// 첫 화면의 나의 기본정보 (학적 상태, 학과, 학년)
    pub async fn profile(&self) -> Result<Profile, PathError> {
        Ok(parse_profile(&self.page("/index.do", &[]).await?))
    }

    /// 비교과 프로그램 목록
    pub async fn programs(&self, query: &ProgramQuery) -> Result<ProgramPage, PathError> {
        let page = query.page.max(1);
        let mut params: Vec<(&str, String)> = vec![
            ("paginationInfo.currentPageNo", page.to_string()),
            ("sort", "0001".into()),
        ];
        if query.recruiting_only {
            params.push(("recStaCdSh", "RS02".into()));
        }
        if let Some(grade) = query.grade.filter(|g| (1..=4).contains(g)) {
            params.push(("schgrCdSh", format!("000{grade}")));
        }
        if let Some(keyword) = query.keyword.as_deref().filter(|k| !k.trim().is_empty()) {
            params.push(("searchValue", keyword.trim().to_string()));
        }
        let html = self.page(LIST_PATH, &params).await?;
        Ok(parse_program_list(&html, page))
    }

    /// 프로그램 상세 (신청 조건, 정원, 주요 내용 등)를 (항목, 값) 목록으로 돌려줍니다.
    pub async fn program_detail(&self, id: &str) -> Result<Vec<(String, String)>, PathError> {
        let html = self
            .page(DETAIL_PATH, &[("encSddpbSeq", id.to_string())])
            .await?;
        Ok(parse_detail(&html))
    }
}

fn is_login_page(url: &Url) -> bool {
    url.path().starts_with("/comm/login/user/login.do")
        || url.host_str() == Some("smartid.ssu.ac.kr")
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

fn text(el: ElementRef<'_>) -> String {
    el.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_text(el: ElementRef<'_>, selector: &str) -> Option<String> {
    el.select(&sel(selector))
        .next()
        .map(text)
        .filter(|t| !t.is_empty())
}

/// `<dl><dt>키</dt><dd>값</dd></dl>` 를 (키, 값) 으로 읽습니다.
fn dl_pairs(el: ElementRef<'_>) -> Vec<(String, String)> {
    el.select(&sel("dl"))
        .filter_map(|dl| {
            Some((
                first_text(dl, "dt")?,
                first_text(dl, "dd").unwrap_or_default(),
            ))
        })
        .collect()
}

fn number(s: &str) -> Option<u32> {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub fn parse_program_list(html: &str, page: u32) -> ProgramPage {
    let doc = Html::parse_document(html);
    let items = doc
        .select(&sel("li"))
        .filter(|li| li.select(&sel(":scope > .cont_box")).next().is_some())
        .filter_map(|li| {
            let id = li
                .select(&sel(".detailBtn"))
                .find_map(|a| a.value().attr("data-params").and_then(enc_seq))?;
            let types: Vec<String> = li.select(&sel("ul.major_type li")).map(text).collect();
            let info = li
                .select(&sel(".info_wrap"))
                .next()
                .map(dl_pairs)
                .unwrap_or_default();
            let etc = li
                .select(&sel(".etc_cont"))
                .next()
                .map(dl_pairs)
                .unwrap_or_default();
            let get = |pairs: &[(String, String)], key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty())
            };
            let applicants = get(&etc, "신청자").as_deref().and_then(number);
            let capacity = get(&etc, "모집정원").as_deref().and_then(number);
            Some(Program {
                id,
                status: first_text(li, ".label_box span"),
                organization: types.first().cloned(),
                form: types.get(1).cloned(),
                title: first_text(li, "a.tit")?,
                summary: first_text(li, "p.desc"),
                apply_period: get(&info, "신청기간"),
                program_period: get(&info, "교육기간"),
                target: get(&info, "신청대상"),
                target_identity: get(&info, "신청신분"),
                mileage: get(&etc, "마일리지").as_deref().and_then(number),
                applicants,
                waiting: get(&etc, "대기자").as_deref().and_then(number),
                capacity,
                // 화면의 "모집이 마감된 프로그램입니다."(p.cnt_desc)는 모든 항목에 들어 있어 쓰지 않습니다.
                seats_left: capacity
                    .zip(applicants)
                    .map(|(c, a)| i64::from(c) - i64::from(a)),
                abilities: li.select(&sel(".cabil dd span")).map(text).collect(),
            })
        })
        .collect();
    let last_page = doc
        .select(&sel("a[onclick]"))
        .filter_map(|a| {
            let onclick = a.value().attr("onclick")?;
            let n = onclick
                .strip_prefix("return global.page(")?
                .split(')')
                .next()?;
            n.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(page)
        .max(page);
    ProgramPage {
        page,
        last_page,
        items,
    }
}

/// `data-params` JSON 문자열에서 `encSddpbSeq` 값을 꺼냅니다.
fn enc_seq(params: &str) -> Option<String> {
    let rest = &params[params.find("\"encSddpbSeq\"")? + "\"encSddpbSeq\"".len()..];
    let rest = &rest[rest.find('"')? + 1..];
    let value = &rest[..rest.find('"')?];
    (!value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| value.to_string())
}

pub fn parse_detail(html: &str) -> Vec<(String, String)> {
    let doc = Html::parse_document(html);
    let mut out: Vec<(String, String)> = Vec::new();
    for head in doc.select(&sel("th, dt")) {
        let key = text(head);
        if key.is_empty() || key.chars().count() > 20 {
            continue;
        }
        let Some(value) = head.next_siblings().filter_map(ElementRef::wrap).next() else {
            continue;
        };
        let value = text(value);
        if !out.iter().any(|(k, v)| k == &key && v == &value) {
            out.push((key, value));
        }
    }
    out
}

/// "글로벌미디어학부/3 학년", "[재학]" 형태의 첫 화면 문구를 읽습니다.
pub fn parse_profile(html: &str) -> Profile {
    let doc = Html::parse_document(html);
    let body = doc.root_element().text().collect::<Vec<_>>().join("\n");
    let mut profile = Profile {
        status: None,
        department: None,
        grade: None,
    };
    for line in body.lines().map(str::trim) {
        if profile.status.is_none()
            && let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']'))
            && matches!(
                inner,
                "재학" | "휴학" | "졸업" | "수료" | "제적" | "졸업유예"
            )
        {
            profile.status = Some(inner.to_string());
        }
        if profile.department.is_none()
            && let Some((dept, rest)) = line.rsplit_once('/')
            && rest.trim().ends_with("학년")
        {
            profile.department = Some(dept.trim().to_string());
            profile.grade = rest.trim().trim_end_matches("학년").trim().parse().ok();
        }
    }
    profile
}

/// 신청 가능 여부 판단
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    Yes,
    No,
    /// 단과대 단위 대상 등 화면 정보만으로 판단할 수 없음
    Unknown,
}

/// 목록 정보와 내 학과로 신청 대상인지 판단합니다. 학년은 목록 검색 조건으로 이미 걸러집니다.
pub fn eligibility(program: &Program, profile: &Profile) -> Eligibility {
    if program
        .target_identity
        .as_deref()
        .is_some_and(|id| !id.contains("학생"))
    {
        return Eligibility::No;
    }
    match program.target.as_deref() {
        None => Eligibility::Unknown,
        Some(t) if t.contains("숭실대학교") => Eligibility::Yes,
        Some(t) => match profile.department.as_deref() {
            Some(dept) if t.contains(dept) => Eligibility::Yes,
            _ => Eligibility::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = r##"<ul><li class=""><div class="cont_box"><div class="img_wrap"><a href="#" class="detailBtn" data-params="{&quot;encSddpbSeq&quot;:&quot;3b6a598fca41694e9cc1a53d84486866&quot;,&quot;paginationInfo.currentPageNo&quot;:&quot;1&quot;}"><img></a></div><div class="desc_wrap"><div class="text_wrap"><div class="label_box"><a href="#" class="btn01 col08 detailBtn"><span>모집중</span></a></div><ul class="major_type"><li>진로취업팀</li><li>비교과 (비학점)</li></ul><a href="#" class="tit ellipsis detailBtn">서부고용복지+센터와 함께하는 아모레퍼시픽 기업탐방</a><p class="desc ellipsis">기업탐방 프로그램</p></div><div class="info_wrap"><dl><dt>신청기간</dt><dd>2026.10.30 17:00~2026.11.04 23:59</dd></dl><dl><dt>교육기간</dt><dd>2026.11.17 10:00~2026.11.17 16:00</dd></dl><dl><dt>신청대상</dt><dd>
            숭실대학교&nbsp;
        </dd></dl><dl><dt>신청신분</dt><dd><div class="cdDiv">학생, 일반인</div></dd></dl></div></div></div><div class="etc_cont"><ul class="rq_desc"><li class="info"><dl><dt>마일리지</dt><dd>40</dd></dl></li><li class="cnt"><dl><dt>신청자</dt><dd>2</dd></dl><dl><dt>대기자</dt><dd>0</dd></dl><dl><dt>모집정원</dt><dd>5</dd></dl><p class="cnt_desc">모집이 마감된 프로그램입니다.</p></li><li class="cabil"><dl><dt>#역량</dt><dd><span class="on">리더십</span><span class="on">창의</span></dd></dl></li></ul></div></li></ul>
        <a href="#" onclick="return global.page(1);">1</a><a href="#" onclick="return global.page(4);">4</a>"##;

    #[test]
    fn parses_list_item() {
        let page = parse_program_list(ITEM, 1);
        assert_eq!(page.last_page, 4);
        let p = &page.items[0];
        assert_eq!(p.id, "3b6a598fca41694e9cc1a53d84486866");
        assert_eq!(p.status.as_deref(), Some("모집중"));
        assert_eq!(p.organization.as_deref(), Some("진로취업팀"));
        assert_eq!(p.target.as_deref(), Some("숭실대학교"));
        assert_eq!(p.target_identity.as_deref(), Some("학생, 일반인"));
        assert_eq!(
            (p.mileage, p.applicants, p.waiting, p.capacity),
            (Some(40), Some(2), Some(0), Some(5))
        );
        assert_eq!(p.abilities, ["리더십", "창의"]);
        assert_eq!(p.seats_left, Some(3));
    }

    #[test]
    fn profile_and_eligibility() {
        let html = "<div><span>[재학]</span><span>글로벌미디어학부/3 학년</span></div>";
        let me = parse_profile(html);
        assert_eq!(
            me,
            Profile {
                status: Some("재학".into()),
                department: Some("글로벌미디어학부".into()),
                grade: Some(3)
            }
        );
        let mut p = parse_program_list(ITEM, 1).items.remove(0);
        assert_eq!(eligibility(&p, &me), Eligibility::Yes);
        p.target = Some("화학공학과".into());
        assert_eq!(eligibility(&p, &me), Eligibility::Unknown);
        p.target = Some("글로벌미디어학부, 경영학부".into());
        assert_eq!(eligibility(&p, &me), Eligibility::Yes);
        p.target_identity = Some("일반인".into());
        assert_eq!(eligibility(&p, &me), Eligibility::No);
    }

    #[test]
    fn detail_pairs() {
        let html = "<table><tr><th>신청대상</th><td>숭실대학교</td></tr><tr><th>정원/대기자</th><td>정원 5명 / 대기자 10명</td></tr></table><dl><dt>학년:</dt><dd>전체</dd></dl>";
        let pairs = parse_detail(html);
        assert!(pairs.contains(&("신청대상".into(), "숭실대학교".into())));
        assert!(pairs.contains(&("학년:".into(), "전체".into())));
    }
}
