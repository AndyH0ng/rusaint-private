//! 일반출력(INTERNET) 주문만 지원합니다. FILE/PDF다운로드 상품은 포함하지 않습니다.
//! 승인 시도는 LGD_OID별로 저장하고, 출력은 MINNO별 Journal로 이어갑니다.
use crate::{
    certificate::{self, Journal, write_private},
    files::FileLinks,
};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path as RoutePath, RawForm, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rusaint_certi::{
    CertiClient,
    payment::{OrderRequest, PreparedOrder},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const TTL: Duration = Duration::from_secs(30 * 60);
const LOOKUPS: usize = 8;
#[cfg(test)]
#[path = "payments_tests.rs"]
mod tests;
type Fields = BTreeMap<String, String>;
type Jobs = Arc<Mutex<HashMap<String, Arc<Job>>>>;
type OwnerLocks = Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>;

fn validate_order(order: &PreparedOrder, maximum: u32) -> Result<(), String> {
    if order.fields.get("SERVICE_TYPE").map(String::as_str) != Some("INTERNET") {
        return Err("일반출력(INTERNET) 상품만 지원합니다".into());
    }
    if order.amount > maximum {
        return Err(format!(
            "학교 금액 {}원이 상한 {}원을 넘어서 결제 연결을 중단했습니다. 미결제 주문은 학교 안내에 따라 자동 취소됩니다",
            order.amount, maximum
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Preparing,
    PreparationFailed,
    Ready,
    CheckoutOpened,
    Cancelled,
    ApprovalPending,
    ApprovalUnknown,
    ApprovalResponseRecorded,
    IssuancePending,
    ContinuationRequired,
    PdfPending,
    Complete,
}
impl Stage {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::PreparationFailed | Self::Cancelled | Self::Complete
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    owner: String,
    id: String,
    request: OrderRequest,
    order: Option<PreparedOrder>,
    stage: Stage,
    approval_attempted: bool,
    created: u64,
    continuation: bool,
    document_id: Option<String>,
}

impl Stored {
    fn save(&self, path: &Path) -> Result<(), String> {
        // 연락처는 메모리에만 둡니다. PG가 돌려준 전체 폼도 저장하지 않습니다.
        if let Some(order) = &self.order
            && order.fields.keys().any(|k| contact_field(k))
        {
            return Err("연락처는 주문 기록에 저장할 수 없습니다".into());
        }
        write_private(path, &serde_json::to_vec(self).map_err(|e| e.to_string())?)
    }
    fn load(path: &Path, owner: &str) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|_| "주문 기록을 읽을 수 없습니다".to_string())?;
        let record: Self = serde_json::from_slice(&data)
            .map_err(|_| "주문 기록이 손상됐습니다. 재결제하지 마세요".to_string())?;
        if record.owner != owner {
            return Err("본인 주문이 아닙니다".into());
        }
        Ok(record)
    }
}

struct Job {
    path: PathBuf,
    data: Mutex<Stored>,
    operation: Mutex<()>,
    contact: Mutex<Option<String>>,
    client: Arc<CertiClient>,
    files: FileLinks,
    born: Instant,
}

/// PG 링크만 공유하고, 주문 조회 권한은 MCP의 인증된 학번으로 검사합니다.
#[derive(Clone)]
pub struct Payments {
    base: String,
    jobs: Jobs,
    owners: OwnerLocks,
    files: FileLinks,
}

impl Payments {
    pub async fn prune(&self) {
        self.jobs
            .lock()
            .await
            .retain(|_, job| job.born.elapsed() < TTL);
    }
    pub fn new(base: impl Into<String>, files: FileLinks) -> Self {
        Self {
            base: base.into().trim_end_matches('/').into(),
            jobs: Arc::default(),
            owners: Arc::default(),
            files,
        }
    }
    async fn owner_lock(&self, owner: &str) -> Arc<Mutex<()>> {
        self.owners
            .lock()
            .await
            .entry(owner.into())
            .or_default()
            .clone()
    }
    async fn ticket(&self, job: Arc<Job>) -> String {
        let ticket = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|_, job| job.born.elapsed() < TTL);
        jobs.insert(ticket.clone(), job);
        ticket
    }
    async fn job(&self, ticket: &str) -> Result<Arc<Job>, String> {
        self.jobs
            .lock()
            .await
            .get(ticket)
            .filter(|j| j.born.elapsed() < TTL)
            .cloned()
            .ok_or_else(|| "결제 링크가 만료됐습니다. MCP에서 주문 상태를 확인하세요".into())
    }

    pub async fn prepare(
        &self,
        owner: &str,
        client: Arc<CertiClient>,
        request: OrderRequest,
        maximum: u32,
    ) -> Result<Value, String> {
        if !(1..=10_000).contains(&maximum) {
            return Err("결제 상한은 1~10,000원으로 지정하세요".into());
        }
        certificate::PythonReport::new().await?;
        let lock = self.owner_lock(owner).await;
        let _guard = lock.lock().await;
        let root = Journal::open(owner)?.orders_root()?;
        let active = root.join("active.json");
        if active.exists() {
            let id: String =
                serde_json::from_slice(&std::fs::read(&active).map_err(|e| e.to_string())?)
                    .map_err(|_| "진행 중 주문 기록을 확인해야 합니다".to_string())?;
            let previous = Stored::load(&root.join(format!("{id}.json")), owner)?;
            if !previous.stage.terminal() {
                return Err(format!(
                    "진행 중인 주문 {}가 있습니다. certificate_order_status로 먼저 확인하세요. 재결제하지 마세요",
                    previous.id
                ));
            }
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = root.join(format!("{id}.json"));
        let mut stored = Stored {
            owner: owner.into(),
            id: id.clone(),
            request: request.clone(),
            order: None,
            stage: Stage::Preparing,
            approval_attempted: false,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            continuation: false,
            document_id: None,
        };
        stored.save(&path)?;
        write_private(
            &active,
            &serde_json::to_vec(&id).map_err(|e| e.to_string())?,
        )?;
        let mut order = match client.prepare_order(&request).await {
            Ok(order) => order,
            Err(error) => {
                stored.stage = Stage::PreparationFailed;
                stored.save(&path)?;
                return Err(error.to_string());
            }
        };
        order.fields.retain(|name, _| !contact_field(name));
        if let Err(error) = validate_order(&order, maximum) {
            stored.stage = Stage::Cancelled;
            stored.order = Some(order.clone());
            stored.save(&path)?;
            return Err(error);
        }
        // 학교 주문번호를 사용자 디렉터리 안의 독립 레코드로 예약한다.
        if order.amount > 0 {
            let oid = order
                .fields
                .get("LGD_OID")
                .ok_or("학교 주문번호가 없습니다")?;
            let key = root.join(format!("oid-{:x}.json", Sha256::digest(oid.as_bytes())));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&key).map_err(|_| {
                "이미 처리 기록이 있는 학교 주문입니다. 재승인하지 않습니다".to_string()
            })?;
            file.sync_all().map_err(|e| e.to_string())?;
            write_private(&key, &serde_json::to_vec(&id).map_err(|e| e.to_string())?)?;
        }
        stored.order = Some(order.clone());
        stored.stage = Stage::Ready;
        stored.save(&path)?;
        let job = Arc::new(Job {
            path,
            data: Mutex::new(stored),
            operation: Mutex::new(()),
            contact: Mutex::new(None),
            client,
            files: self.files.clone(),
            born: Instant::now(),
        });
        let ticket = self.ticket(job.clone()).await;
        if order.amount == 0 {
            // 무료 신청도 승인 시도를 먼저 기록한 뒤 한 번만 제출한다.
            {
                let mut s = job.data.lock().await;
                s.approval_attempted = true;
                s.stage = Stage::ApprovalPending;
                s.save(&job.path)?;
            }
            let background = job.clone();
            tokio::spawn(async move {
                background.approve(order.fields, true).await;
            });
        }
        Ok(
            json!({"order_id":id,"product":"INTERNET","certificate":order.name,"amount":order.amount,
            "summary":format!("{} 국문 1통 · {}원 · 제출기관 {} · 용도 {}. 일반 출력 데이터를 PDF로 변환하며 전자서명 PDF 상품은 아닙니다.",order.name,order.amount,request.receiver,request.purpose),
            "payment_url":if order.amount>0 {Some(format!("{}/certificate-pay/{ticket}",self.base))}else{None},
            "status_url":format!("{}/certificate-pay/{ticket}/status",self.base),
            "status":if order.amount>0 {"ready"}else{"issuance_pending"},"expires_in_minutes":TTL.as_secs()/60}),
        )
    }

    pub async fn status(
        &self,
        owner: &str,
        id: &str,
        client: Arc<CertiClient>,
    ) -> Result<Value, String> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err("주문 ID 형식이 올바르지 않습니다".into());
        }
        let root = Journal::open(owner)?.orders_root()?;
        let path = root.join(format!("{id}.json"));
        // 기존 작업이 있으면 같은 잠금을 사용한다.
        let jobs: Vec<_> = self.jobs.lock().await.values().cloned().collect();
        for job in jobs {
            let matches = {
                let s = job.data.lock().await;
                s.owner == owner && s.id == id
            };
            if matches {
                job.reconcile().await;
                return Ok(job.snapshot().await);
            }
        }
        let mut stored = Stored::load(&path, owner)?;
        if stored.stage == Stage::ApprovalPending {
            stored.stage = Stage::ApprovalUnknown;
            stored.save(&path)?;
        }
        let job = Arc::new(Job {
            path,
            data: Mutex::new(stored),
            operation: Mutex::new(()),
            contact: Mutex::new(None),
            client,
            files: self.files.clone(),
            born: Instant::now(),
        });
        let ticket = self.ticket(job.clone()).await;
        job.reconcile().await;
        let mut result = job.snapshot().await;
        result["status_url"] = json!(format!("{}/certificate-pay/{ticket}/status", self.base));
        // 재시작 뒤에는 브라우저 인증 상태를 잃었으므로 승인을 재개하지 않는다.
        Ok(result)
    }
}

