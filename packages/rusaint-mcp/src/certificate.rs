//! 증명서 발급의 복호화·렌더링을 Python 필터에 맡기는 구현입니다.
//!
//! 필터는 네트워크를 쓰지 않는 순수 변환이고, 학교와의 통신 순서는 `rusaint-certi` 가 가집니다.
//! markitdown·poppler 와 같은 방식으로 외부 실행 파일을 부릅니다.
//!
//! 필요한 자산 (`RUSAINT_CERTI_ASSETS`, 기본 `/usr/local/lib/rusaint-mcp/certi`):
//!   - `REPORTX.exe`: 암호 연산을 읽어 오는 원본 실행 파일 (실행하지 않고 해석만 합니다)
//!   - `certi_report.py`: 필터 (`cipher.py`, `render.py` 와 같은 디렉터리)
//!
//! 글꼴은 자산 디렉터리의 `fonts/` 뿐 아니라 시스템 경로에서도 찾습니다.
//! 데비안·우분투에서는 `apt install fonts-nanum fonts-nanum-coding` 으로 끝나고,
//! 다른 위치에 두었다면 `RUSAINT_CERTI_FONTS` 로 지정합니다.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusaint_certi::{Rendered, Report};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::files::{TempFile, run};

/// 자산 디렉터리에 있어야 하는 파일
const REQUIRED_ASSETS: [&str; 4] = ["REPORTX.exe", "certi_report.py", "cipher.py", "render.py"];

/// 인쇄에 필요한 글꼴 네 가지와, 배포판마다 다른 파일 이름.
/// 이름은 `report/render.py` 의 `FACES` 와 같아야 합니다.
/// 둘째 이름부터는 데비안·우분투의 `fonts-nanum`·`fonts-nanum-coding` 패키지 이름입니다.
const FONT_FACES: [(&str, &[&str]); 4] = [
    (
        "NanumMyeongjo-Regular",
        &["NanumMyeongjo-Regular.ttf", "NanumMyeongjo.ttf"],
    ),
    (
        "NanumMyeongjo-Bold",
        &["NanumMyeongjo-Bold.ttf", "NanumMyeongjoBold.ttf"],
    ),
    (
        "NanumGothicCoding-Regular",
        &["NanumGothicCoding-Regular.ttf", "NanumGothicCoding.ttf"],
    ),
    (
        "NanumGothicCoding-Bold",
        &["NanumGothicCoding-Bold.ttf", "NanumGothicCodingBold.ttf"],
    ),
];

/// 글꼴을 찾는 디렉터리. `RUSAINT_CERTI_FONTS` 와 자산 디렉터리를 먼저 보고, 없으면 시스템 글꼴을 씁니다.
fn font_directories(assets: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var("RUSAINT_CERTI_FONTS")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    dirs.push(assets.join("fonts"));
    for system in [
        "/usr/share/fonts/truetype/nanum",
        "/usr/share/fonts/truetype/nanum-coding",
        "/usr/share/fonts/nanum",
        "/opt/homebrew/share/fonts",
        "/Library/Fonts",
    ] {
        dirs.push(PathBuf::from(system));
    }
    dirs
}

/// 글꼴 네 가지의 실제 경로. 하나라도 없으면 어떤 글꼴이 없는지 알려 줍니다.
fn resolve_fonts(assets: &Path) -> Result<Vec<(&'static str, PathBuf)>, String> {
    resolve_fonts_in(&font_directories(assets))
}

/// 주어진 디렉터리들에서 순서대로 글꼴을 찾습니다.
fn resolve_fonts_in(dirs: &[PathBuf]) -> Result<Vec<(&'static str, PathBuf)>, String> {
    FONT_FACES
        .iter()
        .map(|(face, names)| {
            dirs.iter()
                .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
                .find(|path| path.is_file())
                .map(|path| (*face, path))
                .ok_or_else(|| {
                    format!(
                        "증명서 PDF 생성에 필요한 글꼴 {face} 을 찾을 수 없습니다. \
                         서버에 fonts-nanum·fonts-nanum-coding 을 설치하거나 \
                         RUSAINT_CERTI_FONTS 에 글꼴 디렉터리를 지정하세요"
                    )
                })
        })
        .collect()
}

