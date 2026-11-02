use std::sync::Arc;

use chrono::{Local, NaiveDate, NaiveTime, Timelike};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use rusaint::application::chapel::ChapelApplication;
use rusaint::application::course_grades::CourseGradesApplication;
use rusaint::application::course_grades::model::CourseType;
use rusaint::application::course_registration_status::CourseRegistrationStatusApplication;
use rusaint::application::graduation_requirements::GraduationRequirementsApplication;
use rusaint::application::personal_course_schedule::PersonalCourseScheduleApplication;
use rusaint::application::scholarships::ScholarshipsApplication;
use rusaint::application::student_information::StudentInformationApplication;
use rusaint::model::SemesterType;
use rusaint_library::{LibraryClient, RoomReservation};
use rusaint_lms::{LmsClient, Term};
use rusaint_meal::{Cafeteria, MealClient};
use rusaint_path::{Eligibility, PathClient, ProgramQuery};
use rusaint_sso::SsoCredentials;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OnceCell;

use crate::automation::{Automation, MAX_RENEW_HOURS};
use crate::keychain;
use crate::pending::{Action, Pending};
use crate::usaint::{self, Usaint};

const INSTRUCTIONS: &str = "숭실대학교 u-saint(성적·시간표·졸업사정·채플·장학금·학적), 학식 메뉴, LMS(강의·과제·자료·공지·게시판), SSU-PATH(비교과 프로그램), 중앙도서관(열람실 좌석, 스터디룸, 대출)을 다루는 비공식 도구입니다. 학교 사이트가 바뀌면 틀리거나 실패할 수 있으니, 중요한 결정 전에는 공식 홈페이지 확인을 권하세요.
- 날짜는 YYYY-MM-DD, 시각은 HH:MM(한국 시간)입니다. 생략하면 오늘입니다.
- 도서관 쓰기 작업은 prepare_* 의 summary 를 사용자에게 그대로 보여 주고, 그 작업에 대한 사용자의 명시적 동의를 받은 뒤에만 confirm_library_action 을 호출합니다. 동의는 작업 하나에만 유효하며, 내용이 바뀌면 다시 prepare 합니다.
- 도서관·통합(SSO) 로그인이 계정 정보 오류로 한 번 거부되면 이 서버는 재시작 전까지 다시 시도하지 않습니다(계정 잠금 방지). 재시도를 반복하지 말고, 터미널에서 Keychain 항목(rusaint-library / rusaint-sso)을 고친 뒤 앱을 재시작하라고 안내하세요.
- 성적·과제·학적 같은 개인 정보는 사용자가 요청한 범위에서만 보여 주고 다른 곳에 옮기지 마세요.";

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
pub struct HoursRequest {
    /// 코너별 배식 시간을 볼 날짜 (YYYY-MM-DD, 이번 주만 나옴). 생략하면 오늘
    pub date: Option<String>,
    /// true 면 편의점·카페·서점 운영 시간도 포함
    #[serde(default)]
    pub include_other_stores: bool,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TermRequest {
    /// 학기 id (lms_courses 의 terms[].id). 생략하면 현재 정규 학기
    pub term_id: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CourseRequest {
    /// 강의 id (lms_courses 의 courses[].id)
    pub course_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnnouncementsRequest {
    /// 강의 id. 생략하면 현재 학기 모든 강의
    pub course_id: Option<u64>,
    /// true 면 안 읽은 공지만
    #[serde(default)]
    pub unread_only: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnnouncementRequest {
    pub course_id: u64,
    /// lms_announcements 의 id
    pub announcement_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PostsRequest {
    pub course_id: u64,
    /// lms_boards 의 id
    pub board_id: u64,
    /// 페이지 (1부터). 생략하면 1
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentsRequest {
    pub course_id: u64,
    pub board_id: u64,
    /// lms_posts 의 items[].id
    pub post_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemesterRequest {
    /// 연도 (예: 2026). 생략하면 현재 학기
    pub year: Option<u32>,
    /// 1학기, 2학기, 여름학기, 겨울학기. 생략하면 현재 학기
    pub semester: Option<String>,
}

impl SemesterRequest {
    fn resolve(&self) -> Result<(u32, SemesterType), String> {
        let (year, semester) = usaint::current_semester();
        Ok((
            self.year.unwrap_or(year),
            match self.semester.as_deref() {
                Some(s) => usaint::parse_semester(s)?,
                None => semester,
            },
        ))
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PathProgramsRequest {
    /// 프로그램명·키워드 검색어
    pub keyword: Option<String>,
    /// 페이지 (1부터, 한 페이지 10개). 생략하면 1
    pub page: Option<u32>,
    /// true 면 신청 대상이 아닌 것으로 보이는 프로그램(다른 학과 전용 등)도 포함
    #[serde(default)]
    pub include_ineligible: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PathDetailRequest {
    /// path_programs 의 items[].id
    pub program_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PostRequest {
    pub course_id: u64,
    pub board_id: u64,
    /// lms_posts 의 items[].id
    pub post_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentRequest {
    pub course_id: u64,
    pub board_id: u64,
    pub post_id: u64,
    /// lms_post 의 attachments[].id
    pub attachment_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadRequest {
    pub course_id: u64,
    /// lms_materials 의 material.content_id
    pub content_id: String,
}

#[derive(Clone)]
pub struct SsuServer {
    meal: Arc<MealClient>,
    library: Arc<OnceCell<Arc<LibraryClient>>>,
    lms: Arc<OnceCell<Arc<LmsClient>>>,
    usaint: Arc<OnceCell<Arc<Usaint>>>,
    path: Arc<OnceCell<Arc<PathClient>>>,
    /// u-saint·LMS·SSU-PATH 가 함께 쓰는 통합 계정. 거부되면 세 서비스 모두 다시 시도하지 않습니다.
    sso: Arc<OnceCell<Arc<SsoCredentials>>>,
    pending: Arc<Pending>,
    automation: Arc<Automation>,
    tool_router: ToolRouter<Self>,
}

impl SsuServer {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            meal: Arc::new(MealClient::new().map_err(|e| e.to_string())?),
            library: Arc::new(OnceCell::new()),
            lms: Arc::new(OnceCell::new()),
            usaint: Arc::new(OnceCell::new()),
            path: Arc::new(OnceCell::new()),
            sso: Arc::new(OnceCell::new()),
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

    /// 통합 계정은 처음 필요할 때 Keychain 에서 한 번 읽어 세 서비스가 공유합니다.
    async fn sso(&self) -> Result<Arc<SsoCredentials>, String> {
        self.sso
            .get_or_try_init(|| async {
                let (id, password) = keychain::credentials(keychain::SSO_SERVICE).await?;
                Ok::<_, String>(SsoCredentials::new(id, password))
            })
            .await
            .cloned()
    }

    async fn lms(&self) -> Result<Arc<LmsClient>, String> {
        let sso = self.sso().await?;
        self.lms
            .get_or_try_init(|| async {
                LmsClient::new(sso).map(Arc::new).map_err(|e| e.to_string())
            })
            .await
            .cloned()
    }

    /// u-saint 세션도 처음 필요할 때 Keychain 의 통합 계정으로 만듭니다.
    async fn usaint(&self) -> Result<Arc<Usaint>, String> {
        let sso = self.sso().await?;
        self.usaint
            .get_or_try_init(|| async { Ok::<_, String>(Arc::new(Usaint::new(sso))) })
            .await
            .cloned()
    }

    /// SSU-PATH 도 통합 계정을 씁니다. 세션은 30분이면 끝나며, 만료되면 클라이언트가 한 번 다시 로그인합니다.
    async fn path(&self) -> Result<Arc<PathClient>, String> {
        let sso = self.sso().await?;
        self.path
            .get_or_try_init(|| async { Ok::<_, String>(Arc::new(PathClient::new(sso))) })
            .await
            .cloned()
    }

    /// 현재 학기 강의 이름 (다운로드 폴더 이름용). 못 찾으면 강의 id 를 씁니다.
    async fn lms_course_name(&self, client: &LmsClient, course_id: u64) -> Result<String, String> {
        let (term, _) = self.lms_term(client, None).await?;
        Ok(client
            .courses(term.id)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|c| c.id == course_id)
            .map(|c| c.name)
            .unwrap_or_else(|| course_id.to_string()))
    }

    /// term_id 가 없으면 현재 정규 학기를 고릅니다.
    async fn lms_term(
        &self,
        client: &LmsClient,
        term_id: Option<u64>,
    ) -> Result<(Term, Vec<Term>), String> {
        let terms = client.terms().await.map_err(|e| e.to_string())?;
        let term = match term_id {
            Some(id) => terms.iter().find(|t| t.id == id),
            None => terms
                .iter()
                .find(|t| t.default && t.name.contains("학기"))
                .or_else(|| terms.iter().find(|t| t.default)),
        }
        .cloned()
        .ok_or("학기를 찾을 수 없습니다")?;
        Ok((term, terms))
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
    /// slot(중식1 등)은 내용과 다를 수 있으니 section 과 본문으로 판단하세요(예: 석식1 칸의 [천원의아침밥]은 실제로 아침). 정확한 코너 이름과 배식 시간은 get_cafeteria_hours 의 corners 에 있습니다. status=closed 는 미래 날짜면 미등록일 수 있습니다.
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

    /// 학식·기숙사 식당 운영 시간과 위치·연락처, 코너별 배식 시간.
    /// stores 는 매장시간 페이지, corners 는 그날 식단 페이지 기준이라 값이 다를 수 있으니(예: 학생식당 아침 08:00~09:00 / 08:10~09:20)
    /// 특정 날짜에는 corners 를 우선하세요. corners 는 이번 주(월~토)만 나오고, 모바일 식단의 slot 과 달리 코너 이름이 정확합니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn get_cafeteria_hours(&self, Parameters(req): Parameters<HoursRequest>) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        let mut stores = self.meal.store_hours().await.map_err(|e| e.to_string())?;
        if !req.include_other_stores {
            stores.retain(|s| s.category == "식당");
        }
        let corners = self
            .meal
            .corner_hours(date)
            .await
            .map_err(|e| e.to_string())?;
        let dorm = self.meal.dorm_hours().await.map_err(|e| e.to_string())?;
        to_json(&json!({
            "date": date,
            "stores": stores,
            "corners": corners.into_iter().filter(|c| !c.corners.is_empty()).collect::<Vec<_>>(),
            "dorm": dorm,
        }))
    }

    /// 기숙사 식당 주간 식단 (date 가 속한 월~일). 아직 올라오지 않은 주는 빈 목록이고, 끼니에 "미운영"이 들어갈 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn get_dorm_meals(&self, Parameters(req): Parameters<DateRequest>) -> ToolResult {
        let date = parse_date(req.date.as_deref())?;
        to_json(&self.meal.dorm_week(date).await.map_err(|e| e.to_string())?)
    }

    /// u-saint 학부 성적 요약(학적부·증명서 기준 평점, 이수 학점)과 학기별 성적
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_grades_summary(&self) -> ToolResult {
        let u = self.usaint().await?;
        let mut app = u.app::<CourseGradesApplication>().await?;
        let recorded = app
            .recorded_summary(CourseType::Bachelor)
            .await
            .map_err(usaint::err)?;
        let certificated = app
            .certificated_summary(CourseType::Bachelor)
            .await
            .map_err(usaint::err)?;
        let semesters = app
            .semesters(CourseType::Bachelor)
            .await
            .map_err(usaint::err)?;
        to_json(
            &json!({ "recorded": recorded, "certificated": certificated, "semesters": semesters }),
        )
    }

    /// u-saint 특정 학기의 과목별 성적
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_semester_grades(
        &self,
        Parameters(req): Parameters<SemesterRequest>,
    ) -> ToolResult {
        let (year, semester) = req.resolve()?;
        let u = self.usaint().await?;
        let mut app = u.app::<CourseGradesApplication>().await?;
        let classes = app
            .classes(CourseType::Bachelor, year, semester, false)
            .await
            .map_err(usaint::err)?;
        to_json(&json!({ "year": year, "semester": semester, "classes": classes }))
    }

    /// u-saint 개인 시간표 (요일·교시·강의실)
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_timetable(&self, Parameters(req): Parameters<SemesterRequest>) -> ToolResult {
        let (year, semester) = req.resolve()?;
        let u = self.usaint().await?;
        let mut app = u.app::<PersonalCourseScheduleApplication>().await?;
        let schedule = app.schedule(year, semester).await.map_err(usaint::err)?;
        to_json(&json!({ "year": year, "semester": semester, "schedule": schedule }))
    }

    /// u-saint 수강신청 내역 (과목, 학점, 교수, 분반)
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_registered_lectures(
        &self,
        Parameters(req): Parameters<SemesterRequest>,
    ) -> ToolResult {
        let (year, semester) = req.resolve()?;
        let u = self.usaint().await?;
        let mut app = u.app::<CourseRegistrationStatusApplication>().await?;
        let lectures: Vec<_> = app
            .lectures(year, semester)
            .await
            .map_err(usaint::err)?
            .collect();
        to_json(&json!({ "year": year, "semester": semester, "lectures": lectures }))
    }

    /// u-saint 졸업사정표: 이수 구분별 기준 학점과 취득 학점, 남은 요건
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_graduation(&self) -> ToolResult {
        let u = self.usaint().await?;
        let mut app = u.app::<GraduationRequirementsApplication>().await?;
        let student = app.student_info().await.map_err(usaint::err)?;
        let requirements = app.requirements().await.map_err(usaint::err)?;
        to_json(&json!({ "student": student, "requirements": requirements }))
    }

    /// u-saint 채플 이수 현황(출석·결석 기록, 이수 여부)
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_chapel(&self, Parameters(req): Parameters<SemesterRequest>) -> ToolResult {
        let (year, semester) = req.resolve()?;
        let u = self.usaint().await?;
        let mut app = u.app::<ChapelApplication>().await?;
        let info = app.information(year, semester).await.map_err(usaint::err)?;
        to_json(&json!({ "year": year, "semester": semester, "chapel": info }))
    }

    /// u-saint 장학금 수혜 내역
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_scholarships(&self) -> ToolResult {
        let u = self.usaint().await?;
        let mut app = u.app::<ScholarshipsApplication>().await?;
        to_json(&app.scholarships().await.map_err(usaint::err)?)
    }

    /// u-saint 학적 기본 정보(학과·전공·학년·학기)와 학적 변동 이력.
    /// 주민번호·주소·연락처·사진, 계좌·가족·종교 정보는 이 서버가 내보내지 않습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_student_info(&self) -> ToolResult {
        let u = self.usaint().await?;
        let mut app = u.app::<StudentInformationApplication>().await?;
        let general = app.general().map_err(usaint::err)?;
        let general =
            usaint::redact_student_info(serde_json::to_value(general).map_err(|e| e.to_string())?);
        let records = app.academic_record().await.map_err(usaint::err)?;
        to_json(&json!({ "general": general, "academic_records": records }))
    }

    /// SSU-PATH 에서 지금 모집 중이고 내 학년이 신청할 수 있는 비교과 프로그램 목록. 안내할 때 각 항목의 url(상세 페이지)도 함께 알려 주세요.
    /// eligibility 는 신청 대상(학교 전체/내 학과)과 신청 신분(학생)으로 판단하며, unknown 은 단과대 단위 대상 등이라
    /// path_program_detail 로 확인해야 합니다. seats_left 가 0 이하여도 대기자 신청이 가능할 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn path_programs(&self, Parameters(req): Parameters<PathProgramsRequest>) -> ToolResult {
        let client = self.path().await?;
        let me = client.profile().await.map_err(|e| e.to_string())?;
        let query = ProgramQuery {
            recruiting_only: true,
            grade: me.grade,
            keyword: req.keyword,
            page: req.page.unwrap_or(1),
        };
        let page = client.programs(&query).await.map_err(|e| e.to_string())?;
        let items: Vec<_> = page
            .items
            .iter()
            .map(|p| (rusaint_path::eligibility(p, &me), p))
            .filter(|(e, _)| req.include_ineligible || *e != Eligibility::No)
            .map(|(e, p)| {
                let mut v = serde_json::to_value(p).unwrap_or_default();
                v["eligibility"] = json!(e);
                v
            })
            .collect();
        to_json(&json!({
            "me": me,
            "page": page.page,
            "last_page": page.last_page,
            "items": items,
        }))
    }

    /// SSU-PATH 비교과 프로그램 상세: 신청 대상 상세(학적·학년·학위), 선정 방법, 정원/대기자, 주요 내용, 일정
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn path_program_detail(
        &self,
        Parameters(req): Parameters<PathDetailRequest>,
    ) -> ToolResult {
        let client = self.path().await?;
        let pairs = client
            .program_detail(&req.program_id)
            .await
            .map_err(|e| e.to_string())?;
        to_json(&json!({
            "url": rusaint_path::program_url(&req.program_id),
            "details": pairs
                .into_iter()
                .map(|(k, v)| json!({ "item": k, "value": v }))
                .collect::<Vec<_>>(),
        }))
    }

    /// LMS 학기 목록과 그 학기의 수강 강의 (교수, 수강 상태).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_courses(&self, Parameters(req): Parameters<TermRequest>) -> ToolResult {
        let client = self.lms().await?;
        let (term, terms) = self.lms_term(&client, req.term_id).await?;
        let courses = client.courses(term.id).await.map_err(|e| e.to_string())?;
        to_json(&json!({
            "term": term,
            "terms": terms.iter().map(|t| json!({ "id": t.id, "name": t.name, "current": t.default })).collect::<Vec<_>>(),
            "courses": courses,
        }))
    }

    /// LMS 할 일: 강의별 미제출 과제·퀴즈·미시청 영상 수와 마감이 있는 항목. due_date 는 UTC 이니 한국 시간(+9)으로 바꿔 알려 주세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_todos(&self, Parameters(req): Parameters<TermRequest>) -> ToolResult {
        let client = self.lms().await?;
        let (term, _) = self.lms_term(&client, req.term_id).await?;
        let (courses, todos) = tokio::join!(client.courses(term.id), client.todos(term.id));
        let courses = courses.map_err(|e| e.to_string())?;
        let todos = todos.map_err(|e| e.to_string())?;
        let out: Vec<_> = todos
            .iter()
            .map(|t| {
                let name = courses.iter().find(|c| c.id == t.course_id).map(|c| c.name.as_str());
                let pending: serde_json::Map<String, serde_json::Value> = t
                    .activities
                    .iter()
                    .filter(|(k, v)| !k.contains("total_announcements") && !k.contains("total_resources") && v.as_u64().is_some_and(|n| n > 0))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                json!({ "course_id": t.course_id, "course": name, "pending_counts": pending, "items": t.todo_list })
            })
            .collect();
        to_json(&json!({ "term": term.name, "courses": out }))
    }

    /// 강의의 주차별 항목(자료·영상·과제·퀴즈)과 완료 여부. 자료는 material.content_id 로 내려받을 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_materials(&self, Parameters(req): Parameters<CourseRequest>) -> ToolResult {
        let client = self.lms().await?;
        let modules = client
            .modules(req.course_id)
            .await
            .map_err(|e| e.to_string())?;
        let weeks: Vec<_> = modules
            .iter()
            .map(|m| {
                let items: Vec<_> = m
                    .module_items
                    .iter()
                    .map(|i| {
                        json!({
                            "title": i.title,
                            "type": i.content_type,
                            "completed": i.completed,
                            "due_at": i.due_at(),
                            "material": i.material(),
                        })
                    })
                    .collect();
                json!({ "week": m.week_position, "title": m.title, "items": items })
            })
            .collect();
        to_json(&weeks)
    }

    /// LMS 강의 공지 목록(제목, 게시 시각, 읽음 여부). 본문은 lms_announcement 로 봅니다.
    /// 이 도구로 읽어도 LMS 의 읽음 표시는 바뀌지 않습니다. posted_at 은 UTC 입니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_announcements(
        &self,
        Parameters(req): Parameters<AnnouncementsRequest>,
    ) -> ToolResult {
        let client = self.lms().await?;
        let courses = match req.course_id {
            Some(id) => vec![(id, None)],
            None => {
                let (term, _) = self.lms_term(&client, None).await?;
                client
                    .courses(term.id)
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|c| (c.id, Some(c.name)))
                    .collect()
            }
        };
        let mut out = Vec::new();
        for (course_id, name) in courses {
            let mut list = client
                .announcements(course_id)
                .await
                .map_err(|e| e.to_string())?;
            if req.unread_only {
                list.retain(|a| a.read_state.as_deref() == Some("unread"));
            }
            out.push(json!({ "course_id": course_id, "course": name, "announcements": list }));
        }
        to_json(&out)
    }

    /// LMS 강의 공지 하나의 본문(텍스트). 읽음 표시는 바뀌지 않습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_announcement(
        &self,
        Parameters(req): Parameters<AnnouncementRequest>,
    ) -> ToolResult {
        let client = self.lms().await?;
        to_json(
            &client
                .announcement(req.course_id, req.announcement_id)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 과목 게시판(Q&A, 팀 활동 게시판 등) 목록과 글 수·안 읽은 글 수
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_boards(&self, Parameters(req): Parameters<CourseRequest>) -> ToolResult {
        let client = self.lms().await?;
        to_json(
            &client
                .boards(req.course_id)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 과목 게시판 글 목록(본문 포함, 텍스트). 첨부파일은 lms_post 로 확인합니다. 다른 학생이 쓴 글이 섞여 있으니 요청받은 범위만 요약하고 다른 곳에 옮기지 마세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_posts(&self, Parameters(req): Parameters<PostsRequest>) -> ToolResult {
        let client = self.lms().await?;
        to_json(
            &client
                .posts(req.course_id, req.board_id, req.page.unwrap_or(1))
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 과목 게시판 글의 댓글
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_comments(&self, Parameters(req): Parameters<CommentsRequest>) -> ToolResult {
        let client = self.lms().await?;
        to_json(
            &client
                .comments(req.course_id, req.board_id, req.post_id)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 과목 게시판 글 하나의 본문과 첨부파일 목록(파일명·크기·id). 조회해도 조회수·읽음 표시는 바뀌지 않습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn lms_post(&self, Parameters(req): Parameters<PostRequest>) -> ToolResult {
        let client = self.lms().await?;
        to_json(
            &client
                .post(req.course_id, req.board_id, req.post_id)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    /// 과목 게시판 첨부파일을 ~/Downloads/ssu-lms/<강의명>/게시판/ (또는 RUSAINT_DOWNLOAD_DIR) 에 내려받습니다.
    /// 같은 이름의 파일이 있으면 덮어쓰지 않습니다. 다른 학생이 올린 파일일 수 있으니 요청받은 파일만 받으세요.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = true
    ))]
    async fn lms_download_post_attachment(
        &self,
        Parameters(req): Parameters<AttachmentRequest>,
    ) -> ToolResult {
        let client = self.lms().await?;
        let post = client
            .post(req.course_id, req.board_id, req.post_id)
            .await
            .map_err(|e| e.to_string())?;
        let attachment = post
            .attachments
            .iter()
            .find(|a| a.id == req.attachment_id)
            .ok_or("이 글에 해당 첨부파일이 없습니다")?;
        let course = self.lms_course_name(&client, req.course_id).await?;
        let dest = download_root()?
            .join(safe_name(&course))
            .join("게시판")
            .join(safe_name(&attachment.filename));
        if dest.exists() {
            return to_json(
                &json!({ "saved": false, "reason": "이미 같은 이름의 파일이 있습니다", "path": dest }),
            );
        }
        let bytes = client
            .download_attachment(attachment, &dest)
            .await
            .map_err(|e| e.to_string())?;
        to_json(&json!({ "saved": true, "path": dest, "bytes": bytes, "post": post.title }))
    }

    /// LMS 자료 파일을 ~/Downloads/ssu-lms/<강의명>/ (또는 RUSAINT_DOWNLOAD_DIR) 에 내려받습니다. 같은 이름의 파일이 있으면 덮어쓰지 않습니다.
    /// lms_materials 의 size_bytes 는 실제 원본 파일보다 훨씬 작게 나올 수 있습니다(실측 0.7MB → 22.7MB). 영상(movie)은 특히 클 수 있으니 미리 알려 주세요.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = true
    ))]
    async fn lms_download_material(
        &self,
        Parameters(req): Parameters<DownloadRequest>,
    ) -> ToolResult {
        let client = self.lms().await?;
        let course = self.lms_course_name(&client, req.course_id).await?;
        let modules = client
            .modules(req.course_id)
            .await
            .map_err(|e| e.to_string())?;
        let (item, material) = modules
            .iter()
            .flat_map(|m| &m.module_items)
            .find_map(|i| {
                i.material()
                    .filter(|m| m.content_id == req.content_id)
                    .map(|m| (i, m))
            })
            .ok_or("이 강의에서 해당 자료를 찾을 수 없습니다")?;
        let file_name = material
            .file_name
            .clone()
            .unwrap_or_else(|| format!("{}.{}", item.title, material.kind));
        let dir = download_root()?.join(safe_name(&course));
        let dest = dir.join(safe_name(&file_name));
        if dest.exists() {
            return to_json(
                &json!({ "saved": false, "reason": "이미 같은 이름의 파일이 있습니다", "path": dest }),
            );
        }
        let bytes = client
            .download(&material.content_id, &dest)
            .await
            .map_err(|e| e.to_string())?;
        to_json(&json!({ "saved": true, "path": dest, "bytes": bytes }))
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

/// 내려받은 LMS 자료를 두는 곳. `RUSAINT_DOWNLOAD_DIR` 이 있으면 그 경로를 씁니다.
fn download_root() -> Result<std::path::PathBuf, String> {
    if let Some(dir) = std::env::var_os("RUSAINT_DOWNLOAD_DIR").filter(|d| !d.is_empty()) {
        return Ok(std::path::PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or("HOME 이 설정되어 있지 않습니다")?;
    Ok(std::path::PathBuf::from(home)
        .join("Downloads")
        .join("ssu-lms"))
}

/// 경로 구분자·제어 문자를 지우고 숨김 파일·상위 경로가 되지 않게 합니다.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "untitled".into()
    } else {
        cleaned
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

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn safe_name_blocks_paths() {
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("HCI (2150469601)"), "HCI (2150469601)");
        assert_eq!(safe_name("a/b:c.pdf"), "a_b_c.pdf");
        assert_eq!(safe_name(".."), "untitled");
        assert_eq!(safe_name(".hidden"), "hidden");
    }
}