fn contact_field(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BUYERTEL" | "USERMOBILE1" | "USERMOBILE2" | "USERMOBILE3" | "LGD_BUYERPHONE"
    )
}
fn phone_valid(phone: &str) -> bool {
    phone.len() == 11 && phone.starts_with("010") && phone.bytes().all(|b| b.is_ascii_digit())
}

impl Job {
    async fn cancel(&self) -> Result<(), String> {
        let mut s = self.data.lock().await;
        if s.approval_attempted {
            return Err(
                "이미 승인 처리를 시도한 주문은 취소하거나 재결제하지 말고 상태를 확인하세요"
                    .into(),
            );
        }
        s.stage = Stage::Cancelled;
        s.save(&self.path)?;
        *self.contact.lock().await = None;
        Ok(())
    }
    async fn snapshot(&self) -> Value {
        let s = self.data.lock().await;
        let message = match s.stage {
            Stage::ApprovalUnknown | Stage::ApprovalPending => {
                "승인 결과를 아직 확인하지 못했습니다. 다시 결제하지 말고 이 주문의 상태를 확인하세요."
            }
            Stage::ContinuationRequired => {
                "승인 응답에 후속 절차가 있어 서버 확인이 필요합니다. 재결제하지 마세요. 필요하면 학교 주문번호로 KICA에 문의하세요."
            }
            Stage::IssuancePending | Stage::ApprovalResponseRecorded => {
                "발급 등록이 처리 중일 수 있습니다. 익일 오전까지 지연될 수 있으니 재결제하지 말고 나중에 certificate_order_status 또는 certificate_documents로 확인하세요."
            }
            Stage::PdfPending => {
                "문서 등록은 확인했지만 PDF 원본확인이 아직 끝나지 않았습니다. 재결제하지 말고 주문 상태를 확인하세요."
            }
            Stage::Complete => "PDF 발급과 학교 원본확인이 완료됐습니다.",
            Stage::Cancelled => {
                "결제가 취소됐거나 상한을 넘어 중단했습니다. 승인 요청을 보내지 않았습니다."
            }
            _ => "결제 화면에서 상품과 금액을 확인한 뒤 진행하세요.",
        };
        let mut result = json!({"order_id":s.id,"status":s.stage,"amount":s.order.as_ref().map(|o|o.amount),"approval_attempted":s.approval_attempted,
            "document_id":s.document_id,"message":message});
        if matches!(
            s.stage,
            Stage::ApprovalUnknown
                | Stage::ContinuationRequired
                | Stage::IssuancePending
                | Stage::PdfPending
        ) {
            result["school_order_id"] =
                json!(s.order.as_ref().and_then(|o| o.fields.get("LGD_OID")));
            result["support_action"] = json!(
                "KICA 문의가 필요하면 위 학교 주문번호와 승인 내역으로 기존 결제 건의 발급 상태를 확인하세요. 문의는 자동 전송하지 않습니다."
            );
        }
        if s.stage == Stage::Complete
            && let Some(id) = &s.document_id
            && let Ok(journal) = Journal::open(&s.owner)
            && let Some(pdf) = journal.pdf(id)
        {
            result["download_url"] = json!(self.files.mint_inline(
                pdf,
                "application/pdf",
                format!("certificate-{}.pdf", s.id)
            ));
        }
        result
    }