/// 문서 처리가 어디까지 갔는지. 출력 완료 통지는 되돌릴 수 없어 단계를 디스크에 남깁니다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// PDF 까지 만들었고 아직 출력 매수를 쓰지 않았습니다.
    Rendered,
    /// 학교에 출력 완료를 알렸습니다. 다시 알리면 안 됩니다.
    PrintNotified,
    /// 학교가 원본으로 확인해 주었습니다.
    Verified,
}

/// 한 문서의 처리 기록
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub owner: String,
    pub stage: Stage,
    pub document_code: String,
    pub document_number: String,
    pub name: String,
}

/// 문서별 처리 기록과 만들어 둔 PDF 를 보관합니다.
///
/// 작업 ID 의 1회성만으로는 새 ID 로 다시 출력하는 것을 막지 못하므로, 문서 번호로 기록을 남깁니다.
/// 서버가 다시 떠도 남아야 하므로 systemd 의 상태 디렉터리(`STATE_DIRECTORY`)를 씁니다.
pub struct Journal {
    root: PathBuf,
    owner: String,
}

impl Journal {
    pub(crate) fn orders_root(&self) -> Result<PathBuf, String> {
        let path = self.root.join("orders");
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        private(&path)?;
        sync_directory(&self.root).map_err(|e| e.to_string())?;
        Ok(path)
    }
    pub fn open(owner: &str) -> Result<Self, String> {
        let root = ["RUSAINT_CERTI_STATE", "STATE_DIRECTORY"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("rusaint-certi"))
            .join("certificates");
        Self::at(&root, owner)
    }

    fn at(base: &Path, owner: &str) -> Result<Self, String> {
        if owner.trim().is_empty() {
            return Err("증명서 기록의 소유자가 없습니다".into());
        }
        let root = base.join(format!("{:x}", Sha256::digest(owner.as_bytes())));
        std::fs::create_dir_all(&root).map_err(|e| format!("증명서 기록을 둘 수 없습니다: {e}"))?;
        private(&root)?;
        sync_directory(base).map_err(|e| e.to_string())?;
        if let Some(parent) = base.parent() {
            sync_directory(parent).map_err(|e| e.to_string())?;
        }
        sweep(&root, RETENTION);
        Ok(Self {
            root,
            owner: owner.to_owned(),
        })
    }

    fn dir(&self, document_number: &str) -> PathBuf {
        // 경로 탈출과 서로 다른 문서 번호의 정규화 충돌을 함께 막습니다.
        let safe = format!("{:x}", Sha256::digest(document_number.as_bytes()));
        self.root.join(safe)
    }

    /// 이미 처리한 기록. 있으면 다시 출력하지 않고 이 상태에서 이어 갑니다.
    pub fn record(&self, document_number: &str) -> Result<Option<Record>, String> {
        let dir = self.dir(document_number);
        let data =
            match std::fs::read(dir.join("record.json")) {
                Ok(data) => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !dir.exists() => {
                    return Ok(None);
                }
                Err(_) => return Err(
                    "증명서 처리 기록을 읽을 수 없습니다. 재출력하지 말고 서버 기록을 확인하세요"
                        .into(),
                ),
            };
        let record: Record = serde_json::from_slice(&data).map_err(|_| {
            "증명서 처리 기록이 손상됐습니다. 재출력하지 말고 서버 기록을 확인하세요".to_string()
        })?;
        if record.owner != self.owner || record.document_number != document_number {
            return Err("증명서 처리 기록의 소유자 또는 문서 번호가 일치하지 않습니다".into());
        }
        Ok(Some(record))
    }

    pub fn pdf(&self, document_number: &str) -> Option<Vec<u8>> {
        self.record(document_number).ok()??;
        std::fs::read(self.dir(document_number).join("certificate.pdf")).ok()
    }

    /// 출력 완료를 알리기 **전에** PDF 와 기록을 남깁니다.
    /// 이미 기록이 있으면 거부해, 같은 문서를 두 번 출력하지 않습니다.
    pub fn begin(&self, rendered: &Rendered) -> Result<Record, String> {
        let dir = self.dir(&rendered.document_number);
        std::fs::create_dir(&dir)
            .map_err(|_| "이 문서는 이미 처리한 기록이 있습니다".to_string())?;
        private(&dir)?;
        sync_directory(&self.root).map_err(|e| e.to_string())?;
        let record = Record {
            owner: self.owner.clone(),
            stage: Stage::Rendered,
            document_code: rendered.document_code.clone(),
            document_number: rendered.document_number.clone(),
            name: rendered.name.clone(),
        };
        write_private(&dir.join("certificate.pdf"), &rendered.pdf)?;
        self.save(&record)?;
        Ok(record)
    }

