//! 숭실대학교 SSU-PATH(path.ssu.ac.kr) 비교과 프로그램 클라이언트입니다.
//!
//! 로그인 흐름 (2026-11 실측): 통합 로그인 버튼은 SmartID 로
//! `apiReturnUrl=https://path.ssu.ac.kr/comm/login/user/loginProc.do` 를 넘깁니다.
//! SmartID 가 돌려주는 `sToken`/`sIdno` 를 그 주소에 그대로 넘기면 PATH 세션이 열립니다.
//! 페이지는 서버에서 그린 HTML 이라 화면 구조를 파싱합니다.

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;
use rusaint_sso::{SsoCredentials, SsoError};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use tokio::sync::Mutex;
use url::Url;

const BASE_URL: &str = "https://path.ssu.ac.kr";
const LOGIN_PROC_PATH: &str = "/comm/login/user/loginProc.do";
const LIST_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtPgmList.do";
const DETAIL_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtPgmInfo.do";
const MILEAGE_PATH: &str = "/ptfol/imng/milgMngt/findMyKumMilgList.do";
const APPLICATIONS_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtAplDscList.do";
const APPLY_FORM_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/findNsbjtPtcptAplForm.do";
const CERT_NUMBER_PATH: &str = "/common/certNum/saveCertNumBas.do";
const CERT_PRINT_PATH: &str = "/ptfol/imng/icmpNsbjtPgm/print.do";
const MAX_APPLICATION_PAGES: u32 = 20;
/// 연결을 재사용하는 시간. 미국 리전에서 학교까지 왕복이 길어, 연결이 식으면
/// TCP·TLS 를 다시 맺는 데만 왕복 3번이 듭니다. 기본값(90초)보다 길게 둡니다.
const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
/// 중간 장비가 조용한 연결을 끊지 않도록 보냅니다.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const USER_AGENT: &str = concat!(
    "rusaint-path/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// 요청 오류. 주소는 떼어 낸 뒤 담습니다(아래 `From` 구현).
    #[error("요청 실패: {0}")]
    Request(reqwest::Error),
    /// SmartID 가 계정 정보를 거부했거나 비어 있음. 계정 잠금을 막기 위해 다시 시도하지 않습니다.
    #[error("통합 로그인 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    #[error("SSU-PATH 로그인 실패: {0}")]
    Login(String),
    #[error("이수증을 열지 못했습니다: {0}")]
    Certificate(String),
}

/// 로그인 주소에는 SmartID `sToken` 이 들어갑니다. 변환을 한 곳에 두고 주소를 떼어 내므로
/// `?` 로 올라오는 오류도 주소를 담지 않습니다.
impl From<reqwest::Error> for PathError {
    fn from(error: reqwest::Error) -> Self {
        Self::Request(error.without_url())
    }
}

/// 비교과 프로그램 목록의 한 항목
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Program {
    /// 상세 조회에 쓰는 id (`encSddpbSeq`)
    pub id: String,
    /// 브라우저에서 여는 상세 페이지 주소. 로그인이 안 되어 있으면 통합 로그인을 거쳐 열립니다.
    pub url: String,
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

/// 목록의 접수상태 필터 (`recStaCdSh`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecruitStatus {
    /// 모집대기 (RS01)
    Upcoming,
    /// 모집중 (RS02)
    Recruiting,
    /// 종료 (RS03). 마감된 프로그램도 목록에는 신청자·대기자 수가 남아 있습니다.
    Closed,
}

impl RecruitStatus {
    fn code(self) -> &'static str {
        match self {
            Self::Upcoming => "RS01",
            Self::Recruiting => "RS02",
            Self::Closed => "RS03",
        }
    }
}

/// 목록 검색 조건
#[derive(Debug, Clone, Default)]
pub struct ProgramQuery {
    /// 접수상태. None 이면 전체
    pub status: Option<RecruitStatus>,
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
    sso: Arc<SsoCredentials>,
    session: Mutex<Option<reqwest::Client>>,
}