    async fn open(&self, phone: String, return_url: String) -> Result<Fields, String> {
        if !phone_valid(&phone) {
            return Err("010으로 시작하는 휴대폰 번호 11자리를 입력하세요".into());
        }
        let mut s = self.data.lock().await;
        if s.stage != Stage::Ready || s.approval_attempted {
            return Err("이미 사용한 결제 링크입니다. 기존 주문 상태를 확인하세요".into());
        }
        let mut fields = s.order.as_ref().ok_or("주문이 없습니다")?.fields.clone();
        fields.insert("BUYERTEL".into(), phone.clone());
        fields.insert("USERMOBILE1".into(), phone[..3].into());
        fields.insert("USERMOBILE2".into(), phone[3..7].into());
        fields.insert("USERMOBILE3".into(), phone[7..].into());
        fields.insert("LGD_RETURNURL".into(), return_url);
        fields.insert("CST_WINDOW_TYPE".into(), "submit".into());
        fields.insert("LGD_CUSTOM_SWITCHINGTYPE".into(), "SUBMIT".into());
        *self.contact.lock().await = Some(phone);
        s.stage = Stage::CheckoutOpened;
        s.save(&self.path)?;
        fields.retain(|k, _| k.starts_with("LGD_") || k.starts_with("CST_") || k == "BUYERTEL");
        Ok(fields)
    }

