//! 숭실대학교 중앙도서관(oasis.ssu.ac.kr, Pyxis) 클라이언트입니다.
//!
//! 도서관 계정은 u-saint SSO와 별개입니다. 로그인하면 받은 `Pyxis-Auth-Token`을 메모리에만 두고,
//! 만료되면 한 번 다시 로그인합니다.

mod crypto;
mod model;

use std::time::Duration;

use chrono::NaiveDate;
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub use model::*;

const BASE_URL: &str = "https://oasis.ssu.ac.kr/pyxis-api";
const HOMEPAGE_ID: u32 = 1;
const BRANCH_GROUP_ID: u32 = 1;
/// 스터디룸(그룹스터디실) 시설 유형
const STUDY_ROOM_TYPE_ID: u32 = 1;
const SMUF_METHOD: &str = "PC";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const NEED_LOGIN_CODES: [&str; 2] = [
    "error.authentication.needLogin",
    "error.authentication.invalidToken",
];

/// 도서관 API 오류
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("응답 형식이 예상과 다릅니다: {0}")]
    Decode(#[from] serde_json::Error),
    /// 서버가 `success: false`로 답한 경우
    #[error("{message} ({code})")]
    Api { code: String, message: String },
    #[error("추가 인증이 필요합니다. oasis.ssu.ac.kr 에서 직접 한 번 로그인해 주세요: {0}")]
    SecondaryAuthRequired(String),
    /// 아이디·비밀번호가 비어 있거나 이미 서버에서 거부됨. 계정 잠금(5회 실패 시 5분)을 막기 위해 다시 보내지 않습니다.
    #[error("도서관 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
}

/// Pyxis 공통 응답 봉투
#[derive(serde::Deserialize)]
struct Envelope {
    success: bool,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Value,
}

impl Envelope {
    fn is_no_record(&self) -> bool {
        self.code == "success.noRecord"
    }
}

/// 중앙도서관 클라이언트
pub struct LibraryClient {
    http: reqwest::Client,
    login_id: String,
    password: String,
    token: Mutex<Option<String>>,
    /// 서버가 계정 정보를 거부한 사유. 한 번 거부되면 이 클라이언트로는 다시 로그인하지 않습니다.
    rejected: Mutex<Option<String>>,
}

impl LibraryClient {
    /// 아직 로그인하지 않은 클라이언트를 만듭니다. 첫 인증 요청 때 로그인합니다.
    pub fn new(
        login_id: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, LibraryError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .default_headers(
                [
                    (reqwest::header::ACCEPT, "application/json"),
                    (reqwest::header::ORIGIN, "https://oasis.ssu.ac.kr"),
                    (reqwest::header::REFERER, "https://oasis.ssu.ac.kr/"),
                ]
                .into_iter()
                .map(|(k, v)| (k, reqwest::header::HeaderValue::from_static(v)))
                .collect(),
            )
            .build()?;
        Ok(Self {
            http,
            login_id: login_id.into(),
            password: password.into(),
            token: Mutex::new(None),
            rejected: Mutex::new(None),
        })
    }

    /// 로그인해서 토큰을 갱신합니다.
    pub async fn login(&self) -> Result<(), LibraryError> {
        let mut token = self.token.lock().await;
        *token = Some(self.request_token().await?);
        Ok(())
    }

