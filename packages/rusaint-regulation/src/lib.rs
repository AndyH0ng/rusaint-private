//! 숭실대학교 규정집(`rule.ssu.ac.kr`). 로그인 없이 볼 수 있는 공개 사이트입니다.
//!
//! - 규정 목록: `lawTree.do` 의 `zNodes` JSON (규정 id, 현재 개정본 id `hseq`)
//! - 본문: `lawFullContent.do` — 장(`div.chapter`), 조(`div.JO`), 부칙(`div.addenda`)
//! - 별표: `lawFullLawTitle.do?showAttach=1` — HWP 원본과, 브라우저에서 열리는 PDF 미리보기

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const BASE: &str = "https://rule.ssu.ac.kr";
const USER_AGENT: &str = concat!(
    "rusaint-regulation/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 규정은 자주 바뀌지 않으므로 한 번 받은 본문을 이만큼 재사용합니다.
const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);

/// 학생에게 필요한 학사 규정. (규정집 트리의 id, 짧은 이름)
pub const ACADEMIC: [(u32, &str); 5] = [
    (89, "학칙"),
    (99, "학칙시행세칙"),
    (95, "학사관리내규"),
    (96, "연계융합전공"),
    (98, "교육과정규정"),
];

#[derive(Debug, Clone, Serialize)]
pub struct Article {
    /// 제20조(소속학과외 과목 이수인정)
    pub title: String,
    pub chapter: Option<String>,
    /// 항·호·목을 한 줄씩
    pub body: Vec<String>,
}

impl Article {
    /// "제20조" 처럼 번호만 비교합니다. "제20조의2" 와 "제20조" 는 다릅니다.
    pub fn number(&self) -> &str {
        self.title.split('(').next().unwrap_or(&self.title).trim()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    #[serde(skip)]
    pub id: String,
    /// [별표6] 단과대학별 소속학과(부) 전공 최소이수학점.hwp
    pub name: String,
    /// 브라우저에서 열리는 PDF 미리보기
    pub preview_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Regulation {
    pub id: u32,
    pub name: String,
    pub url: String,
    pub articles: Vec<Article>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegulationError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("서버가 HTTP {0} 을(를) 반환했습니다")]
    Status(u16),
    #[error("규정 목록을 읽지 못했습니다")]
    Tree,
    #[error("규정 {0} 을(를) 찾지 못했습니다")]
    NotFound(u32),
    #[error("별표 PDF 를 찾지 못했습니다")]
    NoPdf,
    #[error("별표 PDF 를 읽지 못했습니다: {0}")]
    Pdf(String),
}

#[derive(Deserialize)]
struct Node {
    id: u32,
    name: String,
    #[serde(default)]
    hseq: u64,
}

/// 규정 id → (이름, 현재 개정본 id)
pub fn parse_tree(html: &str) -> Option<HashMap<u32, (String, u64)>> {
    let start = html.find("var zNodes = ")? + "var zNodes = ".len();
    let end = start + html[start..].find("];")? + 1;
    let nodes: Vec<Node> = serde_json::from_str(&html[start..end]).ok()?;
    Some(
        nodes
            .into_iter()
            .filter(|n| n.hseq > 0)
            .map(|n| (n.id, (n.name, n.hseq)))
            .collect(),
    )
}

static BLOCKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.chapter, div.JO, div.addenda").unwrap());
static TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.article td:last-child").unwrap());
static PARAGRAPHS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(":scope > div:not(.article)").unwrap());
static ATTACH_ROWS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("tr").unwrap());
static ATTACH_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("td.lawtxt_Bg").unwrap());
static PDF_URL: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r#"DEFAULT_URL = "(/lmxdata/[^"]+\.pdf)""#).unwrap());
static FILE_ID: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"fileDown\('(\d+)'").unwrap());