    /// 단계만 올려 다시 씁니다.
    pub fn advance(&self, record: &Record, stage: Stage) -> Result<Record, String> {
        let moved = Record {
            stage,
            ..record.clone()
        };
        self.save(&moved)?;
        Ok(moved)
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        if record.owner != self.owner {
            return Err("다른 사용자의 증명서 처리 기록은 저장할 수 없습니다".into());
        }
        let path = self.dir(&record.document_number).join("record.json");
        let data = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        write_private(&path, &data)
    }
}

/// 학교의 보관·출력 기한은 신청일부터 30일입니다. 그 뒤의 기록과 PDF 는 둘 이유가 없습니다.
const RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// 기한이 지난 문서 기록과 주문 파일을 지웁니다. 실패해도 발급을 막지 않습니다.
fn sweep(root: &Path, older_than: std::time::Duration) {
    let stale = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age >= older_than)
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // 주문 디렉터리는 통째로 지우지 않고 파일마다 따져 봅니다. 디렉터리 시각은
        // 파일 내용이 바뀌어도 그대로라서, 최근에 갱신한 주문까지 날릴 수 있습니다.
        if path.is_dir() && path.file_name().is_some_and(|name| name == "orders") {
            if let Ok(orders) = std::fs::read_dir(&path) {
                for order in orders.flatten() {
                    if stale(&order.path()) {
                        let _ = std::fs::remove_file(order.path());
                    }
                }
            }
            continue;
        }
        if !stale(&path) {
            continue;
        }
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// 본인만 읽는 디렉터리
fn private(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let _ = path;
    Ok(())
}

/// 증명서는 개인정보라 본인만 읽는 파일로 씁니다.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("증명서 저장 경로가 올바르지 않습니다")?;
    let temporary = parent.join(format!(".write-{}", uuid::Uuid::new_v4().simple()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e: std::io::Error| e.to_string())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    let _ = path;
    Ok(())
}

/// 학교에 보고하는 출력 장치 이름. 실제로 쓰는 렌더러를 그대로 알립니다.
pub const PRINT_MODEL: &str = "rusaint PDF renderer";

/// 결제 후 자동 발급과 기존 보관함 출력이 같은 저장·복구 경로를 사용합니다.
pub async fn issue_saved(
    client: &rusaint_certi::CertiClient,
    owner: &str,
    document_id: &str,
) -> Result<(Record, Vec<u8>), String> {
    let journal = Journal::open(owner)?;
    let record = match journal.record(document_id)? {
        Some(record) => record,
        None => {
            let report = PythonReport::new().await?;
            let document = client
                .documents()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|d| d.document_id == document_id)
                .ok_or("본인 보관함에 대상 문서가 없습니다")?;
            let rendered = client
                .render_pdf(&document, &report)
                .await
                .map_err(|e| e.to_string())?;
            let record = journal.begin(&rendered)?;
            client
                .complete_print(&rendered, PRINT_MODEL)
                .await
                .map_err(|_| {
                    "출력 완료 결과를 확인하지 못했습니다. 재출력하지 않고 상태를 확인하세요"
                        .to_string()
                })?;
            journal.advance(&record, Stage::PrintNotified)?
        }
    };
    let pdf = journal
        .pdf(document_id)
        .ok_or("보관한 PDF를 읽을 수 없습니다")?;
    let record = if record.stage == Stage::Verified {
        record
    } else {
        if !client
            .verified_original(&record.document_code, document_id)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("PDF를 서버에 보관했지만 학교 원본확인은 아직 완료되지 않았습니다. 다시 결제하지 마세요".into());
        }
        journal.advance(&record, Stage::Verified)?
    };
    Ok((record, pdf))
}

/// 자산 디렉터리. 없으면 증명서 PDF 기능을 쓸 수 없습니다.
fn assets() -> PathBuf {
    std::env::var("RUSAINT_CERTI_ASSETS")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/lib/rusaint-mcp/certi"))
}

/// Python 필터 실행기. `uv` 가 스크립트의 의존성을 알아서 준비합니다.
fn python() -> String {
    std::env::var("RUSAINT_CERTI_PYTHON")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "uv".into())
}