    async fn request_token(&self) -> Result<String, LibraryError> {
        if self.login_id.trim().is_empty() || self.password.is_empty() {
            return Err(LibraryError::CredentialUnusable(
                "아이디 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        if let Some(reason) = self.rejected.lock().await.clone() {
            return Err(LibraryError::CredentialUnusable(reason));
        }
        let body = json!({
            "loginId": self.login_id,
            "password": crypto::encrypt_password(&self.password),
            "isFamilyLogin": false,
            "isMobile": false,
        });
        let envelope: Envelope = self
            .http
            .post(format!("{BASE_URL}/api/login"))
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if envelope.code == "warning.secondary.need.authentication" {
            return Err(LibraryError::SecondaryAuthRequired(envelope.message));
        }
        if !envelope.success {
            if envelope.code.contains("invalidCredential") {
                *self.rejected.lock().await = Some(envelope.message.clone());
            }
            return Err(LibraryError::Api {
                code: envelope.code,
                message: envelope.message,
            });
        }
        envelope
            .data
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| LibraryError::Api {
                code: envelope.code,
                message: "로그인 응답에 accessToken이 없습니다".into(),
            })
    }

    /// 인증이 필요한 요청. 토큰이 없거나 만료되면 로그인 후 한 번 재시도합니다.
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Envelope, LibraryError> {
        for attempt in 0..2 {
            let token = {
                let mut token = self.token.lock().await;
                if token.is_none() || attempt == 1 {
                    *token = Some(self.request_token().await?);
                }
                token.clone().unwrap_or_default()
            };
            let envelope = self
                .send(method.clone(), path, query, body, Some(&token))
                .await?;
            if !envelope.success
                && NEED_LOGIN_CODES.contains(&envelope.code.as_str())
                && attempt == 0
            {
                continue;
            }
            return Ok(envelope);
        }
        unreachable!("loop returns on second attempt")
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        token: Option<&str>,
    ) -> Result<Envelope, LibraryError> {
        let mut request = self
            .http
            .request(method, format!("{BASE_URL}/{HOMEPAGE_ID}{path}"))
            .query(query);
        if let Some(token) = token {
            request = request.header("Pyxis-Auth-Token", token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        Ok(request.send().await?.json().await?)
    }

    /// 성공 응답의 `data`를 꺼냅니다. 기록 없음(`success.noRecord`)은 `None`입니다.
    fn data<T: DeserializeOwned>(envelope: Envelope) -> Result<Option<T>, LibraryError> {
        if !envelope.success {
            return Err(LibraryError::Api {
                code: envelope.code,
                message: envelope.message,
            });
        }
        if envelope.is_no_record() || envelope.data.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(envelope.data)?))
    }

    fn list<T: DeserializeOwned>(envelope: Envelope) -> Result<Vec<T>, LibraryError> {
        Ok(Self::data::<Page<T>>(envelope)?
            .map(|p| p.list)
            .unwrap_or_default())
    }

    fn ok(envelope: Envelope) -> Result<Value, LibraryError> {
        Self::data::<Value>(envelope).map(Option::unwrap_or_default)
    }

    // ---- 조회 ----

    /// 열람실별 좌석 현황. 로그인이 필요 없습니다.
    pub async fn seat_rooms(&self) -> Result<Vec<SeatRoom>, LibraryError> {
        let query = [
            ("smufMethodCode", SMUF_METHOD.to_string()),
            ("branchGroupId", BRANCH_GROUP_ID.to_string()),
        ];
        Self::list(
            self.send(Method::GET, "/seat-rooms", &query, None, None)
                .await?,
        )
    }

    /// 열람실 하나의 좌석 목록
    pub async fn seats(&self, room_id: u64) -> Result<Vec<Seat>, LibraryError> {
        Self::list(
            self.call(
                Method::GET,
                &format!("/api/rooms/{room_id}/seats"),
                &[],
                None,
            )
            .await?,
        )
    }

    /// 대출 중인 자료
    pub async fn loans(&self) -> Result<Vec<Value>, LibraryError> {
        let query = [("offset", "0".to_string()), ("max", "50".to_string())];
        Self::list(self.call(Method::GET, "/api/charges", &query, None).await?)
    }

    /// 내 열람실 좌석 이용·예약 현황
    pub async fn my_seats(&self) -> Result<Vec<Charge>, LibraryError> {
        Self::list(
            self.call(Method::GET, "/api/seat-charges", &[], None)
                .await?,
        )
    }

    /// 내 스터디룸 예약 현황
    pub async fn my_rooms(&self) -> Result<Vec<Charge>, LibraryError> {
        Self::list(
            self.call(Method::GET, "/api/room-charges", &[], None)
                .await?,
        )
    }

    /// 스터디룸 예약 상세. 목록에는 없는 `isRenewable` 등이 들어 있습니다.
    pub async fn my_room(&self, charge_id: u64) -> Result<Option<Charge>, LibraryError> {
        Self::data(
            self.call(
                Method::GET,
                &format!("/api/room-charges/{charge_id}"),
                &[],
                None,
            )
            .await?,
        )
    }

    /// 날짜별 스터디룸 목록과 빈 시간대
    pub async fn study_rooms(&self, date: NaiveDate) -> Result<Vec<StudyRoom>, LibraryError> {
        let query = [
            ("roomTypeId", STUDY_ROOM_TYPE_ID.to_string()),
            ("smufMethodCode", SMUF_METHOD.to_string()),
            ("branchGroupId", BRANCH_GROUP_ID.to_string()),
            ("hopeDate", date.to_string()),
        ];
        let rooms: Vec<RawStudyRoom> =
            Self::list(self.call(Method::GET, "/api/rooms", &query, None).await?)?;
        Ok(rooms.into_iter().map(StudyRoom::from).collect())
    }

    /// 스터디룸 상세(이용 규칙, 예약 가능 날짜, 빈 시간대)
    pub async fn study_room(
        &self,
        room_id: u64,
        date: NaiveDate,
    ) -> Result<Option<StudyRoom>, LibraryError> {
        let query = [("hopeDate", date.to_string())];
        let room: Option<RawStudyRoom> = Self::data(
            self.call(Method::GET, &format!("/api/rooms/{room_id}"), &query, None)
                .await?,
        )?;
        Ok(room.map(StudyRoom::from))
    }

    /// 동반 이용자를 확인합니다. 확인된 이용자의 `id`를 예약에 넣습니다.
    pub async fn check_companion(
        &self,
        room_id: u64,
        name: &str,
        member_no: &str,
        date: NaiveDate,
    ) -> Result<Value, LibraryError> {
        let query = [
            ("name", name.to_string()),
            ("memberNo", member_no.to_string()),
            ("hopeDate", date.to_string()),
        ];
        Self::ok(
            self.call(
                Method::GET,
                &format!("/api/rooms/{room_id}/check-companions"),
                &query,
                None,
            )
            .await?,
        )
    }

    // ---- 쓰기 ----

    /// 열람실 좌석을 지금부터 배정받습니다.
    pub async fn reserve_seat(&self, seat_id: u64) -> Result<Value, LibraryError> {
        let body = json!({ "seatId": seat_id, "smufMethodCode": SMUF_METHOD });
        Self::ok(
            self.call(Method::POST, "/api/seat-charges", &[], Some(&body))
                .await?,
        )
    }

    /// 사용 중인 좌석을 반납합니다.
    pub async fn return_seat(&self, charge_id: u64) -> Result<Value, LibraryError> {
        let body = json!({ "seatCharge": charge_id, "smufMethodCode": SMUF_METHOD });
        Self::ok(
            self.call(Method::POST, "/api/seat-discharges", &[], Some(&body))
                .await?,
        )
    }

    /// 임시배정(입실 전) 좌석을 취소합니다.
    ///
    /// 2026-11 실측: 이 요청(DELETE)은 브라우저가 아닌 클라이언트에서 보내면 웹 방화벽이
    /// 403 "Restricted Access Page" 로 막습니다. 홈페이지에서는 같은 요청이 동작합니다.
    pub async fn cancel_seat(&self, charge_id: u64) -> Result<Value, LibraryError> {
        let query = [("smufMethodCode", SMUF_METHOD.to_string())];
        Self::ok(
            self.call(
                Method::DELETE,
                &format!("/api/seat-charges/{charge_id}"),
                &query,
                None,
            )
            .await?,
        )
    }

    /// 좌석 이용 시간을 연장합니다. 도착 확인이 필요한 예약은 [`Self::confirm_arrival`]을 먼저 통과해야 합니다.
    pub async fn renew_seat(&self, charge_id: u64) -> Result<Value, LibraryError> {
        let body = json!({ "seatCharge": charge_id, "smufMethodCode": SMUF_METHOD });
        Self::ok(
            self.call(Method::POST, "/api/seat-renewed-charges", &[], Some(&body))
                .await?,
        )
    }

    /// 스터디룸을 예약합니다.
    pub async fn reserve_room(&self, request: &RoomReservation) -> Result<Value, LibraryError> {
        let body = request.to_body(self.use_section_id(request.room_id).await?);
        Self::ok(
            self.call(Method::POST, "/api/room-charges", &[], Some(&body))
                .await?,
        )
    }

    /// 스터디룸 예약을 취소합니다.
    pub async fn cancel_room(&self, charge_id: u64) -> Result<Value, LibraryError> {
        Self::ok(
            self.call(
                Method::DELETE,
                &format!("/api/room-charges/{charge_id}"),
                &[],
                None,
            )
            .await?,
        )
    }

    /// 스터디룸 이용 시간을 연장합니다.
    pub async fn renew_room(&self, charge_id: u64) -> Result<Value, LibraryError> {
        let body = json!({ "roomChargeId": charge_id, "smufMethodCode": SMUF_METHOD });
        Self::ok(
            self.call(Method::POST, "/api/room-renewed-charges", &[], Some(&body))
                .await?,
        )
    }

    /// 연장 전 도착 확인. 실제 재실 여부를 서버가 판단하는 방식(`GATE`)만 지원합니다.
    /// `GPS`·`BEACON`·`RF_TAG`는 기기 정보가 필요하므로 보내지 않습니다.
    pub async fn confirm_arrival(&self, charge: &Charge) -> Result<ArrivalCheck, LibraryError> {
        let Some(methods) = charge.arrival_confirm_methods.as_deref() else {
            return Ok(ArrivalCheck::NotRequired);
        };
        if !methods.iter().any(|m| m == "GATE") {
            return Ok(ArrivalCheck::Unsupported(methods.to_vec()));
        }
        let Some(room_id) = charge.room_id() else {
            return Ok(ArrivalCheck::Unsupported(methods.to_vec()));
        };
        let body = json!({ "methodCode": "GATE" });
        let passed = Self::ok(
            self.call(
                Method::POST,
                &format!("/api/rooms/{room_id}/check-arrival"),
                &[],
                Some(&body),
            )
            .await?,
        )?;
        Ok(if passed == Value::Bool(true) {
            ArrivalCheck::Passed
        } else {
            ArrivalCheck::Failed
        })
    }

    /// 스터디룸 이용 목적 구분. 사이트 기본값처럼 첫 번째 항목을 쓰고, 없으면 1입니다.
    async fn use_section_id(&self, room_id: u64) -> Result<u64, LibraryError> {
        let sections: Vec<Value> = Self::list(
            self.call(
                Method::GET,
                &format!("/api/rooms/{room_id}/use-sections"),
                &[],
                None,
            )
            .await?,
        )
        .unwrap_or_default();
        Ok(sections
            .first()
            .and_then(|s| s.get("id"))
            .and_then(Value::as_u64)
            .unwrap_or(1))
    }
}

/// 도착 확인 결과
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", content = "methods", rename_all = "snake_case")]
pub enum ArrivalCheck {
    /// 이 예약은 도착 확인이 필요 없음
    NotRequired,
    Passed,
    Failed,
    /// 지원하지 않는 확인 방식만 허용됨
    Unsupported(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(json: &str) -> Envelope {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_seat_rooms() {
        let rooms: Vec<SeatRoom> =
            LibraryClient::list(envelope(include_str!("../tests/fixtures/seatRooms.json")))
                .unwrap();
        assert_eq!(rooms.len(), 7);
        assert!(rooms.iter().all(|r| r.seats.total > 0));
    }

    #[test]
    fn parses_seats() {
        let seats: Vec<Seat> =
            LibraryClient::list(envelope(include_str!("../tests/fixtures/seats.json"))).unwrap();
        assert!(!seats.is_empty());
        assert!(seats.iter().all(|s| !s.code.is_empty()));
    }

    #[test]
    fn parses_study_rooms_with_free_ranges() {
        let rooms: Vec<RawStudyRoom> =
            LibraryClient::list(envelope(include_str!("../tests/fixtures/rooms.json"))).unwrap();
        let rooms: Vec<StudyRoom> = rooms.into_iter().map(StudyRoom::from).collect();
        assert_eq!(rooms.len(), 8);
        for room in &rooms {
            for range in &room.free_ranges {
                assert_eq!(range.len(), "13:00-15:00".len(), "{range}");
            }
        }
        let detail: Option<RawStudyRoom> =
            LibraryClient::data(envelope(include_str!("../tests/fixtures/roomDetail.json")))
                .unwrap();
        let detail = StudyRoom::from(detail.unwrap());
        let rule = detail.rule.unwrap();
        assert_eq!((rule.min_time, rule.max_time), (60, 180));
        assert!(!detail.reservable_dates.is_empty());
    }

    #[test]
    fn api_error_and_no_record() {
        let err = LibraryClient::list::<Value>(envelope(
            r#"{"success":false,"code":"error.authentication.needLogin","message":"로그인이 필요합니다."}"#,
        ))
        .unwrap_err();
        assert!(
            matches!(err, LibraryError::Api { ref code, .. } if code == "error.authentication.needLogin")
        );
        let empty = LibraryClient::list::<Charge>(envelope(
            r#"{"success":true,"code":"success.noRecord","message":"","data":null}"#,
        ))
        .unwrap();
        assert!(empty.is_empty());
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[tokio::test]
    async fn empty_password_is_never_sent() {
        let client = LibraryClient::new("20240000", "").unwrap();
        let err = client.login().await.unwrap_err();
        assert!(
            matches!(err, LibraryError::CredentialUnusable(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejected_credentials_are_not_retried() {
        let client = LibraryClient::new("20240000", "pw").unwrap();
        *client.rejected.lock().await = Some("ID or password is not correct.".into());
        let err = client.login().await.unwrap_err();
        assert!(
            matches!(err, LibraryError::CredentialUnusable(_)),
            "{err:?}"
        );
    }
}
