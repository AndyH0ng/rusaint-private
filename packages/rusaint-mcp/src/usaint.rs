//! u-saint(rusaint) 세션 관리. 통합 계정은 LMS·SSU-PATH 와 공유하는 [`SsoCredentials`] 를 씁니다.

use std::sync::Arc;

use chrono::{Datelike, Local};
use rusaint::client::{USaintApplication, USaintClientBuilder};
use rusaint::model::SemesterType;
use rusaint::{RusaintError, USaintSession};
use rusaint_sso::SsoCredentials;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

pub struct Usaint {
    sso: Arc<SsoCredentials>,
    session: Mutex<Option<Arc<USaintSession>>>,
}

impl Usaint {
    pub fn new(sso: Arc<SsoCredentials>) -> Self {
        Self {
            sso,
            session: Mutex::new(None),
        }
    }

    async fn session(&self, force: bool) -> Result<Arc<USaintSession>, String> {
        let mut session = self.session.lock().await;
        if let Some(s) = session.as_ref().filter(|_| !force) {
            return Ok(s.clone());
        }
        let token = self.sso.token().await.map_err(|e| e.to_string())?;
        let fresh = Arc::new(
            USaintSession::with_token(self.sso.student_id(), &token)
                .await
                .map_err(|e| format!("u-saint 세션 생성 실패: {e}"))?,
        );
        *session = Some(fresh.clone());
        Ok(fresh)
    }

    /// 다시 로그인하지 않고 지금 세션으로 u-saint 앱이 열리는지 봅니다. 세션이 없으면 `None`.
    /// 세션 수명을 재고 세션을 살려 두는 데(keep-alive) 씁니다. 끊겼으면 세션을 버립니다.
    pub async fn probe<T: USaintApplication>(&self) -> Option<bool> {
        let session = self.session.lock().await.clone()?;
        let alive = USaintClientBuilder::new()
            .session(session.clone())
            .build_into::<T>()
            .await
            .is_ok();
        if !alive {
            let mut current = self.session.lock().await;
            if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, &session)) {
                *current = None;
            }
        }
        Some(alive)
    }

    /// u-saint 앱을 엽니다. 실패하면 세션이 만료됐을 수 있어 새 세션으로 한 번 다시 엽니다.
    pub async fn app<T: USaintApplication>(&self) -> Result<T, String> {
        let session = self.session(false).await?;
        match USaintClientBuilder::new()
            .session(session)
            .build_into::<T>()
            .await
        {
            Ok(app) => Ok(app),
            Err(_) => {
                let session = self.session(true).await?;
                USaintClientBuilder::new()
                    .session(session)
                    .build_into::<T>()
                    .await
                    .map_err(err)
            }
        }
    }
}

pub fn err(e: RusaintError) -> String {
    format!("u-saint 오류: {e}")
}

/// 학기 이름("1학기", "여름학기", "2학기", "겨울학기")을 읽습니다.
pub fn parse_semester(s: &str) -> Result<SemesterType, String> {
    match s.trim() {
        "1" | "1학기" | "one" => Ok(SemesterType::One),
        "2" | "2학기" | "two" => Ok(SemesterType::Two),
        "여름" | "여름학기" | "summer" => Ok(SemesterType::Summer),
        "겨울" | "겨울학기" | "winter" => Ok(SemesterType::Winter),
        other => Err(format!(
            "학기 형식이 아닙니다: {other} (1학기, 2학기, 여름학기, 겨울학기)"
        )),
    }
}

/// 오늘 기준 정규 학기. 1~2월은 전년도 2학기, 3~8월은 1학기, 9~12월은 2학기로 봅니다.
pub fn current_semester() -> (u32, SemesterType) {
    let today = Local::now().date_naive();
    let year = today.year() as u32;
    match today.month() {
        1 | 2 => (year - 1, SemesterType::Two),
        3..=8 => (year, SemesterType::One),
        _ => (year, SemesterType::Two),
    }
}

/// 학적 기본 정보에서 내보낼 항목. 주민번호·주소·연락처·사진은 넣지 않습니다.
const STUDENT_INFO_ALLOWED: [&str; 14] = [
    "apply_year",
    "student_number",
    "name",
    "collage",
    "department",
    "major",
    "division",
    "grade",
    "term",
    "is_transfer_student",
    "plural_major",
    "sub_major",
    "connected_major",
    "abeek",
];

pub fn redact_student_info(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| STUDENT_INFO_ALLOWED.contains(&k.as_str()))
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_sensitive_fields() {
        let info = json!({
            "student_number": 20240000, "name": "홍길동", "rrn": 1234567,
            "address": "서울", "mobile_number": "010", "email": "a@b", "image": [1, 2],
            "department": "글로벌미디어학부", "grade": 3
        });
        let out = redact_student_info(info);
        let keys: Vec<_> = out.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 4);
        for banned in ["rrn", "address", "mobile_number", "email", "image"] {
            assert!(out.get(banned).is_none(), "{banned}");
        }
    }

    #[test]
    fn parses_semesters() {
        assert!(matches!(parse_semester("2학기"), Ok(SemesterType::Two)));
        assert!(matches!(parse_semester("여름"), Ok(SemesterType::Summer)));
        assert!(parse_semester("3학기").is_err());
    }
}