    /// Some일 때만 승인한다. 기록을 먼저 동기화해 동시 콜백도 한 번만 처리한다.
    async fn accept(&self, pairs: Vec<(String, String)>) -> Result<Option<Fields>, String> {
        let mut result = Fields::new();
        for (k, v) in pairs {
            if result.insert(k, v).is_some() {
                return Err("콜백 항목이 중복됐습니다".into());
            }
        }
        let mut s = self.data.lock().await;
        if s.stage != Stage::CheckoutOpened || s.approval_attempted {
            return Ok(None);
        }
        let order = s.order.as_ref().ok_or("주문이 없습니다")?;
        for key in ["LGD_OID", "LGD_AMOUNT", "LGD_MID"] {
            if result
                .get(key)
                .is_some_and(|value| Some(value) != order.fields.get(key))
            {
                return Err("주문번호·금액·상점 식별자가 원래 주문과 다릅니다".into());
            }
        }
        let code = result.get("LGD_RESPCODE").map(String::as_str).unwrap_or("");
        if code.is_empty() {
            return Err("결제 응답 코드가 없습니다".into());
        }
        if code != "0000" {
            s.stage = Stage::Cancelled;
            s.save(&self.path)?;
            *self.contact.lock().await = None;
            return Ok(None);
        }
        let key = result
            .get("LGD_PAYKEY")
            .filter(|k| !k.is_empty() && k.len() <= 4096)
            .ok_or("결제 인증 키가 없습니다")?;
        let phone = self
            .contact
            .lock()
            .await
            .clone()
            .ok_or("연락처 인증 상태가 만료됐습니다. 재결제하지 말고 주문 상태를 확인하세요")?;
        let mut approval = order.fields.clone();
        approval.insert("LGD_RESPCODE".into(), "0000".into());
        approval.insert("LGD_RESPMSG".into(), String::new());
        approval.insert("LGD_PAYKEY".into(), key.clone());
        approval.insert("BUYERTEL".into(), phone.clone());
        approval.insert("USERMOBILE1".into(), phone[..3].into());
        approval.insert("USERMOBILE2".into(), phone[3..7].into());
        approval.insert("USERMOBILE3".into(), phone[7..].into());
        s.approval_attempted = true;
        s.stage = Stage::ApprovalPending;
        s.save(&self.path)?;
        Ok(Some(approval))
    }

    async fn approve(&self, fields: Fields, free: bool) {
        let _operation = self.operation.lock().await;
        let response = self.client.approve_order(&fields, free).await;
        // 원문을 메모리에서 판정한 뒤 연락처를 제거한 보존본을 디스크에 둔다.
        let phone = self.contact.lock().await.take();
        let mut s = self.data.lock().await;
        match response {
            Ok(response) => {
                s.continuation = response.location.is_some() || needs_continuation(&response.body);
                let redacted = scrub_contact(&response.body, phone.as_deref());
                let metadata = json!({"http_status":response.status,"location":response.location.map(|s|String::from_utf8_lossy(&scrub_contact(s.as_bytes(),phone.as_deref())).into_owned()),"contact_redacted":true,"continuation_detected":s.continuation});
                let recorded_metadata = write_private(
                    &self.path.with_extension("approval-response.json"),
                    &serde_json::to_vec(&metadata).unwrap_or_default(),
                );
                if write_private(
                    &self.path.with_extension("approval-response.html"),
                    &redacted,
                )
                .is_err()
                    || recorded_metadata.is_err()
                    || response.status >= 400
                {
                    s.stage = Stage::ApprovalUnknown;
                } else {
                    s.stage = Stage::ApprovalResponseRecorded;
                }
            }
            Err(_) => s.stage = Stage::ApprovalUnknown,
        }
        let _ = s.save(&self.path);
        drop(s);
        self.reconcile_locked().await;
    }