impl PathClient {
    /// `sso` 는 다른 서비스와 공유해도 됩니다. 로그인 시도와 거부 상태가 함께 관리됩니다.
    pub fn new(sso: Arc<SsoCredentials>) -> Self {
        Self {
            sso,
            session: Mutex::new(None),
        }
    }

    async fn open_session(&self) -> Result<reqwest::Client, PathError> {
        let s_token = self.sso.token().await.map_err(|e| match e {
            SsoError::CredentialUnusable(reason) => PathError::CredentialUnusable(reason),
            SsoError::Failed(reason) => PathError::Login(reason),
        })?;
        let student_id = self.sso.student_id();

        let base = Url::parse(BASE_URL).expect("static url");
        let jar = Arc::new(Jar::default());
        for (name, value) in [("sToken", s_token.as_str()), ("sIdno", student_id)] {
            jar.add_cookie_str(&format!("{name}={value}; Domain=.ssu.ac.kr; Path=/"), &base);
        }
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE)
            .tcp_keepalive(TCP_KEEPALIVE)
            .cookie_provider(jar)
            .build()?;
        let response = http
            .get(format!("{BASE_URL}{LOGIN_PROC_PATH}"))
            .query(&[
                ("rtnUrl", ""),
                ("sToken", s_token.as_str()),
                ("sIdno", student_id),
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

    /// 다시 로그인하지 않고 지금 세션이 살아 있는지 봅니다. 세션이 없으면 `None`.
    /// 세션 수명을 재고 세션을 살려 두는 데(keep-alive) 씁니다. 끊겼으면 세션을 버립니다.
    pub async fn probe(&self) -> Option<bool> {
        let http = self.session.lock().await.clone()?;
        let alive = match http.get(format!("{BASE_URL}/index.do")).send().await {
            Ok(r) => r.status().is_success() && !is_login_page(r.url()),
            Err(_) => return None,
        };
        if !alive {
            *self.session.lock().await = None;
        }
        Some(alive)
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

    /// 비교과 마일리지: 첫 화면의 올해 누적과, 마일리지 조회 화면의 전체 합계·내역
    pub async fn mileage(&self) -> Result<Mileage, PathError> {
        // 서로 다른 화면이라 순서가 필요 없습니다. 왕복이 긴 구간이라 함께 받습니다.
        let (index, list) =
            tokio::try_join!(self.page("/index.do", &[]), self.page(MILEAGE_PATH, &[]))?;
        Ok(parse_mileage(&list, &index))
    }

    /// 비교과 프로그램 목록
    pub async fn programs(&self, query: &ProgramQuery) -> Result<ProgramPage, PathError> {
        let page = query.page.max(1);
        let mut params: Vec<(&str, String)> = vec![
            ("paginationInfo.currentPageNo", page.to_string()),
            ("sort", "0001".into()),
        ];
        // 검색 조건은 서버 세션에 남아서, 빠뜨리면 이전 요청의 값이 다시 쓰입니다. 전체(0000)와 빈 검색어도 명시합니다.
        params.push((
            "recStaCdSh",
            query.status.map_or("0000", RecruitStatus::code).into(),
        ));
        if let Some(grade) = query.grade.filter(|g| (1..=4).contains(g)) {
            params.push(("schgrCdSh", format!("000{grade}")));
        }
        params.push((
            "searchValue",
            query
                .keyword
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string(),
        ));
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

    /// 나의 신청내역 (신청·이수 상태, 출석, 이수증). 모든 페이지를 모읍니다.
    pub async fn applications(&self) -> Result<Vec<Application>, PathError> {
        let mut all = Vec::new();
        let mut page = 1;
        loop {
            // 페이지 번호가 없으면 서버가 세션에 남은 이전 화면 상태로 응답해 빈 목록이 나옵니다.
            let html = self
                .page(
                    APPLICATIONS_PATH,
                    &[("paginationInfo.currentPageNo", page.to_string())],
                )
                .await?;
            let (items, last_page) = parse_applications(&html, page);
            all.extend(items);
            if page >= last_page || page >= MAX_APPLICATION_PAGES {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// 신청내역에서 프로그램 하나만 찾습니다. 찾으면 남은 페이지는 받지 않습니다.
    pub async fn find_application(
        &self,
        program_id: &str,
    ) -> Result<Option<Application>, PathError> {
        let mut page = 1;
        loop {
            let html = self
                .page(
                    APPLICATIONS_PATH,
                    &[("paginationInfo.currentPageNo", page.to_string())],
                )
                .await?;
            let (items, last_page) = parse_applications(&html, page);
            if let Some(found) = items.into_iter().find(|a| a.program_id == program_id) {
                return Ok(Some(found));
            }
            if page >= last_page || page >= MAX_APPLICATION_PAGES {
                return Ok(None);
            }
            page += 1;
        }
    }

    /// 프로그램의 신청서 양식. 프로그램마다 텍스트·파일 항목과 사전설문 여부가 다릅니다.
    /// 이미 신청한 프로그램이면 제출한 텍스트 답이 `answer` 에 들어 있습니다.
    pub async fn application_form(&self, id: &str) -> Result<ApplicationForm, PathError> {
        let html = self
            .page(APPLY_FORM_PATH, &[("encSddpbSeq", id.to_string())])
            .await?;
        Ok(parse_application_form(&html))
    }

    /// 이수증 HTML. 이미 발급된 이수증은 그대로 열고, 처음이면 화면의 "발급" 버튼처럼 증명서 번호를 등록한 뒤 엽니다.
    /// 돌려주는 HTML 은 스크립트를 빼고 `<base>` 를 넣어 로그인 없이 브라우저에서 열 수 있습니다.
    pub async fn certificate_html(
        &self,
        cert: &CertificateRef,
        year: i32,
    ) -> Result<String, PathError> {
        let query = [
            ("yy", year.to_string()),
            ("certSxnCd", "SP1".to_string()),
            ("certSxnSeq", cert.application_seq.clone()),
            ("certPgmSeq", cert.program_seq.clone()),
            ("stuid", self.sso.student_id().to_string()),
        ];
        if !cert.issued {
            self.post_form(CERT_NUMBER_PATH, &query).await?;
        }
        let html = self.page(CERT_PRINT_PATH, &query).await?;
        if !has_certificate_number(&html) {
            return Err(PathError::Certificate(
                "이수증 번호가 비어 있습니다. SSU-PATH 나의 신청내역에서 직접 발급해 주세요."
                    .into(),
            ));
        }
        Ok(standalone_certificate(&html))
    }

    /// 로그인된 상태로 폼을 POST 합니다. 세션이 끝났으면 한 번 다시 로그인합니다.
    async fn post_form(&self, path: &str, form: &[(&str, String)]) -> Result<String, PathError> {
        for attempt in 0..2 {
            let http = {
                let mut session = self.session.lock().await;
                if session.is_none() || attempt == 1 {
                    *session = Some(self.open_session().await?);
                }
                session.clone().expect("set above")
            };
            let response = http
                .post(format!("{BASE_URL}{path}"))
                .form(form)
                .header("X-Requested-With", "XMLHttpRequest")
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
}

/// 나의 신청내역의 한 줄
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Application {
    /// 프로그램 id (`encSddpbSeq`)
    pub program_id: String,
    pub url: String,
    pub title: String,
    /// 신청, 이수, 미이수, 대기 등
    pub status: String,
    /// 모집기간 안이라 취소 버튼이 있는지. 취소하면 다시 신청할 수 없습니다.
    pub cancelable: bool,
    /// 출석 (예: "0/1"), 없으면 None
    pub attendance: Option<String>,
    pub assignment: Option<String>,
    pub survey: Option<String>,
    /// 이수증을 받을 수 있으면 Some
    pub certificate: Option<CertificateRef>,
    /// 2026.11.15
    pub applied_on: Option<String>,
}

/// 이수증 버튼에 담긴 값
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CertificateRef {
    /// `sddpbSeq`
    pub program_seq: String,
    /// `sddpabSeq`
    pub application_seq: String,
    /// 화면 버튼이 "출력"이면 이미 번호가 발급된 것, "발급"이면 처음입니다.
    pub issued: bool,
}

/// 신청서 양식
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ApplicationForm {
    pub items: Vec<FormItem>,
    /// 사전설문이 있으면 Some(참여했는지). 참여하지 않으면 신청할 수 없습니다.
    pub pre_survey_done: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FormItem {
    /// 담당자가 적은 질문·요청 (예: "팀명", "지원서(pdf)")
    pub label: String,
    pub kind: FormItemKind,
    /// 이미 제출한 텍스트 답
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FormItemKind {
    Text,
    File,
    Other(String),
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
    // 선택자 컴파일은 항목 수와 무관하게 한 번만 합니다.
    let (item, box_, detail) = (sel("li"), sel(":scope > .cont_box"), sel(".detailBtn"));
    let (major, info_wrap, etc_cont) =
        (sel("ul.major_type li"), sel(".info_wrap"), sel(".etc_cont"));
    let items = doc
        .select(&item)
        .filter(|li| li.select(&box_).next().is_some())
        .filter_map(|li| {
            let id = li
                .select(&detail)
                .find_map(|a| a.value().attr("data-params").and_then(enc_seq))?;
            let types: Vec<String> = li.select(&major).map(text).collect();
            let info = li
                .select(&info_wrap)
                .next()
                .map(dl_pairs)
                .unwrap_or_default();
            let etc = li
                .select(&etc_cont)
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
                url: program_url(&id),
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
    let last_page = parse_last_page(&doc, page);
    ProgramPage {
        page,
        last_page,
        items,
    }
}

/// 프로그램 상세 페이지 주소
pub fn program_url(id: &str) -> String {
    format!("{BASE_URL}{DETAIL_PATH}?encSddpbSeq={id}")
}

/// `data-params` JSON 문자열에서 `encSddpbSeq` 값을 꺼냅니다.
fn enc_seq(params: &str) -> Option<String> {
    let rest = &params[params.find("\"encSddpbSeq\"")? + "\"encSddpbSeq\"".len()..];
    let rest = &rest[rest.find('"')? + 1..];
    let value = &rest[..rest.find('"')?];
    (!value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| value.to_string())
}

/// `data-params` JSON 문자열에서 `"key":"value"` 의 값을 꺼냅니다.
fn param(params: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let rest = &params[params.find(&quoted)? + quoted.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string()).filter(|v| !v.is_empty())
}

/// 비어 있거나 "-"·"없음" 인 칸은 None
fn cell_value(text: String) -> Option<String> {
    (!matches!(text.as_str(), "" | "-" | "없음")).then_some(text)
}

/// 나의 신청내역 한 페이지를 읽어 (항목, 마지막 페이지) 를 돌려줍니다.
pub fn parse_applications(html: &str, page: u32) -> (Vec<Application>, u32) {
    let doc = Html::parse_document(html);
    let mut items: Vec<Application> = Vec::new();
    let (rows, cells) = (sel("table tbody tr"), sel("td[data-th]"));
    let (detail, cert, cancel) = (sel("a.detailBtn"), sel("a.certBtn"), sel(".pgmCancelBtn"));
    for row in doc.select(&rows) {
        // 칸 이름마다 선택자를 컴파일하는 대신 행을 한 번만 훑어 이름으로 찾습니다.
        let columns: Vec<(&str, ElementRef)> = row
            .select(&cells)
            .filter_map(|td| td.value().attr("data-th").map(|name| (name, td)))
            .collect();
        let cell = |th: &str| {
            columns
                .iter()
                .find(|(name, _)| *name == th)
                .map(|(_, td)| *td)
        };
        let Some(name) = cell("분반명") else {
            continue;
        };
        let Some(link) = name.select(&detail).next() else {
            continue;
        };
        let Some(program_id) = link.value().attr("data-params").and_then(enc_seq) else {
            continue;
        };
        let certificate = cell("이수증")
            .and_then(|td| td.select(&cert).next())
            .and_then(|a| {
                let params = a.value().attr("data-params")?;
                Some(CertificateRef {
                    program_seq: param(params, "sddpbSeq")?,
                    application_seq: param(params, "sddpabSeq")?,
                    issued: text(a) != "발급",
                })
            });
        let application = Application {
            url: program_url(&program_id),
            program_id,
            title: text(link),
            status: cell("상태").map(text).unwrap_or_default(),
            cancelable: cell("수강취소").is_some_and(|td| td.select(&cancel).next().is_some()),
            attendance: cell("출석").map(text).and_then(cell_value),
            assignment: cell("과제").map(text).and_then(cell_value),
            survey: cell("설문참여").map(text).and_then(cell_value),
            certificate,
            applied_on: cell("신청일").map(text).and_then(cell_value),
        };
        // PC·모바일 표가 함께 있으면 같은 줄이 두 번 나올 수 있습니다.
        if !items.contains(&application) {
            items.push(application);
        }
    }
    (items, parse_last_page(&doc, page))
}

/// 신청서 양식(`#nsbjtPtcptAplForm`)의 항목을 읽습니다.
pub fn parse_application_form(html: &str) -> ApplicationForm {
    let doc = Html::parse_document(html);
    // 항목마다 문서를 다시 훑으면 항목 수 × 문서 크기가 됩니다. 한 번만 훑어 이름으로 찾습니다.
    let mut values: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for element in doc.select(&sel("#nsbjtPtcptAplForm [name]")) {
        let Some(name) = element.value().attr("name") else {
            continue;
        };
        if let Some(value) = element.value().attr("value") {
            values
                .entry(name)
                .or_insert_with(|| value.trim().to_string());
        }
    }
    let value = |name: &str| values.get(name).cloned();
    let mut items = Vec::new();
    for i in 0.. {
        let Some(label) = value(&format!("inputTypeADList[{i}].childVal")) else {
            break;
        };
        let kind = match value(&format!("inputTypeADList[{i}].childVal2")).as_deref() {
            Some("TEXT") => FormItemKind::Text,
            Some("FILE") => FormItemKind::File,
            other => FormItemKind::Other(other.unwrap_or_default().to_string()),
        };
        items.push(FormItem {
            label,
            kind,
            answer: value(&format!("inputTypeADList[{i}].childVal6")).filter(|v| !v.is_empty()),
        });
    }
    ApplicationForm {
        items,
        pre_survey_done: value("preSurvCount").map(|v| v != "0" && !v.is_empty()),
    }
}

/// `return global.page(N);` 버튼에서 가장 큰 N
fn parse_last_page(doc: &Html, page: u32) -> u32 {
    doc.select(&sel("a[onclick]"))
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
        .max(page)
}

/// 이수증 화면에 "제 SP1-2026008559 호" 처럼 번호가 있는지
fn has_certificate_number(html: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&sel(".nsbjt_cert p.num"))
        .next()
        .map(text)
        .is_some_and(|t| t.chars().any(|c| c.is_ascii_digit()))
}

/// 스크립트를 지우고 `<base>` 를 넣어, 로그인 없이 열어도 공개 CSS·이미지로 그려지게 합니다.
pub fn standalone_certificate(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<script").map(|i| pos + i) {
        out.push_str(&html[pos..start]);
        pos = match lower[start..].find("</script>") {
            Some(end) => start + end + "</script>".len(),
            None => html.len(),
        };
    }
    out.push_str(&html[pos..]);
    let base = format!(r#"<base href="{BASE_URL}/">"#);
    let lower = out.to_ascii_lowercase();
    let head_end = lower.match_indices("<head").find_map(|(i, _)| {
        let rest = &lower[i + "<head".len()..];
        if !rest.starts_with(['>', ' ', '\t', '\n']) {
            return None;
        }
        Some(i + "<head".len() + rest.find('>')? + 1)
    });
    match head_end {
        Some(i) => out.insert_str(i, &base),
        None => out = format!("<!DOCTYPE html><html><head>{base}</head><body>{out}</body></html>"),
    }
    out
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

/// 비교과 마일리지
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Mileage {
    /// 첫 화면의 "2026학년도 취득 마일리지" 처럼 올해 쌓은 점수. 장학금 순위는 이 연도 누적으로 매깁니다.
    pub this_year: Option<YearMileage>,
    pub remaining: Option<u32>,
    pub total_earned: Option<u32>,
    pub total_used: Option<u32>,
    pub history: Vec<MileageEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct YearMileage {
    pub year: u32,
    pub earned: Option<u32>,
    pub used: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MileageEntry {
    /// 2026.10.13
    pub date: String,
    /// 비교과 프로그램, 그룹비교과 …
    pub kind: String,
    pub detail: String,
    /// 획득 / 사용
    pub direction: String,
    pub points: Option<u32>,
}

/// `list` 는 마일리지 조회 화면, `index` 는 첫 화면입니다.
pub fn parse_mileage(list: &str, index: &str) -> Mileage {
    let doc = Html::parse_document(list);
    let summary: Vec<(String, String)> = doc
        .select(&sel("div.mileage_box"))
        .next()
        .map(dl_pairs)
        .unwrap_or_default();
    let get = |key: &str| {
        summary
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| number(v))
    };
    let history = doc
        .select(&sel("div.table_wrap.pc_view table.t_list tbody tr"))
        .filter_map(|row| {
            let cell = |th: &str| first_text(row, &format!(r#"td[data-th="{th}"]"#));
            Some(MileageEntry {
                date: cell("취득일자")?,
                kind: cell("항목").unwrap_or_default(),
                detail: cell("상세내역").unwrap_or_default(),
                direction: cell("구분").unwrap_or_default(),
                points: cell("마일리지").as_deref().and_then(number),
            })
        })
        .collect();

    let index = Html::parse_document(index);
    let mut this_year: Option<YearMileage> = None;
    for (key, value) in index
        .select(&sel("div.box"))
        .flat_map(dl_pairs)
        .filter(|(k, _)| k.contains("학년도") && k.contains("마일리지"))
    {
        let Some(year) = key.get(..4).and_then(number) else {
            continue;
        };
        let entry = this_year.get_or_insert(YearMileage {
            year,
            earned: None,
            used: None,
        });
        if key.contains("취득") {
            entry.earned = number(&value);
        } else if key.contains("사용") {
            entry.used = number(&value);
        }
    }
    Mileage {
        this_year,
        remaining: get("잔여 마일리지"),
        total_earned: get("총 획득 마일리지"),
        total_used: get("총 사용한 마일리지"),
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 로그인 주소에는 토큰이 들어가므로 오류 메시지에 주소가 남지 않아야 합니다.
    #[tokio::test]
    async fn request_errors_drop_the_url_with_the_token() {
        // 로컬의 닫힌 포트로 보내 연결 오류를 만듭니다. 외부 통신은 하지 않습니다.
        let failed = reqwest::Client::new()
            .get("http://127.0.0.1:1/login?sToken=SECRET-TOKEN&sIdno=20240000")
            .send()
            .await
            .expect_err("연결은 실패해야 합니다");
        assert!(failed.url().is_some(), "원래 오류에는 주소가 있습니다");
        let message = PathError::from(failed).to_string();
        for leak in ["SECRET-TOKEN", "sToken", "20240000", "127.0.0.1"] {
            assert!(
                !message.contains(leak),
                "{leak} 가 메시지에 남았습니다: {message}"
            );
        }
    }

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
        assert_eq!(
            p.url,
            "https://path.ssu.ac.kr/ptfol/imng/icmpNsbjtPgm/findIcmpNsbjtPgmInfo.do?encSddpbSeq=3b6a598fca41694e9cc1a53d84486866"
        );
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

    #[test]
    fn mileage_summary_history_and_this_year() {
        let list = r#"<div class="mileage_box"><ul><li><dl><dt class="lg">잔여 마일리지</dt><dd><span class="col01">700</span><b>점</b></dd></dl></li><li><dl><dt class="lg">총 획득 마일리지</dt><dd><span class="col02">1,000</span><b>점</b></dd></dl></li><li><dl><dt class="lg">총 사용한 마일리지</dt><dd><span class="col03">300</span><b>점</b></dd></dl></li></ul></div>
<div class="table_wrap pc_view"><table class="t_list"><tbody>
<tr><td data-th="번호">2</td><td data-th="취득일자">2026.03.02</td><td data-th="항목">비교과 프로그램</td><td data-th="상세내역" class="td_btn t_left">예시 특강</td><td data-th="구분"><span class="col01">획득</span></td><td data-th="마일리지"><span class="col02">40</span></td></tr>
<tr><td data-th="번호">1</td><td data-th="취득일자">2025.12.30</td><td data-th="항목">장학금</td><td data-th="상세내역" class="td_btn t_left">마일리지 장학금</td><td data-th="구분"><span class="col03">사용</span></td><td data-th="마일리지"><span class="col02">300</span></td></tr>
</tbody></table></div>
<div class="table_wrap mob_view"><table class="t_list"><tbody><tr><td data-th="취득일자">중복</td></tr></tbody></table></div>"#;
        let index = r#"<div class="box"><dl><dt>2026학년도 전체 마일리지</dt><dd><strong>40점</strong></dd></dl><dl><dt>2026학년도 취득 마일리지</dt><dd><strong>40점</strong></dd></dl><dl><dt>2026학년도 사용 마일리지</dt><dd><strong>0점</strong></dd></dl></div>"#;
        let m = parse_mileage(list, index);
        assert_eq!(
            (m.remaining, m.total_earned, m.total_used),
            (Some(700), Some(1000), Some(300))
        );
        assert_eq!(m.history.len(), 2);
        assert_eq!(m.history[0].detail, "예시 특강");
        assert_eq!(m.history[1].direction, "사용");
        assert_eq!(
            m.this_year,
            Some(YearMileage {
                year: 2026,
                earned: Some(40),
                used: Some(0)
            })
        );
    }

    #[test]
    fn parses_applications() {
        let html = r##"<table><tbody>
<tr class="first last"><td class="input_td"><div class="td_box" id="pushCtnt"></div></td></tr>
<tr><td data-th="분반명"><a href="#" class="detailBtn" data-params="{&quot;encSddpbSeq&quot;:&quot;b1f0c0261597962db8b69702af059eef&quot;,&quot;paginationInfo.currentPageNo&quot;:&quot;1&quot;}">멘토단 모집</a></td><td data-th="상태">신청</td><td data-th="수강취소"><a href="#" class="btn01 col03 pgmCancelBtn" data-params="{}">취소</a></td><td data-th="출석"><a href="#" class="attndBtn">0/1</a></td><td data-th="과제"> 없음</td><td data-th="설문참여"> -</td><td data-th="이수증"> - </td><td data-th="신청일"><a href="#">2026.11.15</a></td></tr>
<tr><td data-th="분반명"><a href="#" class="detailBtn" data-params="{&quot;encSddpbSeq&quot;:&quot;535f053a570f18fd6b7c8604f97fdfee&quot;}">해커톤</a></td><td data-th="상태">이수</td><td data-th="수강취소"> - </td><td data-th="출석"> 없음 </td><td data-th="과제"> 없음</td><td data-th="설문참여"> -</td><td data-th="이수증"><a href="#;" class="btn01 col08 certBtn" data-params="{&quot;encSddpbSeq&quot;:&quot;535f053a570f18fd6b7c8604f97fdfee&quot;,&quot;sddpbSeq&quot;:&quot;1001453&quot;,&quot;sddpabSeq&quot;:&quot;1058498&quot;,&quot;acredDd&quot;:&quot;&quot;,&quot;stuid&quot;:&quot;20240000&quot;}">발급</a></td><td data-th="신청일"><a href="#">2026.08.03</a></td></tr>
<tr><td data-th="분반명"><a href="#" class="detailBtn" data-params="{&quot;encSddpbSeq&quot;:&quot;294e4b9a39c54e0b23b2811c9b34f662&quot;}">대회</a></td><td data-th="상태">이수</td><td data-th="이수증"><a href="#;" class="certBtn" data-params="{&quot;sddpbSeq&quot;:&quot;1000699&quot;,&quot;sddpabSeq&quot;:&quot;1016587&quot;}">출력</a></td></tr>
</tbody></table><a onclick="return global.page(1);">1</a>"##;
        let (apps, last) = parse_applications(html, 1);
        assert_eq!(last, 1);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].program_id, "b1f0c0261597962db8b69702af059eef");
        assert_eq!(apps[0].status, "신청");
        assert!(apps[0].cancelable);
        assert_eq!(apps[0].attendance.as_deref(), Some("0/1"));
        assert_eq!(apps[0].assignment, None);
        assert_eq!(apps[0].certificate, None);
        assert_eq!(apps[0].applied_on.as_deref(), Some("2026.11.15"));
        assert!(!apps[1].cancelable);
        assert_eq!(
            apps[1].certificate,
            Some(CertificateRef {
                program_seq: "1001453".into(),
                application_seq: "1058498".into(),
                issued: false
            })
        );
        assert!(apps[2].certificate.as_ref().unwrap().issued);
    }

    #[test]
    fn parses_application_form() {
        let html = r#"<form id="nsbjtPtcptAplForm">
<input type="hidden" name="sddpbSeq" value="1001656">
<input type="hidden" name="inputTypeADList[0].childVal" value="지원서(서명 후 pdf 파일로 제출)">
<input type="hidden" name="inputTypeADList[0].childVal2" value="FILE">
<input type="hidden" name="inputTypeADList[1].childVal" value="면접 희망 시간대">
<input type="hidden" name="inputTypeADList[1].childVal2" value="TEXT">
<input type="text" name="inputTypeADList[1].childVal6" value="19일 15시">
<input type="hidden" name="preSurvCount" value="0">
</form>"#;
        let form = parse_application_form(html);
        assert_eq!(form.items.len(), 2);
        assert_eq!(form.items[0].kind, FormItemKind::File);
        assert_eq!(form.items[0].answer, None);
        assert_eq!(form.items[1].kind, FormItemKind::Text);
        assert_eq!(form.items[1].answer.as_deref(), Some("19일 15시"));
        assert_eq!(form.pre_survey_done, Some(false));
        let empty = parse_application_form(r#"<form id="nsbjtPtcptAplForm"></form>"#);
        assert!(empty.items.is_empty());
        assert_eq!(empty.pre_survey_done, None);
    }

    #[test]
    fn certificate_is_standalone() {
        let html = r#"<!DOCTYPE HTML><html lang="ko"><head><meta charset="UTF-8"><link rel="stylesheet" href="/common/1user/common/css/print.css"><script src="/x.js"></script></head><body><SCRIPT>setTimeout(function() { window.print(); }, 2000);</SCRIPT><div class="print_wrap nsbjt_cert"><p class="num text01">제&nbsp;SP1-2026008559 호</p></div></body></html>"#;
        assert!(has_certificate_number(html));
        assert!(!has_certificate_number(
            r#"<div class="nsbjt_cert"><p class="num">제&nbsp; 호</p></div>"#
        ));
        let out = standalone_certificate(html);
        assert!(!out.to_ascii_lowercase().contains("<script"));
        assert!(out.contains(r#"<head><base href="https://path.ssu.ac.kr/"><meta"#));
        assert!(out.contains("SP1-2026008559"));
    }
}
