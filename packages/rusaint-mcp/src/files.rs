//! LMS 파일을 원격 커넥터에서 다루는 부분입니다.
//!
//! - 다운로드 링크: 서버 디스크에 쓰지 않고, 1회성 티켓 주소로 LMS 응답을 그대로 흘려보냅니다.
//! - 읽기: PDF·PPTX·DOCX 등을 Claude 가 확실히 읽는 형태(Markdown 텍스트·PNG)로 바꿉니다.
//!   텍스트 변환은 markitdown, PDF 페이지 이미지는 poppler-utils(`pdfinfo`·`pdftoppm`)가 PATH 에 있어야 합니다.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{CallToolResult, ContentBlock as Content, ResourceContents};
use rusaint_lms::{Attachment, LmsClient};

/// 다운로드 링크가 유효한 시간
pub const LINK_TTL: Duration = Duration::from_secs(10 * 60);
/// 읽기용으로 메모리에 받는 파일의 최대 크기
pub const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
/// 한 번에 돌려주는 텍스트 길이(문자 수). 넘으면 다음 페이지부터 이어 읽게 합니다.
const MAX_TEXT_CHARS: usize = 40_000;
/// 페이지 범위를 주지 않았을 때 텍스트로 돌려주는 페이지 수
const DEFAULT_TEXT_PAGES: u32 = 30;
/// 한 번에 돌려주는 페이지 이미지 수
const MAX_IMAGE_PAGES: u32 = 5;
/// `format=pdf` 로 원본을 그대로 넘길 수 있는 최대 크기
const MAX_RAW_PDF_BYTES: usize = 10 * 1024 * 1024;
/// 페이지 구분이 없는 문서(DOCX·텍스트)를 나눠 읽는 한 부분 길이
const PART_CHARS: usize = MAX_TEXT_CHARS;
/// 20MB 넘는 슬라이드 PDF 도 변환되도록 넉넉히 둡니다.
const TOOL_TIMEOUT: Duration = Duration::from_secs(180);

/// 내려받을 대상
pub enum FileTarget {
    Material(String),
    Attachment(Attachment),
}

impl FileTarget {
    pub async fn open(&self, lms: &LmsClient) -> Result<reqwest::Response, String> {
        match self {
            FileTarget::Material(id) => lms.open_material(id).await,
            FileTarget::Attachment(a) => lms.open_attachment(a).await,
        }
        .map_err(|e| e.to_string())
    }
}

#[derive(Clone)]
pub struct Link {
    pub lms: Arc<LmsClient>,
    pub target: Arc<FileTarget>,
    pub filename: String,
    until: Instant,
}

/// 다운로드 티켓 저장소. 모든 원격 연결이 하나를 공유합니다.
#[derive(Clone)]
pub struct FileLinks {
    base: Arc<str>,
    links: Arc<Mutex<HashMap<String, Link>>>,
}

impl FileLinks {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: Arc::from(base.into().as_str()),
            links: Arc::default(),
        }
    }

    /// 이 연결의 LMS 세션에 묶인 티켓을 만들고 다운로드 주소를 돌려줍니다.
    pub fn mint(&self, lms: Arc<LmsClient>, target: FileTarget, filename: String) -> String {
        let ticket = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let now = Instant::now();
        let mut links = self.links.lock().unwrap();
        links.retain(|_, l| l.until > now);
        links.insert(
            ticket.clone(),
            Link {
                lms,
                target: Arc::new(target),
                filename,
                until: now + LINK_TTL,
            },
        );
        format!("{}/files/{ticket}", self.base)
    }

    /// 만료 전까지는 여러 번 받을 수 있습니다(앱이 미리보기 후 다시 받는 경우).
    pub fn get(&self, ticket: &str) -> Option<Link> {
        let now = Instant::now();
        let mut links = self.links.lock().unwrap();
        links.retain(|_, l| l.until > now);
        links.get(ticket).cloned()
    }
}

