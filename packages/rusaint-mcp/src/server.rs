use std::sync::Arc;

use chrono::{Local, NaiveDate, NaiveTime, Timelike};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use rusaint_library::{LibraryClient, RoomReservation};
use rusaint_meal::{Cafeteria, MealClient};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OnceCell;

use crate::automation::{Automation, MAX_RENEW_HOURS};
use crate::keychain;
use crate::pending::{Action, Pending};

const INSTRUCTIONS: &str = "숭실대학교 학식 메뉴와 중앙도서관(열람실 좌석, 스터디룸, 대출)을 다루는 비공식 도구입니다. 학교 사이트가 바뀌면 틀리거나 실패할 수 있으니, 중요한 결정 전에는 공식 홈페이지 확인을 권하세요.
- 날짜는 YYYY-MM-DD, 시각은 HH:MM(한국 시간)입니다. 생략하면 오늘입니다.
- 도서관 쓰기 작업은 prepare_* 의 summary 를 사용자에게 그대로 보여 주고, 그 작업에 대한 사용자의 명시적 동의를 받은 뒤에만 confirm_library_action 을 호출합니다. 동의는 작업 하나에만 유효하며, 내용이 바뀌면 다시 prepare 합니다.
- 도서관 로그인이 계정 정보 오류로 한 번 거부되면 이 서버는 재시작 전까지 다시 시도하지 않습니다(계정 잠금 방지). 재시도를 반복하지 말고, 터미널에서 Keychain 항목을 고친 뒤 앱을 재시작하라고 안내하세요.";

type ToolResult = Result<String, String>;

fn to_json<T: Serialize>(value: &T) -> ToolResult {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

fn parse_date(date: Option<&str>) -> Result<NaiveDate, String> {
    match date {
        None | Some("") => Ok(Local::now().date_naive()),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| format!("날짜 형식이 아닙니다: {s} (YYYY-MM-DD)")),
    }
}