    async fn reconcile(&self) {
        let _operation = self.operation.lock().await;
        self.reconcile_locked().await;
    }
    async fn reconcile_locked(&self) {
        let initial = self.data.lock().await.clone();
        if !initial.approval_attempted || initial.stage == Stage::Complete {
            return;
        }
        let Some(order) = initial.order else { return };
        let mut selected = initial.document_id;
        if selected.is_none() {
            for attempt in 0..LOOKUPS {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let Ok(Ok(documents)) =
                    tokio::time::timeout(Duration::from_secs(5), self.client.documents()).await
                else {
                    continue;
                };
                let matched: Vec<_> = documents
                    .into_iter()
                    .filter(|d| {
                        d.name == order.name
                            && if order.amount > 0 {
                                order.document_numbers.contains(&d.document_id)
                            } else {
                                !order.before.contains(&d.document_id)
                            }
                    })
                    .collect();
                if matched.len() == 1 {
                    selected = Some(matched[0].document_id.clone());
                    break;
                }
            }
        }
        let mut s = self.data.lock().await;
        let Some(id) = selected else {
            if s.stage != Stage::ApprovalUnknown {
                s.stage = if s.continuation {
                    Stage::ContinuationRequired
                } else {
                    Stage::IssuancePending
                };
            }
            let _ = s.save(&self.path);
            return;
        };
        s.document_id = Some(id.clone());
        s.stage = Stage::PdfPending;
        if s.save(&self.path).is_err() {
            return;
        }
        let owner = s.owner.clone();
        drop(s);
        if certificate::issue_saved(&self.client, &owner, &id)
            .await
            .is_ok()
        {
            let mut s = self.data.lock().await;
            s.stage = Stage::Complete;
            let _ = s.save(&self.path);
        }
    }
}

fn needs_continuation(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    // 확인하지 않은 자동 제출 폼이나 스크립트는 실행하지 않고 응답을 보존한다.
    let lower = text.to_ascii_lowercase();
    lower.contains(".submit(")
        || lower.contains("onload=")
        || lower.contains("location.href")
        || lower.contains("location.replace(")
}