/// 한글 파일명도 깨지지 않는 `Content-Disposition` 값 (RFC 6266 / 5987)
pub fn content_disposition(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' && c != '%' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = url::form_urlencoded::byte_serialize(filename.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// 읽기용으로 파일 전체를 메모리에 받습니다. 크기 제한을 넘으면 중단합니다.
pub async fn fetch(lms: &LmsClient, target: &FileTarget) -> Result<Vec<u8>, String> {
    let mut response = target.open(lms).await?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_READ_BYTES)
    {
        return Err(too_big());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_READ_BYTES {
            return Err(too_big());
        }
    }
    Ok(bytes)
}

fn too_big() -> String {
    format!(
        "파일이 {}MB 보다 커서 읽을 수 없습니다. 다운로드 도구로 받아 주세요.",
        MAX_READ_BYTES / 1024 / 1024
    )
}

/// 영상·음성처럼 읽을 수 없는 자료인지 (내려받기 전에 거릅니다)
pub fn is_media(kind: &str, filename: &str) -> bool {
    let ext = extension(filename);
    kind == "movie"
        || matches!(
            ext.as_str(),
            "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "mp3" | "m4a" | "wav"
        )
}

fn extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    /// markitdown 이 Markdown 으로 바꾸는 문서. 값은 임시 파일에 붙일 확장자입니다.
    Document(String),
    Text,
    Image(&'static str),
    Other,
}

/// markitdown 에 넘기는 확장자
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "docx", "pptx", "xlsx", "xls", "html", "htm", "epub", "ipynb",
];
/// 변환 없이 그대로 읽는 텍스트 확장자
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "csv", "tsv", "json", "xml", "py", "c", "h", "cpp", "hpp", "java", "kt", "rs",
    "js", "ts", "go", "sql", "r", "m", "tex", "s", "asm", "yaml", "yml",
];

pub fn detect(filename: &str, bytes: &[u8]) -> Kind {
    if bytes.starts_with(b"%PDF") {
        return Kind::Pdf;
    }
    if bytes.starts_with(b"\x89PNG") {
        return Kind::Image("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Kind::Image("image/jpeg");
    }
    let ext = extension(filename);
    if DOCUMENT_EXTENSIONS.contains(&ext.as_str()) {
        return Kind::Document(ext);
    }
    if TEXT_EXTENSIONS.contains(&ext.as_str()) && std::str::from_utf8(bytes).is_ok() {
        return Kind::Text;
    }
    Kind::Other
}

/// `"3"`, `"3-7"`, `"3-"` 같은 1부터 세는 범위
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub first: u32,
    pub last: Option<u32>,
}

impl Range {
    pub fn parse(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let bad = || format!("페이지 범위 형식이 아닙니다: {raw} (예: 3, 3-7, 10-)");
        let number = |s: &str| s.trim().parse::<u32>().ok().filter(|&n| n > 0);
        let range = match raw.split_once('-') {
            None => {
                let n = number(raw).ok_or_else(bad)?;
                Range {
                    first: n,
                    last: Some(n),
                }
            }
            Some((a, b)) => Range {
                first: number(a).ok_or_else(bad)?,
                last: if b.trim().is_empty() {
                    None
                } else {
                    Some(number(b).ok_or_else(bad)?)
                },
            },
        };
        if range.last.is_some_and(|l| l < range.first) {
            return Err(bad());
        }
        Ok(Some(range))
    }

    /// 전체 `total` 안으로 자르고, 한 번에 `span` 개까지만 잡습니다.
    fn clamp(range: Option<Self>, total: u32, span: u32) -> Result<(u32, u32), String> {
        let range = range.unwrap_or(Range {
            first: 1,
            last: None,
        });
        if total == 0 || range.first > total {
            return Err(format!("범위를 벗어났습니다 (전체 {total})"));
        }
        let limit = range.first + span - 1;
        let last = range.last.unwrap_or(limit).min(limit).min(total);
        Ok((range.first, last))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadFormat {
    /// 텍스트·Markdown (기본값)
    #[default]
    Text,
    /// PDF 페이지를 PNG 이미지로 (도표·수식·스캔본용, 한 번에 5쪽)
    Images,
    /// PDF 원본을 그대로 (실험용, 10MB 이하)
    Pdf,
}

impl ReadFormat {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("text") | Some("markdown") => Ok(Self::Text),
            Some("images") | Some("image") | Some("png") => Ok(Self::Images),
            Some("pdf") => Ok(Self::Pdf),
            Some(other) => Err(format!(
                "format 은 text·images·pdf 중 하나여야 합니다: {other}"
            )),
        }
    }
}

