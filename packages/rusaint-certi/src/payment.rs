//! 학교 주문 생성과 승인 통신. PG 인증은 사용자 브라우저에서 수행합니다.
use crate::{BASE_URL, CertiClient, CertiError, common_fields, parse};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const APPROVAL_PATH: &str = "/wm1.0/jsp/pay/lg/smart/newcertres.jsp";
type Fields = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Enrollment,
    Scholarship,
}
impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Enrollment => "재학증명서",
            Self::Scholarship => "장학금지급증명서",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub kind: Kind,
    pub start: Option<String>,
    pub end: Option<String>,
    pub receiver: String,
    pub purpose: String,
}

/// 서버에만 보관합니다. fields에는 학교 주문 서명이 포함됩니다.
#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedOrder {
    pub fields: Fields,
    pub amount: u32,
    pub name: String,
    pub document_numbers: Vec<String>,
    pub before: Vec<String>,
}

pub struct ApprovalResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub location: Option<String>,
}

fn protocol(message: &str) -> CertiError {
    CertiError::Protocol(message.into())
}
fn form(source: &str) -> Result<parse::Form, CertiError> {
    parse::first_form(source).ok_or_else(|| protocol("학교 신청 양식이 없습니다"))
}
fn path(action: &str) -> Result<String, CertiError> {
    let base = url::Url::parse(BASE_URL).expect("static");
    let url = base
        .join(action)
        .map_err(|_| protocol("학교 양식 주소가 올바르지 않습니다"))?;
    if !crate::school_url(&url) {
        return Err(protocol("학교 외부 양식은 제출할 수 없습니다"));
    }
    let path = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    if !matches!(
        path.as_str(),
        "/servlet/WMCERT?COMMAND=CERTWAITING"
            | "/servlet/WMCERT?COMMAND=CERTILIST"
            | "/servlet/WMCERT?COMMAND=POSTBASKET"
    ) {
        return Err(protocol("확인하지 않은 학교 신청 단계입니다"));
    }
    Ok(path)
}