fn scrub_contact(body: &[u8], phone: Option<&str>) -> Vec<u8> {
    let mut text = String::from_utf8_lossy(body).into_owned();
    if let Some(phone) = phone {
        text = text.replace(phone, "[redacted]");
        if phone_valid(phone) {
            text = text.replace(
                &format!("{}-{}-{}", &phone[..3], &phone[3..7], &phone[7..]),
                "[redacted]",
            );
        }
    }
    let inputs = regex::Regex::new(r"(?is)<input\b[^>]*>").expect("static");
    let value =
        regex::Regex::new(r#"(?is)(\bvalue\s*=\s*)(?:"[^"]*"|'[^']*'|[^\s>]+)"#).expect("static");
    text = inputs
        .replace_all(&text, |caps: &regex::Captures| {
            let tag = &caps[0];
            let lower = tag.to_ascii_uppercase();
            if [
                "BUYERTEL",
                "USERMOBILE1",
                "USERMOBILE2",
                "USERMOBILE3",
                "LGD_BUYERPHONE",
            ]
            .iter()
            .any(|key| lower.contains(key))
            {
                value.replace_all(tag, "${1}\"[redacted]\"").into_owned()
            } else {
                tag.into()
            }
        })
        .into_owned();
    let assignments = regex::Regex::new(
        r#"(?i)((?:BUYERTEL|USERMOBILE[123]|LGD_BUYERPHONE)["']?\s*[:=]\s*)["'][^"']*["']"#,
    )
    .expect("static");
    assignments
        .replace_all(&text, "${1}\"[redacted]\"")
        .into_owned()
        .into_bytes()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
/// 로그인 화면과 같은 디자인 시스템(shadcn/ui zinc)을 쓰는 `crate::page` 가 화면을 만듭니다.
/// `pg` 는 결제창으로 폼을 보내는 화면이라 CSP 의 form-action 이 다릅니다.
fn page(body: String, pg: bool) -> Response {
    let csp: &str = if pg {
        &crate::page::PG_CSP
    } else {
        &crate::page::PAY_CSP
    };
    (
        [
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(body),
    )
        .into_response()
}
fn failure(error: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        page(
            crate::page::certificate_status("", &error, None, false),
            false,
        ),
    )
        .into_response()
}

pub fn router<S: Clone + Send + Sync + 'static>(payments: Payments) -> Router<S> {
    Router::new()
        .route("/certificate-pay/{ticket}", get(checkout))
        .route("/certificate-pay/{ticket}/start", post(start))
        .route("/certificate-pay/{ticket}/return", post(returned))
        .route("/certificate-pay/{ticket}/status", get(status))
        .route("/certificate-pay/{ticket}/refresh", post(refresh))
        .route("/certificate-pay/{ticket}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(payments)
}
async fn checkout(State(p): State<Payments>, RoutePath(ticket): RoutePath<String>) -> Response {
    let job = match p.job(&ticket).await {
        Ok(j) => j,
        Err(e) => return failure(e),
    };
    let s = job.data.lock().await;
    let Some(order) = &s.order else {
        return failure("주문이 없습니다".into());
    };
    if s.stage != Stage::Ready {
        drop(s);
        return result_page(&job, &ticket).await;
    }
    page(
        crate::page::certificate_pay(
            &ticket,
            &order.name,
            order.amount,
            &s.request.receiver,
            &s.request.purpose,
        ),
        false,
    )
}
async fn start(
    State(p): State<Payments>,
    RoutePath(ticket): RoutePath<String>,
    RawForm(body): RawForm,
) -> Response {
    let job = match p.job(&ticket).await {
        Ok(j) => j,
        Err(e) => return failure(e),
    };
    let inputs: Fields = url::form_urlencoded::parse(&body).into_owned().collect();
    let fields = match job
        .open(
            inputs.get("phone").cloned().unwrap_or_default(),
            format!("{}/certificate-pay/{ticket}/return", p.base),
        )
        .await
    {
        Ok(f) => f,
        Err(e) => return failure(e),
    };
    let inputs = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "<input type=hidden name='{}' value='{}'>",
                escape(k),
                escape(v)
            )
        })
        .collect::<String>();
    page(crate::page::certificate_pg(&inputs), true)
}
async fn returned(
    State(p): State<Payments>,
    RoutePath(ticket): RoutePath<String>,
    RawForm(body): RawForm,
) -> Response {
    let job = match p.job(&ticket).await {
        Ok(j) => j,
        Err(e) => return failure(e),
    };
    match job
        .accept(url::form_urlencoded::parse(&body).into_owned().collect())
        .await
    {
        Ok(Some(fields)) => {
            let background = job.clone();
            tokio::spawn(async move {
                background.approve(fields, false).await;
            });
        }
        Ok(None) => {}
        Err(e) => return failure(e),
    }
    result_page(&job, &ticket).await
}
async fn result_page(job: &Job, ticket: &str) -> Response {
    let state = job.snapshot().await;
    let download = state.get("download_url").and_then(Value::as_str);
    let cancelable = state["approval_attempted"] == false && state["status"] != "cancelled";
    page(
        crate::page::certificate_status(
            ticket,
            state["message"].as_str().unwrap_or("상태 확인 중"),
            download,
            cancelable,
        ),
        false,
    )
}
async fn status(State(p): State<Payments>, RoutePath(ticket): RoutePath<String>) -> Response {
    match p.job(&ticket).await {
        Ok(job) => result_page(&job, &ticket).await,
        Err(e) => failure(e),
    }
}
async fn refresh(State(p): State<Payments>, RoutePath(ticket): RoutePath<String>) -> Response {
    let job = match p.job(&ticket).await {
        Ok(j) => j,
        Err(e) => return failure(e),
    };
    let background = job.clone();
    tokio::spawn(async move {
        background.reconcile().await;
    });
    result_page(&job, &ticket).await
}

async fn cancel(State(p): State<Payments>, RoutePath(ticket): RoutePath<String>) -> Response {
    let job = match p.job(&ticket).await {
        Ok(job) => job,
        Err(error) => return failure(error),
    };
    if let Err(error) = job.cancel().await {
        return failure(error);
    }
    result_page(&job, &ticket).await
}