/// 받은 파일. 변환 결과(페이지별 텍스트)는 처음 필요할 때 만들어 같이 들고 있습니다.
pub struct Doc {
    pub name: String,
    pub bytes: Vec<u8>,
    pages: tokio::sync::OnceCell<(&'static str, Vec<String>)>,
}

impl Doc {
    pub fn new(name: String, bytes: Vec<u8>) -> Self {
        Self {
            name,
            bytes,
            pages: tokio::sync::OnceCell::new(),
        }
    }

    /// (단위 이름, 페이지별 텍스트)
    async fn pages(&self, kind: &Kind) -> Result<&(&'static str, Vec<String>), String> {
        self.pages
            .get_or_try_init(|| async {
                Ok(match kind {
                    Kind::Pdf => {
                        let md = markitdown(&self.bytes, "pdf").await?;
                        ("페이지", split_pdf_pages(&md))
                    }
                    Kind::Document(ext) if ext == "pptx" => {
                        let md = markitdown(&self.bytes, ext).await?;
                        ("슬라이드", split_slides(&md))
                    }
                    Kind::Document(ext) => {
                        let md = markitdown(&self.bytes, ext).await?;
                        ("부분", split_parts(&md))
                    }
                    Kind::Text => ("부분", split_parts(&String::from_utf8_lossy(&self.bytes))),
                    Kind::Image(_) | Kind::Other => unreachable!("텍스트로 바꾸지 않는 형식"),
                })
            })
            .await
    }
}

/// 받은 파일을 Claude 가 읽을 결과로 바꿉니다.
pub async fn render(
    doc: &Doc,
    range: Option<Range>,
    format: ReadFormat,
) -> Result<CallToolResult, String> {
    let kind = detect(&doc.name, &doc.bytes);
    match (&kind, format) {
        (Kind::Image(mime), _) => Ok(CallToolResult::success(vec![
            Content::text(doc.name.clone()),
            Content::image(STANDARD.encode(&doc.bytes), *mime),
        ])),
        (Kind::Other, _) => Err(
            "읽을 수 없는 형식입니다 (PDF·PPTX·DOCX·XLSX·HTML·텍스트·이미지 지원). 다운로드 도구로 받아 주세요."
                .into(),
        ),
        (Kind::Pdf, ReadFormat::Images) => pdf_images(&doc.name, &doc.bytes, range).await,
        (Kind::Pdf, ReadFormat::Pdf) => {
            if doc.bytes.len() > MAX_RAW_PDF_BYTES {
                return Err(format!(
                    "원본 PDF 는 {}MB 이하만 넘길 수 있습니다. format=text 나 images 를 쓰세요.",
                    MAX_RAW_PDF_BYTES / 1024 / 1024
                ));
            }
            Ok(CallToolResult::success(vec![
                Content::text(format!("{} (PDF 원본, {} bytes)", doc.name, doc.bytes.len())),
                Content::resource(
                    ResourceContents::blob(
                        STANDARD.encode(&doc.bytes),
                        format!("lms://file/{}", safe_uri(&doc.name)),
                    )
                    .with_mime_type("application/pdf"),
                ),
            ]))
        }
        (_, ReadFormat::Images | ReadFormat::Pdf) => Err(
            "이미지·원본은 PDF 만 지원합니다. format=\"text\" 로 읽거나 다운로드 링크로 받아 주세요."
                .into(),
        ),
        (_, ReadFormat::Text) => {
            let (unit, pages) = doc.pages(&kind).await?;
            let hint = (kind == Kind::Pdf).then_some(
                "도표·수식·그림은 텍스트에 빠질 수 있습니다. 필요하면 format=\"images\" 로 해당 페이지 이미지를 받아 보세요.",
            );
            let sparse = kind == Kind::Pdf && looks_sparse(pages);
            Ok(paged_text(
                &doc.name,
                unit,
                pages,
                range,
                if sparse {
                    Some("글자가 거의 없습니다(이미지·스캔 위주). format=\"images\" 로 페이지 이미지를 받아 보세요.")
                } else {
                    hint
                },
            ))
        }
    }
}

fn looks_sparse(pages: &[String]) -> bool {
    let chars: usize = pages.iter().map(|p| p.trim().chars().count()).sum();
    chars < 30 * pages.len().max(1)
}

fn safe_uri(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes()).collect()
}