fn text(el: ElementRef<'_>) -> String {
    el.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 본문의 조문. 부칙(시행일 등)은 넣지 않습니다.
pub fn parse_articles(html: &str) -> Vec<Article> {
    let doc = Html::parse_document(html);
    let mut chapter = None;
    let mut articles = Vec::new();
    for block in doc.select(&BLOCKS) {
        let class = |c| {
            block
                .value()
                .has_class(c, scraper::CaseSensitivity::CaseSensitive)
        };
        if class("addenda") {
            break;
        }
        if class("chapter") {
            chapter = Some(text(block));
            continue;
        }
        let Some(title) = block.select(&TITLE).next().map(text) else {
            continue;
        };
        let body: Vec<String> = block
            .select(&PARAGRAPHS)
            .map(text)
            .filter(|l| !l.is_empty())
            .collect();
        articles.push(Article {
            title,
            chapter: chapter.clone(),
            body,
        });
    }
    articles
}

/// 별표·서식 목록
pub fn parse_attachments(html: &str) -> Vec<Attachment> {
    let doc = Html::parse_document(html);
    doc.select(&ATTACH_ROWS)
        .filter_map(|row| {
            let name = text(row.select(&ATTACH_NAME).next()?);
            let id = FILE_ID.captures(&row.html())?.get(1)?.as_str().to_string();
            Some(Attachment {
                id: id.clone(),
                name,
                preview_url: format!("{BASE}/lmxsrv/print/pdfView.do?idx={id}&type=attach"),
            })
        })
        .collect()
}

pub struct RegulationClient {
    http: reqwest::Client,
    cache: Mutex<HashMap<u32, (Instant, Regulation)>>,
    texts: Mutex<HashMap<String, (Instant, String)>>,
}

impl RegulationClient {
    pub fn new() -> Result<Self, RegulationError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            cache: Mutex::default(),
            texts: Mutex::default(),
        })
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String, RegulationError> {
        let response = self
            .http
            .get(format!("{BASE}{path}"))
            .query(query)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(RegulationError::Status(response.status().as_u16()));
        }
        Ok(response.text().await?)
    }

    /// 규정 하나의 현재 본문과 별표 목록
    pub async fn regulation(&self, id: u32) -> Result<Regulation, RegulationError> {
        let mut cache = self.cache.lock().await;
        if let Some((at, r)) = cache.get(&id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(r.clone());
        }
        let tree = self
            .get(
                "/lmxsrv/law/lawTree.do",
                &[
                    ("LAWGROUP", String::new()),
                    ("MODE", "srvlist".into()),
                    ("TREE_SEARCH_BOOL", "y".into()),
                ],
            )
            .await?;
        let (name, hseq) = parse_tree(&tree)
            .ok_or(RegulationError::Tree)?
            .remove(&id)
            .ok_or(RegulationError::NotFound(id))?;
        let query = [("SEQ", id.to_string()), ("SEQ_HISTORY", hseq.to_string())];
        let content = self.get("/lmxsrv/law/lawFullContent.do", &query).await?;
        let mut title_query = query.to_vec();
        title_query.push(("showAttach", "1".into()));
        let title = self
            .get("/lmxsrv/law/lawFullLawTitle.do", &title_query)
            .await?;
        let regulation = Regulation {
            id,
            name,
            url: format!("{BASE}/lmxsrv/law/lawDetail.do?SEQ={id}&LAWGROUP=1"),
            articles: parse_articles(&content),
            attachments: parse_attachments(&title),
        };
        cache.insert(id, (Instant::now(), regulation.clone()));
        Ok(regulation)
    }

    /// 별표의 PDF 미리보기에서 글자를 뽑습니다. 표는 칸 구분 없이 줄글로 풀리고,
    /// 병합된 칸은 숫자 순서가 섞일 수 있습니다.
    pub async fn attachment_text(
        &self,
        attachment: &Attachment,
    ) -> Result<String, RegulationError> {
        let mut texts = self.texts.lock().await;
        if let Some((at, t)) = texts.get(&attachment.id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(t.clone());
        }
        let viewer = self
            .get(
                "/lmxsrv/print/pdfView.do",
                &[("idx", attachment.id.clone()), ("type", "attach".into())],
            )
            .await?;
        let path = PDF_URL
            .captures(&viewer)
            .and_then(|c| c.get(1))
            .ok_or(RegulationError::NoPdf)?
            .as_str()
            .to_string();
        let response = self.http.get(format!("{BASE}{path}")).send().await?;
        if !response.status().is_success() {
            return Err(RegulationError::Status(response.status().as_u16()));
        }
        let bytes = response.bytes().await?;
        let text = pdf_extract::extract_text_from_mem(&bytes)
            .map_err(|e| RegulationError::Pdf(e.to_string()))?;
        // 줄 끝 공백과 빈 줄 반복을 줄입니다.
        let mut out = String::new();
        for line in text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        {
            if line.is_empty() && out.ends_with("\n\n") {
                continue;
            }
            out.push_str(&line);
            out.push('\n');
        }
        texts.insert(attachment.id.clone(), (Instant::now(), out.clone()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_has_current_revisions() {
        let tree = parse_tree(include_str!("../tests/fixtures/tree.html")).unwrap();
        let (name, hseq) = &tree[&99];
        assert_eq!(name, "3-1-2 학칙시행세칙(학사과정)");
        assert_eq!(*hseq, 2532);
        assert!(tree.contains_key(&89));
    }

    #[test]
    fn articles_have_titles_chapters_and_paragraphs() {
        let articles = parse_articles(include_str!("../tests/fixtures/content_99.html"));
        assert!(articles.len() > 100, "{}", articles.len());
        let a = articles
            .iter()
            .find(|a| a.title == "제20조(소속학과외 과목 이수인정)")
            .unwrap();
        assert_eq!(a.number(), "제20조");
        assert!(a.chapter.as_deref().unwrap().starts_with("제3장"));
        assert!(a.body.iter().any(|l| l.contains("별표6")));
        // 부칙의 시행일 조문은 넣지 않습니다.
        assert!(!articles.iter().any(|a| a.title.contains("시행일")));
    }

    #[test]
    fn attachments_link_to_pdf_previews() {
        let attachments = parse_attachments(include_str!("../tests/fixtures/title_99.html"));
        let a = attachments
            .iter()
            .find(|a| a.name.starts_with("[별표6]"))
            .unwrap();
        assert_eq!(
            a.preview_url,
            "https://rule.ssu.ac.kr/lmxsrv/print/pdfView.do?idx=15240&type=attach"
        );
    }
}