fn document_numbers(fields: &Fields) -> Vec<String> {
    fields
        .get("MINNOS")
        .into_iter()
        .chain(fields.get("MINNO"))
        .flat_map(|s| s.split(['@', ',', '$']))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl CertiClient {
    /// 승인은 리디렉션을 따라가지 않아 첫 응답을 잃지 않습니다. 호출자는 재전송하면 안 됩니다.
    pub async fn approve_order(
        &self,
        fields: &Fields,
        free: bool,
    ) -> Result<ApprovalResponse, CertiError> {
        {
            let mut session = self.session.lock().await;
            if session.is_none() {
                *session = Some(self.open_session().await?);
            }
        }
        let http = self
            .approval_http
            .lock()
            .await
            .clone()
            .ok_or_else(|| protocol("승인용 학교 세션이 없습니다"))?;
        let path = if free {
            "/servlet/WMPAY?COMMAND=NEWPAYMENTFREE"
        } else {
            APPROVAL_PATH
        };
        let mut response = http
            .post(format!("{BASE_URL}{path}"))
            .form(fields)
            .send()
            .await?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > crate::MAX_DOCUMENT {
                return Err(protocol("승인 응답 크기를 넘었습니다"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(ApprovalResponse {
            body,
            status,
            location,
        })
    }
    /// 주문 및 승인 요청은 자동 재전송하지 않습니다. 응답 보존은 호출자가 담당합니다.
    pub async fn order_post(&self, path: &str, fields: &Fields) -> Result<Vec<u8>, CertiError> {
        if !matches!(
            path,
            "/servlet/WMMEMBER?COMMAND=GIWANLIST"
                | "/servlet/WMCERT?COMMAND=CERTWAITING"
                | "/servlet/WMCERT?COMMAND=CERTILIST"
                | "/servlet/WMCERT?COMMAND=POSTBASKET"
                | "/servlet/WMCERT?COMMAND=POSTGAJUBSU"
                | "/servlet/WMBASKET?COMMAND=POSTBASKETLIST"
                | "/servlet/WMPAY?COMMAND=NEWPAYMENT"
                | "/servlet/WMPAY?COMMAND=NEWPAYMENTFREE"
                | APPROVAL_PATH
        ) {
            return Err(protocol("확인하지 않은 주문 경로입니다"));
        }
        let http = {
            let mut session = self.session.lock().await;
            if session.is_none() {
                *session = Some(self.open_session().await?);
            }
            session.clone().expect("session")
        };
        let mut response = http
            .post(format!("{BASE_URL}{path}"))
            .form(fields)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(CertiError::from)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(CertiError::from)? {
            if bytes.len() + chunk.len() > crate::MAX_DOCUMENT {
                return Err(protocol("학교 응답 크기를 넘었습니다"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn order_screen(&self, path: &str, fields: &Fields) -> Result<String, CertiError> {
        let bytes = self.order_post(path, fields).await?;
        String::from_utf8(bytes).map_err(|_| protocol("학교 신청 화면 인코딩이 올바르지 않습니다"))
    }

    /// 학교가 만든 일반 출력 1통 주문. 무료 건의 최종 발급과 유료 승인은 아직 하지 않습니다.
    pub async fn prepare_order(&self, request: &OrderRequest) -> Result<PreparedOrder, CertiError> {
        let _guard = self.ordering.lock().await;
        if request.receiver.trim().is_empty()
            || request.purpose.trim().is_empty()
            || request.receiver.len() > 200
            || request.purpose.len() > 200
        {
            return Err(protocol("제출기관과 용도를 200바이트 이내로 입력하세요"));
        }
        let before = self
            .documents()
            .await?
            .into_iter()
            .map(|d| d.document_id)
            .collect();
        let mut fields: Fields = common_fields()
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect();
        for (key, value) in [
            ("SERVICE_TYPE", "INTERNET"),
            ("LEFTNUM", "1"),
            ("SUBNUM", "1"),
            ("TOP", "SERVICE"),
            ("FILETYPE", ""),
        ] {
            fields.insert(key.into(), value.into());
        }
        let source = self
            .order_screen("/servlet/WMMEMBER?COMMAND=GIWANLIST", &fields)
            .await?;
        let academic = form(&source)?;
        fields = academic.fields;
        if !fields.contains_key("Inner_hakbun0") || fields.contains_key("Inner_hakbun1") {
            return Err(protocol(
                "학적이 없거나 여러 개여서 자동 선택할 수 없습니다",
            ));
        }
        for (to, from) in [
            ("HAKBUN", "hakbun"),
            ("HAKWI", "hakwi"),
            ("HAKWI2", "hakwi2"),
            ("HAKWA", "hakwa"),
            ("HAKWA2", "hakwa2"),
            ("KNAME", "kname"),
            ("ENAME", "ename"),
            ("SANGTAE", "sangtae"),
            ("SANGTAENO", "sangtae_no"),
            ("JOBSUBGPCD", "job_subgp_cd"),
        ] {
            fields.insert(
                to.into(),
                fields
                    .get(&format!("Inner_{from}0"))
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        fields.insert("KORENG".into(), "K".into());
        fields.insert("sel_hakwi".into(), "1".into());
        fields.remove("COMMAND");
        self.order_screen(&path(&academic.action)?, &fields).await?;
        let mut source = String::new();
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            source = self
                .order_screen("/servlet/WMCERT?COMMAND=CERTILIST", &fields)
                .await?;
            if form(&source)?
                .fields
                .values()
                .any(|v| v == request.kind.title())
            {
                break;
            }
        }
        let selection = form(&source)?;
        let index = selection
            .fields
            .iter()
            .find_map(|(k, v)| {
                (k.starts_with("job_ne") && v == request.kind.title())
                    .then(|| k.trim_start_matches("job_ne").to_owned())
            })
            .ok_or_else(|| protocol("이 학적에서 선택할 수 없는 증명서입니다"))?;
        fields = selection.fields;
        if request.kind == Kind::Scholarship {
            for (name, value) in [
                ("Inner_JANG_YEAR", &request.start),
                ("Inner_JANG_YEART", &request.end),
            ] {
                let key = format!("{name}{index}");
                let value = value
                    .as_ref()
                    .ok_or_else(|| protocol("장학금 조회 시작·종료 학기를 지정하세요"))?;
                if !selection
                    .options
                    .get(&key)
                    .is_some_and(|options| options.contains(value))
                {
                    return Err(protocol("장학금 학기 선택이 올바르지 않습니다"));
                }
                fields.insert(key, value.clone());
            }
        }
        // 한 종류 한 통. 다른 체크박스의 기본값을 전송하지 않습니다.
        fields.retain(|k, _| !k.starts_with("issue"));
        fields.insert(format!("minwoncount{index}"), "01".into());
        let names = [
            "job_subgp_cd",
            "job_no",
            "job_ne",
            "ko_en_doc",
            "in_out_user",
            "program_id",
            "SYSTEM_PARAM",
            "minwoncount",
            "Inner_SUNGJUKPYOGI",
            "Inner_KYOUK_YEAR",
            "Inner_JANG_YEAR",
            "Inner_JANG_YEART",
            "Inner_JUMIN_PYOGI",
            "Inner_ARITHMETIC",
        ];
        let sum = names
            .iter()
            .map(|name| {
                fields
                    .get(&format!("{name}{index}"))
                    .map(String::as_str)
                    .unwrap_or(" ")
            })
            .collect::<Vec<_>>()
            .join("∥");
        fields.insert("HTEMPSUM".into(), format!("⌒{sum}∥"));
        fields.insert(format!("issue{index}"), format!("issue{index}"));
        fields.remove("COMMAND");
        let source = self
            .order_screen(&path(&selection.action)?, &fields)
            .await?;
        let basket = form(&source)?;
        if !source.contains(request.kind.title()) || basket.fields.contains_key("MINNO1") {
            return Err(protocol("다른 신청이 섞여 있어 중단했습니다"));
        }
        let mut source = self
            .order_screen("/servlet/WMCERT?COMMAND=POSTGAJUBSU", &basket.fields)
            .await?;
        for _ in 0..8 {
            if source.contains("gointernetfree();") || source.contains("gointernet();") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            fields = form(&source)?.fields;
            fields.remove("COMMAND");
            source = self
                .order_screen("/servlet/WMBASKET?COMMAND=POSTBASKETLIST", &fields)
                .await?;
        }
        fields = form(&source)?.fields;
        fields.remove("COMMAND");
        if source.contains("javascript:gointernetfree();") {
            return Ok(PreparedOrder {
                document_numbers: document_numbers(&fields),
                fields,
                amount: 0,
                name: request.kind.title().into(),
                before,
            });
        }
        if !source.contains("gointernet();") {
            return Err(protocol(
                "학교 접수가 아직 처리 중입니다. 보관함을 확인하세요",
            ));
        }
        source = self
            .order_screen("/servlet/WMPAY?COMMAND=NEWPAYMENT", &fields)
            .await?;
        let mut fields = parse::forms(&source)
            .into_iter()
            .find(|f| f.fields.contains_key("LGD_AMOUNT"))
            .ok_or_else(|| protocol("학교 결제 양식이 없습니다"))?
            .fields;
        fields.insert("RECEIVER".into(), request.receiver.clone());
        fields.insert("PURPOSE".into(), request.purpose.clone());
        let amount = fields
            .get("LGD_AMOUNT")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| protocol("학교 결제 금액이 올바르지 않습니다"))?;
        if ["LGD_HASHDATA", "LGD_MID", "LGD_OID", "LGD_TIMESTAMP"]
            .iter()
            .any(|key| fields.get(*key).is_none_or(String::is_empty))
        {
            return Err(protocol(
                "학교가 생성한 결제 서명 또는 주문 번호가 없습니다",
            ));
        }
        let numbers = document_numbers(&fields);
        if numbers.len() != 1 {
            return Err(protocol("결제 대상 문서 1건을 식별하지 못했습니다"));
        }
        Ok(PreparedOrder {
            fields,
            amount,
            name: request.kind.title().into(),
            document_numbers: numbers,
            before,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_ids_are_whole_values_and_deduplicated() {
        let fields = Fields::from([
            ("MINNOS".into(), "one@two@one@".into()),
            ("MINNO".into(), "two".into()),
        ]);
        assert_eq!(document_numbers(&fields), vec!["one", "two"]);
    }
    #[test]
    fn only_known_school_order_actions_are_allowed() {
        assert!(path("/servlet/WMCERT?COMMAND=POSTBASKET").is_ok());
        for action in [
            "https://evil.example/servlet/WMCERT?COMMAND=POSTBASKET",
            "http://certi.ssu.ac.kr/servlet/WMCERT?COMMAND=POSTBASKET",
            APPROVAL_PATH,
        ] {
            assert!(path(action).is_err());
        }
    }
}