fn parse_time(time: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(time, "%H:%M")
        .map_err(|_| format!("시각 형식이 아닙니다: {time} (HH:MM)"))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MealsRequest {
    /// 날짜 (YYYY-MM-DD). 생략하면 오늘
    pub date: Option<String>,
    /// 식당 이름: 학생식당, 도담, 스낵코너, 푸드코트, THE KITCHEN, FACULTY LOUNGE. 생략하면 전체
    pub cafeteria: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DateRequest {
    /// 날짜 (YYYY-MM-DD). 생략하면 오늘
    pub date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SeatsRequest {
    /// library_seat_rooms 의 열람실 id
    pub room_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompanionRequest {
    pub room_id: u64,
    /// 동반 이용자 이름
    pub name: String,
    /// 동반 이용자 학번(회원번호)
    pub member_no: String,
    pub date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeatActionKind {
    /// 좌석 배정 (seat_id 필요)
    Reserve,
    /// 입실 확정된 좌석 반납 (charge_id 필요). 임시배정 상태에서는 서버가 거부합니다
    Return,
    /// 이용 시간 연장 (charge_id 필요)
    Renew,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PrepareSeatRequest {
    pub action: SeatActionKind,
    /// 배정할 좌석 id (library_seats 의 id)
    pub seat_id: Option<u64>,
    /// 내 좌석 이용 건 id (library_my_status 의 seats[].id)
    pub charge_id: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PrepareRoomRequest {
    pub room_id: u64,
    /// 날짜 (YYYY-MM-DD)
    pub date: String,
    /// 시작 시각 (HH:MM, library_study_rooms 의 start_times 중 하나)
    pub begin: String,
    /// 종료 시각 (HH:MM)
    pub end: String,
    /// library_check_companion 으로 확인한 동반 이용자 id 목록 (본인 제외)
    #[serde(default)]
    pub companion_ids: Vec<u64>,
    /// 이용 목적 메모
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoomActionKind {
    Cancel,
    Renew,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PrepareRoomActionRequest {
    pub action: RoomActionKind,
    /// 내 스터디룸 예약 id (library_my_status 의 rooms[].id)
    pub charge_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfirmRequest {
    /// prepare_* 가 돌려준 action_id
    pub action_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AutoRenewRequest {
    /// 자동 연장을 유지할 시간(시간, 1~12). 기본 4
    pub hours: Option<u64>,
}

#[derive(Clone)]
pub struct SsuServer {
    meal: Arc<MealClient>,
    library: Arc<OnceCell<Arc<LibraryClient>>>,
    pending: Arc<Pending>,
    automation: Arc<Automation>,
    tool_router: ToolRouter<Self>,
}

impl SsuServer {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            meal: Arc::new(MealClient::new().map_err(|e| e.to_string())?),
            library: Arc::new(OnceCell::new()),
            pending: Arc::new(Pending::new()),
            automation: Arc::new(Automation::default()),
            tool_router: Self::tool_router(),
        })
    }

    /// 도서관 클라이언트는 처음 필요할 때 Keychain 에서 계정을 읽어 만듭니다.
    async fn library(&self) -> Result<Arc<LibraryClient>, String> {
        self.library
            .get_or_try_init(|| async {
                let (id, password) = keychain::credentials(keychain::LIBRARY_SERVICE).await?;
                LibraryClient::new(id, password)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .await
            .cloned()
    }

    async fn prepared(&self, action: Action, summary: String) -> ToolResult {
        let action_id = self.pending.insert(action, summary.clone()).await;
        to_json(&json!({
            "action_id": action_id,
            "summary": summary,
            "expires_in_minutes": 5,
            "next": "사용자에게 summary 를 보여 주고 동의를 받은 뒤 confirm_library_action 을 호출하세요",
        }))
    }
}

#[tool_router]
impl SsuServer {
    /// 교내 식당(soongguri.com) 하루 메뉴. 알레르기·원산지 정보는 제외됩니다.
    /// lines 는 화면 그대로의 텍스트입니다: 가격 "- 5.0" 은 5,000원, "★ 메뉴" 줄과 "메뉴, 메뉴- 6.0" 줄은 같은 메뉴입니다.
    /// slot(중식1 등)은 내용과 다를 수 있으니 section 과 본문으로 판단하세요. status=closed 는 미래 날짜면 미등록일 수 있습니다.
    /// 메뉴 이름은 영문 설명을 참고해 바꾸지 말고 원문 그대로 쓰세요. 식당을 지정하지 않으면 6곳을 차례로 조회해 몇 초 걸립니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn get_meals(&self, Parameters(req): Parameters<MealsRequest>) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        let cafeterias = match req.cafeteria.as_deref() {
            None | Some("") => Cafeteria::ALL.to_vec(),
            Some(name) => vec![name.parse::<Cafeteria>().map_err(|e| e.to_string())?],
        };
        let mut menus = Vec::new();
        for cafeteria in cafeterias {
            menus.push(
                self.meal
                    .campus(cafeteria, date)
                    .await
                    .map_err(|e| e.to_string())?,
            );
        }
        to_json(&menus)
    }

    /// 기숙사 식당 주간 식단 (date 가 속한 월~일). 아직 올라오지 않은 주는 빈 목록이고, 끼니에 "미운영"이 들어갈 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn get_dorm_meals(&self, Parameters(req): Parameters<DateRequest>) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        to_json(&self.meal.dorm_week(date).await.map_err(|e| e.to_string())?)
    }

    /// 열람실별 전체·사용 중·빈 좌석 수. unableMessage 가 있으면 지금 이용할 수 없는 열람실입니다.
    /// 빈자리를 기다리려고 이 도구나 library_seats 를 반복 호출하지 마세요(학교 서버 부담, 노쇼 위험).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn library_seat_rooms(&self) -> ToolResult {
        let client = self.library().await?;
        to_json(&client.seat_rooms().await.map_err(|e| e.to_string())?)
    }

    /// 열람실 하나의 빈 좌석 목록. 배정에는 여기의 seat_id 를 씁니다. 반복 호출로 빈자리를 기다리지 마세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn library_seats(&self, Parameters(req): Parameters<SeatsRequest>) -> ToolResult {
        let client = self.library().await?;
        let seats = client.seats(req.room_id).await.map_err(|e| e.to_string())?;
        let free: Vec<_> = seats
            .iter()
            .filter(|s| s.is_free())
            .map(|s| json!({ "seat_id": s.id, "code": s.code, "type": s.seat_type.as_ref().map(|t| &t.name) }))
            .collect();
        to_json(&json!({ "total": seats.len(), "free_count": free.len(), "free": free }))
    }

    /// 내 대출 자료, 열람실 좌석 이용·예약, 스터디룸 예약. seats[].id·rooms[].id 가 반납·연장에 쓰는 charge_id 입니다.
    /// state 가 임시배정(TEMP_CHARGE)이면 아직 입실 전이며, renewableDate 이후(종료 30분 전)부터 renewalLimit 회까지 연장할 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn library_my_status(&self) -> ToolResult {
        let client = self.library().await?;
        let (loans, seats, rooms) =
            tokio::join!(client.loans(), client.my_seats(), client.my_rooms());
        to_json(&json!({
            "loans": loans.map_err(|e| e.to_string())?,
            "seats": seats.map_err(|e| e.to_string())?,
            "rooms": rooms.map_err(|e| e.to_string())?,
        }))
    }

    /// 날짜별 스터디룸(세미나룸) 빈 시간대. free_ranges 는 비어 있는 시간, 예약 시작은 start_times(정각)만 가능합니다.
    /// 인원(min_quota~max_quota)은 본인 포함이며, 이용 시간 규칙은 prepare 단계에서 확인합니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn library_study_rooms(&self, Parameters(req): Parameters<DateRequest>) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        let client = self.library().await?;
        to_json(&client.study_rooms(date).await.map_err(|e| e.to_string())?)
    }

    /// 스터디룸 동반 이용자 확인. 결과의 id 를 prepare_library_room_reservation 에 넣습니다.
    /// 이름·학번은 제3자 개인정보이므로 사용자가 직접 알려 준 값만 쓰고, 추측하거나 다른 용도로 쓰지 마세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn library_check_companion(
        &self,
        Parameters(req): Parameters<CompanionRequest>,
    ) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        let client = self.library().await?;
        to_json(
            &client
                .check_companion(req.room_id, &req.name, &req.member_no, date)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 열람실 좌석 배정·반납·연장을 준비합니다. 실행은 confirm_library_action 으로 합니다.
    /// 배정된 좌석(임시배정)은 이 서버로 취소할 수 없으니 배정 전에 사용자에게 꼭 알리세요.
    #[tool(annotations(read_only_hint = true))]
    async fn prepare_library_seat_action(
        &self,
        Parameters(req): Parameters<PrepareSeatRequest>,
    ) -> ToolResult {
        let need_charge = || req.charge_id.ok_or("charge_id 가 필요합니다");
        let (action, summary) = match req.action {
            SeatActionKind::Reserve => {
                let seat_id = req.seat_id.ok_or("seat_id 가 필요합니다")?;
                (
                    Action::ReserveSeat { seat_id },
                    format!(
                        "열람실 좌석 {seat_id} 을(를) 지금부터 배정받습니다. \
                         임시배정으로 시작하며 20분 안에 도서관 게이트를 통과하지 않으면 노쇼가 됩니다. \
                         배정 후 취소는 이 도구로 할 수 없고 도서관 홈페이지나 앱에서 직접 해야 합니다"
                    ),
                )
            }
            SeatActionKind::Return => {
                let charge_id = need_charge()?;
                (
                    Action::ReturnSeat { charge_id },
                    format!("사용 중인 좌석(이용 건 {charge_id})을 반납합니다"),
                )
            }
            SeatActionKind::Renew => {
                let charge_id = need_charge()?;
                (
                    Action::RenewSeat { charge_id },
                    format!("좌석 이용 건 {charge_id} 의 시간을 연장합니다"),
                )
            }
        };
        self.prepared(action, summary).await
    }

    /// 스터디룸 예약을 준비합니다. 빈 시간·이용 시간 규칙을 미리 확인합니다.
    #[tool(annotations(read_only_hint = true))]
    async fn prepare_library_room_reservation(
        &self,
        Parameters(req): Parameters<PrepareRoomRequest>,
    ) -> ToolResult {
        let date = parse_date(Some(&req.date))?;
        let (begin, end) = (parse_time(&req.begin)?, parse_time(&req.end)?);
        if end <= begin {
            return Err("종료 시각이 시작 시각보다 늦어야 합니다".into());
        }
        let client = self.library().await?;
        let room = client
            .study_room(req.room_id, date)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("스터디룸을 찾을 수 없습니다")?;
        if !room.start_times.contains(&req.begin) {
            return Err(format!(
                "{} 은(는) 시작할 수 없는 시각입니다. 가능한 시각: {:?}",
                req.begin, room.start_times
            ));
        }
        let minutes = (end - begin).num_minutes() as u32;
        if let Some(rule) = &room.rule
            && !(rule.min_time..=rule.max_time).contains(&minutes)
        {
            return Err(format!(
                "이용 시간은 {}~{}분이어야 합니다 (요청 {minutes}분)",
                rule.min_time, rule.max_time
            ));
        }
        let to_min = |t: NaiveTime| t.hour() * 60 + t.minute();
        let covered = room.free_ranges.iter().any(|range| {
            let Some((s, e)) = range.split_once('-') else {
                return false;
            };
            match (parse_time(s), parse_time(e)) {
                (Ok(s), Ok(e)) => to_min(s) <= to_min(begin) && to_min(end) <= to_min(e),
                // 자정을 넘는 구간("23:00-24:00")은 NaiveTime 으로 못 읽으므로 시작만 비교
                (Ok(s), Err(_)) => to_min(s) <= to_min(begin),
                _ => false,
            }
        });
        if !covered {
            return Err(format!(
                "{}~{} 은(는) 비어 있지 않습니다. 빈 시간: {:?}",
                req.begin, req.end, room.free_ranges
            ));
        }
        let people = req.companion_ids.len() as u32 + 1;
        if !(room.min_quota..=room.max_quota).contains(&people) {
            return Err(format!(
                "{} 은(는) {}~{}명이 이용해야 합니다 (본인 포함 {people}명). 동반자를 library_check_companion 으로 확인해 주세요",
                room.name, room.min_quota, room.max_quota
            ));
        }
        let summary = format!(
            "{} {} {}~{} 예약 (본인 포함 {people}명)",
            room.name, date, req.begin, req.end
        );
        let reservation = RoomReservation {
            room_id: req.room_id,
            date,
            begin,
            end,
            companion_ids: req.companion_ids,
            message: req.message,
        };
        self.prepared(Action::ReserveRoom(reservation), summary)
            .await
    }

    /// 스터디룸 예약 취소·연장을 준비합니다.
    #[tool(annotations(read_only_hint = true))]
    async fn prepare_library_room_action(
        &self,
        Parameters(req): Parameters<PrepareRoomActionRequest>,
    ) -> ToolResult {
        let charge_id = req.charge_id;
        let (action, summary) = match req.action {
            RoomActionKind::Cancel => (
                Action::CancelRoom { charge_id },
                format!(
                    "스터디룸 예약 {charge_id} 을(를) 취소합니다. \
                     좌석 취소처럼 웹 방화벽에 막힐 수 있으며, 실패하면 홈페이지에서 직접 취소해야 합니다"
                ),
            ),
            RoomActionKind::Renew => (
                Action::RenewRoom { charge_id },
                format!("스터디룸 예약 {charge_id} 을(를) 연장합니다"),
            ),
        };
        self.prepared(action, summary).await
    }

    /// prepare_* 로 준비하고 사용자가 동의한 도서관 작업을 실행합니다.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    async fn confirm_library_action(
        &self,
        Parameters(req): Parameters<ConfirmRequest>,
    ) -> ToolResult {
        let (action, summary) = self.pending.take(&req.action_id).await?;
        let client = self.library().await?;
        let result = match action {
            Action::ReserveSeat { seat_id } => client.reserve_seat(seat_id).await,
            Action::ReturnSeat { charge_id } => client.return_seat(charge_id).await,
            Action::RenewSeat { charge_id } => {
                let charge = client
                    .my_seats()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|c| c.id == charge_id)
                    .ok_or("해당 좌석 이용 건이 없습니다")?;
                ensure_arrival(&client, &charge).await?;
                client.renew_seat(charge_id).await
            }
            Action::ReserveRoom(reservation) => client.reserve_room(&reservation).await,
            Action::CancelRoom { charge_id } => client.cancel_room(charge_id).await,
            Action::RenewRoom { charge_id } => {
                let charge = client
                    .my_room(charge_id)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("해당 스터디룸 예약이 없습니다")?;
                ensure_arrival(&client, &charge).await?;
                client.renew_room(charge_id).await
            }
        };
        let data = result.map_err(|e| format!("{summary}: 실패 - {e}"))?;
        to_json(&json!({ "done": summary, "result": data }))
    }

    /// 내 좌석·스터디룸이 연장 가능해지면 자동으로 연장합니다 (60초 간격, 최대 12시간). 사용자가 직접 요청했을 때만 켜세요.
    /// 이 MCP 서버(앱/세션)가 켜져 있는 동안만 동작하며, 앱을 닫으면 멈춘다는 점을 사용자에게 알리세요.
    /// 연장은 도서관 게이트 출입 기록(GATE)이 있어야 통과하므로 도서관 밖에서는 실패 기록만 남습니다. 결과는 library_auto_renew_status 로 확인합니다.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    async fn library_auto_renew(
        &self,
        Parameters(req): Parameters<AutoRenewRequest>,
    ) -> ToolResult {
        let client = self.library().await?;
        let hours = req.hours.unwrap_or(4).min(MAX_RENEW_HOURS);
        to_json(&self.automation.start_renew(client, hours).await?)
    }

    /// 자동 연장 작업의 상태와 기록
    #[tool(annotations(read_only_hint = true))]
    async fn library_auto_renew_status(&self) -> ToolResult {
        to_json(&self.automation.renew_status().await)
    }

    /// 자동 연장을 중지합니다.
    #[tool(annotations(read_only_hint = false, destructive_hint = false))]
    async fn library_stop_auto_renew(&self) -> ToolResult {
        to_json(&self.automation.stop_renew().await?)
    }
}

/// 연장 전 도착 확인. 지원하지 않는 방식이면 이유와 함께 거절합니다.
async fn ensure_arrival(
    client: &LibraryClient,
    charge: &rusaint_library::Charge,
) -> Result<(), String> {
    use rusaint_library::ArrivalCheck::*;
    match client
        .confirm_arrival(charge)
        .await
        .map_err(|e| e.to_string())?
    {
        NotRequired | Passed => Ok(()),
        Failed => Err("도착 확인에 실패했습니다 (게이트 출입 기록 없음)".into()),
        Unsupported(methods) => Err(format!(
            "이 예약은 {methods:?} 도착 확인이 필요해 도서관 앱에서 직접 연장해야 합니다"
        )),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SsuServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "rusaint-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}