/// 페이지(슬라이드·부분)별 텍스트를 범위와 길이 제한에 맞게 묶습니다.
fn paged_text(
    name: &str,
    unit: &str,
    pages: &[String],
    range: Option<Range>,
    hint: Option<&str>,
) -> CallToolResult {
    let total = pages.len() as u32;
    let (first, last) = match Range::clamp(range, total, DEFAULT_TEXT_PAGES) {
        Ok(r) => r,
        Err(e) => return CallToolResult::error(vec![Content::text(e)]),
    };
    let mut body = String::new();
    let mut chars = 0;
    let mut shown = first - 1;
    for n in first..=last {
        let block = format!("\n--- {unit} {n} ---\n{}\n", pages[(n - 1) as usize].trim());
        let len = block.chars().count();
        if !body.is_empty() && chars + len > MAX_TEXT_CHARS {
            break;
        }
        body.push_str(&block);
        chars += len;
        shown = n;
    }
    let mut header = format!("{name} — {unit} {first}-{shown} / 전체 {total}");
    if shown < total {
        header.push_str(&format!(
            "\n이어서 읽으려면 pages=\"{}-\" 로 다시 호출하세요.",
            shown + 1
        ));
    }
    if let Some(hint) = hint {
        header.push('\n');
        header.push_str(hint);
    }
    CallToolResult::success(vec![Content::text(format!("{header}\n{body}"))])
}

/// markitdown 은 PDF 페이지 사이에 폼피드(\x0c)를 남깁니다.
fn split_pdf_pages(md: &str) -> Vec<String> {
    let mut pages: Vec<String> = md.split('\x0c').map(str::to_owned).collect();
    if pages.len() > 1 && pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    pages
}

/// markitdown 은 슬라이드마다 `<!-- Slide number: N -->` 를 붙입니다.
fn split_slides(md: &str) -> Vec<String> {
    const MARK: &str = "<!-- Slide number:";
    let mut slides: Vec<String> = Vec::new();
    for (i, chunk) in md.split(MARK).enumerate() {
        if i == 0 {
            if !chunk.trim().is_empty() {
                slides.push(chunk.to_owned());
            }
            continue;
        }
        let body = chunk.split_once("-->").map_or(chunk, |(_, rest)| rest);
        slides.push(body.to_owned());
    }
    if slides.is_empty() {
        slides.push(String::new());
    }
    slides
}

fn split_parts(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = 0;
    for line in text.lines() {
        let len = line.chars().count() + 1;
        if chars + len > PART_CHARS && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            chars = 0;
        }
        current.push_str(line);
        current.push('\n');
        chars += len;
    }
    if !current.trim().is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