/// 증명서 문서를 PDF 로 만드는 데 필요한 자산을 확인한 구현
#[derive(Debug)]
pub struct PythonReport {
    root: PathBuf,
    /// 글꼴 이름과 실제 파일 경로. 통신 전에 모두 찾아 둡니다.
    fonts: Vec<(&'static str, PathBuf)>,
}

impl PythonReport {
    /// 자산이 모두 있는지 먼저 확인합니다. 발급 도중에 빠진 것을 발견하면
    /// 이미 출력 매수를 쓴 뒤일 수 있으므로, 통신을 시작하기 전에 봅니다.
    pub async fn new() -> Result<Self, String> {
        Self::at(assets()).await
    }

    async fn at(root: PathBuf) -> Result<Self, String> {
        for name in REQUIRED_ASSETS {
            if tokio::fs::metadata(root.join(name)).await.is_err() {
                return Err(format!(
                    "증명서 PDF 생성에 필요한 {name} 이 서버에 없습니다 (RUSAINT_CERTI_ASSETS)"
                ));
            }
        }
        let fonts = resolve_fonts(&root)?;
        Ok(Self { root, fonts })
    }

    fn script(&self) -> PathBuf {
        self.root.join("certi_report.py")
    }

    /// 필터를 부르고 결과 파일을 읽습니다. 입력과 출력은 임시 디렉터리에만 둡니다.
    async fn filter(
        &self,
        input: &[u8],
        extension: &str,
        arguments: impl FnOnce(&Path, &Path) -> Vec<String>,
    ) -> Result<Vec<u8>, String> {
        let file = TempFile::new(input, extension).await?;
        let output = file.dir.join("output.bin");
        let mut args: Vec<String> = vec![
            "run".into(),
            "--quiet".into(),
            "--python".into(),
            "3.11".into(),
            self.script().to_string_lossy().into_owned(),
        ];
        args.extend(arguments(&file.path, &output));
        let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        run(&python(), "uv", &owned).await?;
        tokio::fs::read(&output)
            .await
            .map_err(|_| "필터가 결과 파일을 만들지 않았습니다".to_string())
    }
}

impl Report for PythonReport {
    async fn decrypt_launch(&self, encrypted: &[u8]) -> Result<Vec<u8>, String> {
        let exe = self.root.join("REPORTX.exe");
        self.filter(encrypted, "bin", |input, output| {
            vec![
                "decrypt".into(),
                "--exe".into(),
                exe.to_string_lossy().into_owned(),
                "--mode".into(),
                "launch".into(),
                "--input".into(),
                input.to_string_lossy().into_owned(),
                "--output".into(),
                output.to_string_lossy().into_owned(),
            ]
        })
        .await
    }

    async fn decrypt_document(
        &self,
        encrypted: &[u8],
        document_number: &str,
    ) -> Result<Vec<u8>, String> {
        let exe = self.root.join("REPORTX.exe");
        let number = document_number.to_owned();
        self.filter(encrypted, "bin", |input, output| {
            vec![
                "decrypt".into(),
                "--exe".into(),
                exe.to_string_lossy().into_owned(),
                "--mode".into(),
                "document".into(),
                "--document-number".into(),
                number,
                "--input".into(),
                input.to_string_lossy().into_owned(),
                "--output".into(),
                output.to_string_lossy().into_owned(),
            ]
        })
        .await
    }

    async fn render(&self, document: &[u8], document_code: &str) -> Result<Vec<u8>, String> {
        let fonts = self.fonts.clone();
        let code = document_code.to_owned();
        self.filter(document, "bin", |input, output| {
            let mut args = vec![
                "render".into(),
                "--document".into(),
                input.to_string_lossy().into_owned(),
                "--document-code".into(),
                code,
                "--output".into(),
                output.to_string_lossy().into_owned(),
            ];
            for (face, path) in fonts {
                args.push("--font".into());
                args.push(format!("{face}={}", path.to_string_lossy()));
            }
            args
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_in(dir: &Path) -> Journal {
        Journal::at(dir, "20240000").expect("journal")
    }

    fn rendered(number: &str) -> Rendered {
        Rendered {
            pdf: b"%PDF-1.7 test".to_vec(),
            document_code: "0123456789ABCDEF".into(),
            document_number: number.into(),
            name: "장학금지급증명서".into(),
            session: String::new(),
            receive_type: String::new(),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("certi-{tag}-{}", uuid::Uuid::new_v4().simple()))
    }

    /// 출력 완료 통지 전에 PDF 가 디스크에 있어야 합니다. 통지 뒤 오류가 나도 잃지 않습니다.
    #[test]
    fn pdf_is_kept_before_the_print_notice() {
        let dir = scratch("keep");
        let journal = journal_in(&dir);
        let record = journal.begin(&rendered("2026ABCD")).expect("기록");
        assert_eq!(record.stage, Stage::Rendered);
        assert_eq!(
            journal.pdf("2026ABCD").as_deref(),
            Some(b"%PDF-1.7 test".as_slice())
        );
        let moved = journal
            .advance(&record, Stage::PrintNotified)
            .expect("단계");
        assert_eq!(
            journal
                .record("2026ABCD")
                .expect("읽기")
                .expect("기록")
                .stage,
            Stage::PrintNotified
        );
        assert_eq!(moved.document_code, "0123456789ABCDEF");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 작업 ID 를 새로 받아도 같은 문서를 두 번 출력하지 못해야 합니다.
    #[test]
    fn the_same_document_cannot_be_printed_twice() {
        let dir = scratch("twice");
        let journal = journal_in(&dir);
        journal.begin(&rendered("2026ABCD")).expect("첫 기록");
        let again = journal.begin(&rendered("2026ABCD"));
        assert!(again.is_err(), "두 번째 출력은 거부해야 합니다");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn document_numbers_cannot_escape_the_journal() {
        let dir = scratch("escape");
        let journal = journal_in(&dir);
        let path = journal.dir("../../etc/passwd");
        assert_eq!(path.parent(), Some(journal.root.as_path()));
        assert!(journal.record("../../etc/passwd").expect("읽기").is_none());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn journal_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm");
        let journal = journal_in(&dir);
        journal.begin(&rendered("2026ABCD")).expect("기록");
        for name in ["certificate.pdf", "record.json"] {
            let mode = std::fs::metadata(journal.dir("2026ABCD").join(name))
                .expect("파일")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn users_cannot_read_or_advance_each_others_documents() {
        let dir = scratch("owners");
        let first = Journal::at(&dir, "20240001").expect("first");
        let second = Journal::at(&dir, "20240002").expect("second");
        let record = first.begin(&rendered("same-document")).expect("record");
        assert!(second.record("same-document").expect("lookup").is_none());
        assert!(second.pdf("same-document").is_none());
        assert!(second.advance(&record, Stage::Verified).is_err());
        let own = second
            .begin(&rendered("same-document"))
            .expect("own record");
        assert_eq!(own.owner, "20240002");
        // 경로 분리뿐 아니라 기록 안의 소유자도 검증한다.
        std::fs::copy(
            first.dir("same-document").join("record.json"),
            second.dir("same-document").join("record.json"),
        )
        .expect("copy");
        assert!(second.record("same-document").is_err());
        assert!(second.pdf("same-document").is_none());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn mismatched_or_corrupt_records_never_become_new_print_jobs() {
        let dir = scratch("corrupt");
        let journal = journal_in(&dir);
        let mut record = journal.begin(&rendered("first")).expect("record");
        record.document_number = "different".into();
        let path = journal.dir("first").join("record.json");
        write_private(&path, &serde_json::to_vec(&record).expect("json")).expect("write");
        assert!(journal.record("first").is_err());
        assert!(journal.pdf("first").is_none());
        write_private(&path, b"{broken").expect("write");
        assert!(journal.record("first").is_err());
        assert!(journal.begin(&rendered("first")).is_err());
        std::fs::remove_file(&path).expect("remove record");
        assert!(journal.record("first").is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_record_keeps_the_previous_snapshot_intact() {
        use std::io::Read;
        let dir = scratch("atomic");
        let journal = journal_in(&dir);
        let record = journal.begin(&rendered("document")).expect("record");
        let mut previous =
            std::fs::File::open(journal.dir("document").join("record.json")).expect("open");
        journal.advance(&record, Stage::Verified).expect("advance");
        let mut snapshot = Vec::new();
        previous.read_to_end(&mut snapshot).expect("snapshot");
        assert_eq!(
            serde_json::from_slice::<Record>(&snapshot)
                .expect("old record")
                .stage,
            Stage::Rendered
        );
        assert_eq!(
            journal
                .record("document")
                .expect("lookup")
                .expect("record")
                .stage,
            Stage::Verified
        );
        let entries = std::fs::read_dir(journal.dir("document"))
            .expect("directory")
            .count();
        assert_eq!(entries, 2, "임시 파일을 남기지 않아야 합니다");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 학교에 요청을 보내기 전에 자산 부족을 알려야 합니다. 발급 도중에 멈추면
    /// 출력 매수를 이미 쓴 상태로 PDF 없이 끝날 수 있습니다.
    #[tokio::test]
    async fn missing_assets_are_reported_before_any_school_request() {
        let error = PythonReport::at(PathBuf::from("/nonexistent-certi-assets"))
            .await
            .expect_err("자산이 없어야 합니다");
        assert!(error.contains("REPORTX.exe"), "{error}");
        assert!(error.contains("RUSAINT_CERTI_ASSETS"), "{error}");
    }

    #[tokio::test]
    async fn every_required_asset_is_checked() {
        let dir =
            std::env::temp_dir().join(format!("certi-test-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(dir.join("fonts"))
            .await
            .expect("dir");
        tokio::fs::write(dir.join("REPORTX.exe"), b"stub")
            .await
            .expect("exe");
        let error = PythonReport::at(dir.clone())
            .await
            .expect_err("필터가 없습니다");
        assert!(error.contains("certi_report.py"), "{error}");
        for name in REQUIRED_ASSETS.iter().skip(1) {
            tokio::fs::write(dir.join(name), b"stub")
                .await
                .expect("asset");
        }
        let error = PythonReport::at(dir.clone())
            .await
            .expect_err("글꼴이 없습니다");
        assert!(error.contains("NanumMyeongjo-Regular"), "{error}");
        assert!(error.contains("fonts-nanum"), "{error}");
        for (_, names) in FONT_FACES {
            tokio::fs::write(dir.join("fonts").join(names[0]), b"stub")
                .await
                .expect("font");
        }
        if let Err(error) = PythonReport::at(dir.clone()).await {
            panic!("자산을 모두 만들었는데 거부했습니다: {error}");
        }
        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    /// 보관 기한이 지난 기록은 지우고, 주문 디렉터리는 통째로 날리지 않아야 합니다.
    #[test]
    fn stale_records_are_swept_but_fresh_orders_survive() {
        let dir = scratch("sweep");
        let documents = dir.join("2026ABCD");
        let orders = dir.join("orders");
        std::fs::create_dir_all(&documents).expect("dir");
        std::fs::create_dir_all(&orders).expect("dir");
        std::fs::write(documents.join("certificate.pdf"), b"%PDF-").expect("pdf");
        std::fs::write(orders.join("order-1.json"), b"{}").expect("order");

        // 기한이 아직 안 지난 경우: 아무것도 지우지 않습니다.
        sweep(&dir, std::time::Duration::from_secs(3600));
        assert!(documents.exists() && orders.join("order-1.json").exists());

        // 기한이 지난 경우: 문서 기록은 사라지고 주문 디렉터리 자체는 남습니다.
        sweep(&dir, std::time::Duration::ZERO);
        assert!(!documents.exists(), "기한이 지난 문서 기록은 지워야 합니다");
        assert!(orders.is_dir(), "주문 디렉터리는 통째로 지우면 안 됩니다");
        assert!(!orders.join("order-1.json").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 데비안·우분투 패키지의 파일 이름으로도 찾아야 합니다. 그러면 글꼴을 서버에 올리지 않아도 됩니다.
    #[test]
    fn system_package_font_names_are_accepted() {
        let dir = scratch("fonts");
        let system = dir.join("nanum");
        std::fs::create_dir_all(&system).expect("dir");
        for (_, names) in FONT_FACES {
            // 자산 디렉터리에는 없고, 시스템 이름으로만 둡니다.
            std::fs::write(system.join(names[names.len() - 1]), b"stub").expect("font");
        }
        let resolved = resolve_fonts_in(&[dir.join("fonts"), system.clone()]).expect("시스템 글꼴");
        assert_eq!(resolved.len(), FONT_FACES.len());
        for (face, path) in resolved {
            assert!(path.starts_with(&system), "{face}: {path:?}");
        }
        // 어느 디렉터리에도 없으면 어떤 글꼴이 없는지 말해야 합니다.
        let missing = resolve_fonts_in(&[dir.join("fonts")]).expect_err("없어야 합니다");
        assert!(missing.contains("NanumMyeongjo-Regular"), "{missing}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