/// 임시 폴더에 파일을 두고 외부 도구를 부릅니다. 폴더는 끝나면 지웁니다.
struct TempFile {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl TempFile {
    async fn new(bytes: &[u8], ext: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("rusaint-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let path = dir.join(format!("input.{ext}"));
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { dir, path })
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn run(program: &str, package: &str, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(TOOL_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("{program} 가 너무 오래 걸립니다"))?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("서버에 {package} 가 설치되어 있지 않습니다 ({program} 없음)")
            } else {
                e.to_string()
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default();
        return Err(format!("{program} 실패: {}", last.trim()));
    }
    Ok(output.stdout)
}

/// markitdown 실행 파일. `RUSAINT_MARKITDOWN` 으로 바꿀 수 있습니다.
fn markitdown_bin() -> String {
    std::env::var("RUSAINT_MARKITDOWN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "markitdown".into())
}

async fn markitdown(bytes: &[u8], ext: &str) -> Result<String, String> {
    let file = TempFile::new(bytes, ext).await?;
    let out = run(&markitdown_bin(), "markitdown", &[file.path.as_os_str()]).await?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

async fn pdf_page_count(pdf: &TempFile) -> Result<u32, String> {
    let info = run("pdfinfo", "poppler-utils", &[pdf.path.as_os_str()]).await?;
    String::from_utf8_lossy(&info)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| "PDF 페이지 수를 알 수 없습니다".into())
}

async fn pdf_images(
    name: &str,
    bytes: &[u8],
    range: Option<Range>,
) -> Result<CallToolResult, String> {
    let pdf = TempFile::new(bytes, "pdf").await?;
    let total = pdf_page_count(&pdf).await?;
    let (first, last) = Range::clamp(range, total, MAX_IMAGE_PAGES)?;
    let (f, l) = (first.to_string(), last.to_string());
    let prefix = pdf.dir.join("page");
    run(
        "pdftoppm",
        "poppler-utils",
        &[
            "-png".as_ref(),
            "-scale-to".as_ref(),
            "1400".as_ref(),
            "-f".as_ref(),
            f.as_ref(),
            "-l".as_ref(),
            l.as_ref(),
            pdf.path.as_os_str(),
            prefix.as_os_str(),
        ],
    )
    .await?;
    let mut files: Vec<_> = std::fs::read_dir(&pdf.dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    // page-01.png, page-02.png … (자릿수는 전체 페이지 수에 맞춰지므로 정렬하면 순서대로입니다)
    files.sort();
    let mut header = format!("{name} — 페이지 {first}-{last} / 전체 {total}");
    if last < total {
        header.push_str(&format!(
            "\n다음 페이지는 pages=\"{}-{}\" 로 받으세요.",
            last + 1,
            (last + MAX_IMAGE_PAGES).min(total)
        ));
    }
    let mut contents = vec![Content::text(header)];
    for (n, file) in (first..).zip(files) {
        let png = std::fs::read(&file).map_err(|e| e.to_string())?;
        contents.push(Content::text(format!("페이지 {n}")));
        contents.push(Content::image(STANDARD.encode(png), "image/png"));
    }
    Ok(CallToolResult::success(contents))
}

/// 마지막으로 읽은 파일을 잠깐 들고 있어, 다음 페이지를 읽을 때 다시 받거나 변환하지 않게 합니다.
#[derive(Default)]
pub struct DocCache {
    entry: Option<(String, Arc<Doc>, Instant)>,
}

impl DocCache {
    pub fn get(&mut self, key: &str) -> Option<Arc<Doc>> {
        match &self.entry {
            Some((k, doc, until)) if k == key && *until > Instant::now() => Some(doc.clone()),
            _ => None,
        }
    }

    pub fn put(&mut self, key: String, doc: Arc<Doc>) {
        self.entry = Some((key, doc, Instant::now() + LINK_TTL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(Range::parse(None).unwrap(), None);
        assert_eq!(
            Range::parse(Some("3")).unwrap(),
            Some(Range {
                first: 3,
                last: Some(3)
            })
        );
        assert_eq!(
            Range::parse(Some(" 2 - 5 ")).unwrap(),
            Some(Range {
                first: 2,
                last: Some(5)
            })
        );
        assert_eq!(
            Range::parse(Some("10-")).unwrap(),
            Some(Range {
                first: 10,
                last: None
            })
        );
        for bad in ["0", "5-2", "a", "-3", "1-x"] {
            assert!(Range::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn clamps_ranges() {
        assert_eq!(Range::clamp(None, 100, 30).unwrap(), (1, 30));
        assert_eq!(Range::clamp(None, 4, 30).unwrap(), (1, 4));
        let r = Range::parse(Some("8-")).unwrap();
        assert_eq!(Range::clamp(r, 10, 5).unwrap(), (8, 10));
        let r = Range::parse(Some("1-50")).unwrap();
        assert_eq!(Range::clamp(r, 100, 5).unwrap(), (1, 5));
        assert!(Range::clamp(Range::parse(Some("11")).unwrap(), 10, 5).is_err());
    }

    #[test]
    fn encodes_korean_filenames() {
        let header = content_disposition("1주차 강의 \"자료\".pdf");
        assert_eq!(
            header,
            "attachment; filename=\"1__ __ ____.pdf\"; filename*=UTF-8''1%EC%A3%BC%EC%B0%A8%20%EA%B0%95%EC%9D%98%20%22%EC%9E%90%EB%A3%8C%22.pdf"
        );
    }

    #[test]
    fn detects_kinds() {
        assert_eq!(detect("x.bin", b"%PDF-1.7"), Kind::Pdf);
        assert_eq!(
            detect("a.PPTX", b"PK\x03\x04"),
            Kind::Document("pptx".into())
        );
        assert_eq!(detect("a.txt", "안녕".as_bytes()), Kind::Text);
        assert_eq!(detect("a.txt", b"\xff\xfe"), Kind::Other);
        assert_eq!(detect("a.hwp", b"\xd0\xcf\x11\xe0"), Kind::Other);
        assert_eq!(detect("a", b"\x89PNG\r\n"), Kind::Image("image/png"));
    }

    #[test]
    fn splits_markitdown_output() {
        assert_eq!(
            split_pdf_pages("첫\n\n\x0c둘\n\n\n"),
            ["첫\n\n", "둘\n\n\n"]
        );
        assert_eq!(split_pdf_pages("a\x0cb\x0c\n"), ["a", "b"]);
        let md = "<!-- Slide number: 1 -->\n# 개요\n본문\n\n<!-- Slide number: 2 -->\n# 둘\n";
        let slides = split_slides(md);
        assert_eq!(slides.len(), 2);
        assert_eq!(slides[0].trim(), "# 개요\n본문");
        assert_eq!(slides[1].trim(), "# 둘");
    }

    #[test]
    fn pages_text_within_limits() {
        let pages: Vec<String> = (1..=3).map(|n| format!("p{n}")).collect();
        let text = serde_json::to_string(&paged_text("doc", "페이지", &pages, None, None)).unwrap();
        assert!(text.contains("페이지 1-3 / 전체 3"), "{text}");
        assert!(!text.contains("이어서"));

        let big: Vec<String> = (0..3).map(|_| "가".repeat(MAX_TEXT_CHARS / 2)).collect();
        let text = serde_json::to_string(&paged_text("doc", "부분", &big, None, None)).unwrap();
        assert!(text.contains("부분 1-1 / 전체 3"), "{text}");
        assert!(text.contains(r#"pages=\"2-\""#), "{text}");

        let parts = split_parts(&"줄\n".repeat(PART_CHARS));
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.chars().count() <= PART_CHARS));
    }

    #[test]
    fn spots_media() {
        assert!(is_media("movie", "a"));
        assert!(is_media("", "lecture.MP4"));
        assert!(!is_media("pdf", "a.pdf"));
    }

    /// 로컬에 markitdown·poppler 가 있을 때만 실제 변환을 확인합니다.
    #[tokio::test]
    #[ignore = "markitdown·poppler 필요"]
    async fn converts_real_pdf() {
        let path = std::env::var("RUSAINT_TEST_PDF").expect("RUSAINT_TEST_PDF");
        let doc = Doc::new("test.pdf".into(), std::fs::read(path).unwrap());
        let text =
            serde_json::to_string(&render(&doc, None, ReadFormat::Text).await.unwrap()).unwrap();
        assert!(text.contains("--- 페이지 1 ---"), "{text}");
        let images = render(
            &doc,
            Some(Range {
                first: 1,
                last: Some(1),
            }),
            ReadFormat::Images,
        )
        .await
        .unwrap();
        assert_eq!(images.content.len(), 3);
    }
}
