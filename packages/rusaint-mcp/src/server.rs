use std::sync::Arc;

use chrono::{Local, NaiveDate, NaiveTime, Timelike};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use rusaint::application::chapel::ChapelApplication;
use rusaint::application::course_grades::CourseGradesApplication;
use rusaint::application::course_grades::model::CourseType;
use rusaint::application::course_registration_status::CourseRegistrationStatusApplication;
use rusaint::application::course_schedule::CourseScheduleApplication;
use rusaint::application::curriculum::CurriculumApplication;
use rusaint::application::graduation_requirements::GraduationRequirementsApplication;
use rusaint::application::personal_course_schedule::PersonalCourseScheduleApplication;
use rusaint::application::scholarships::ScholarshipsApplication;
use rusaint::application::student_information::StudentInformationApplication;
use rusaint::model::SemesterType;
use rusaint_calendar::CalendarClient;
use rusaint_library::{LibraryClient, RoomReservation};
use rusaint_lms::{LmsClient, Term};
use rusaint_meal::{Cafeteria, MealClient};
use rusaint_path::{Eligibility, PathClient, ProgramQuery};
use rusaint_regulation::RegulationClient;
use rusaint_sso::SsoCredentials;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OnceCell;

use crate::automation::{Automation, MAX_RENEW_HOURS};
use crate::courses::{self, Kind};
use crate::keychain;
use crate::pending::{Action, Pending};
use crate::usaint::{self, Usaint};

const INSTRUCTIONS: &str = "숭실대학교 u-saint(성적·시간표·졸업사정·채플·장학금·학적), 학식 메뉴, LMS(강의·과제·자료·공지·게시판), SSU-PATH(비교과 프로그램), 중앙도서관(열람실 좌석, 스터디룸, 대출)을 다루는 비공식 도구입니다. 학교 사이트가 바뀌면 틀리거나 실패할 수 있으니, 중요한 결정 전에는 공식 홈페이지 확인을 권하세요.
- 날짜는 YYYY-MM-DD, 시각은 HH:MM(한국 시간)입니다. 생략하면 오늘입니다.
- 도서관 쓰기 작업은 prepare_* 의 summary 를 사용자에게 그대로 보여 주고, 그 작업에 대한 사용자의 명시적 동의를 받은 뒤에만 confirm_library_action 을 호출합니다. 동의는 작업 하나에만 유효하며, 내용이 바뀌면 다시 prepare 합니다.
- 로그인은 login 도구로 안내합니다. 비밀번호를 대화나 도구 인자로 받지 마세요. 계정이 거부되면 반복 시도하지 말고 로컬에서는 `rusaint-mcp login` 후 앱을 재시작하고, 원격에서는 커넥터를 다시 연결하도록 안내하세요.
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
pub struct CalendarRequest {
    /// 시작 날짜 (YYYY-MM-DD). 생략하면 오늘
    pub from: Option<String>,
    /// 끝 날짜 (YYYY-MM-DD). 생략하면 시작일부터 60일 뒤
    pub to: Option<String>,
    /// 일정 이름에 들어 있어야 할 단어 (예: 수강신청, 중간고사, 휴학)
    pub keyword: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegulationRequest {
    /// 학칙, 학칙시행세칙, 학사관리내규, 연계융합전공, 교육과정규정, 장학규정, 장학금지급내규, 국가장학금II내규.
    /// 생략하면 학칙시행세칙(졸업·수강·성적·전공 인정 등 학사 세부 규정). 장학금 종류·성적 기준은 장학금지급내규에 있습니다
    pub regulation: Option<String>,
    /// 조문 제목·본문에 모두 들어 있어야 할 단어들(공백 구분). 예: "졸업 학점", "타 학과 인정", "복수전공"
    pub keyword: Option<String>,
    /// 조 번호. 예: 제20조, 제87조의3
    pub article: Option<String>,
    /// 별표 번호나 이름 일부. 예: 별표1, 별표6. 주면 그 별표 PDF 의 글자를 돌려줍니다
    pub attachment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CourseSearchRequest {
    /// 연도. 생략하면 현재 학기
    pub year: Option<u32>,
    /// 1학기, 2학기, 여름학기, 겨울학기. 생략하면 현재 학기
    pub semester: Option<String>,
    /// lecture(과목명 검색), professor(교수명 검색), major(전공), required_elective(교양필수),
    /// optional_elective(교양선택), chapel(채플), connected_major(연계전공), united_major(융합전공), education(교직),
    /// recognized_other_major(내 전공에서 전공 학점으로 인정하는 타전공 과목. collage·department 생략하면 내 소속)
    pub kind: String,
    /// lecture/professor 는 검색어, required_elective·chapel 은 과목명, optional_elective 는 교양 분야,
    /// connected_major·united_major 는 전공명. 이름은 usaint_course_options 의 값과 정확히 같아야 합니다
    pub keyword: Option<String>,
    /// major 일 때 단과대·학부·전공. 생략하면 내 소속
    pub collage: Option<String>,
    pub department: Option<String>,
    pub major: Option<String>,
    /// 결과에서 다시 거를 단어들(공백 구분, 모두 포함). 과목명·교수·강의시간·이수구분·과목번호에서 찾습니다. 예: "월 전선"
    pub filter: Option<String>,
    /// 최대 개수 (기본 40)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CourseOptionsRequest {
    pub year: Option<u32>,
    pub semester: Option<String>,
    /// 주면 그 단과대의 학부 목록도 돌려줍니다
    pub collage: Option<String>,
    /// collage 와 함께 주면 그 학부의 전공 목록도 돌려줍니다
    pub department: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CourseInfoRequest {
    /// 과목명들 (최대 8개). 예: ["HCI", "데이터베이스"]
    pub names: Vec<String>,
    pub year: Option<u32>,
    pub semester: Option<String>,
    /// 과목마다 살펴볼 분반 수 (기본 3, 최대 8). 분반마다 강의계획서를 따로 읽어 느립니다
    pub max_sections: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CurriculumRequest {
    /// 입학년도 (2016 이후). 생략하면 내 입학년도. 교과과정은 입학년도 기준으로 적용됩니다
    pub entry_year: Option<u32>,
    /// 다른 학과를 보려면 단과대학·학과·전공 이름(usaint_course_options 의 이름과 같음). 생략하면 내 소속
    pub collage: Option<String>,
    pub department: Option<String>,
    pub major: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraduationPlanRequest {
    /// 추천할 학기 연도. 생략하면 다음 정규 학기
    pub year: Option<u32>,
    /// 1학기 또는 2학기. 생략하면 다음 정규 학기
    pub semester: Option<String>,
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
    calendar: Arc<CalendarClient>,
    regulation: Arc<RegulationClient>,
    library: Arc<OnceCell<Arc<LibraryClient>>>,
    lms: Arc<OnceCell<Arc<LmsClient>>>,
    usaint: Arc<OnceCell<Arc<Usaint>>>,
    path: Arc<OnceCell<Arc<PathClient>>>,
    /// u-saint·LMS·SSU-PATH 가 함께 쓰는 통합 계정. 거부되면 세 서비스 모두 다시 시도하지 않습니다.
    sso: Arc<OnceCell<Arc<SsoCredentials>>>,
    pending: Arc<Pending>,
    automation: Arc<Automation>,
    tool_router: ToolRouter<Self>,
    remote: bool,
}

impl SsuServer {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            meal: Arc::new(MealClient::new().map_err(|e| e.to_string())?),
            calendar: Arc::new(CalendarClient::new().map_err(|e| e.to_string())?),
            regulation: Arc::new(RegulationClient::new().map_err(|e| e.to_string())?),
            library: Arc::new(OnceCell::new()),
            lms: Arc::new(OnceCell::new()),
            usaint: Arc::new(OnceCell::new()),
            path: Arc::new(OnceCell::new()),
            sso: Arc::new(OnceCell::new()),
            pending: Arc::new(Pending::new()),
            automation: Arc::new(Automation::default()),
            tool_router: Self::tool_router(),
            remote: false,
        })
    }

    /// 원격 연결은 검증된 계정만 사용하며 호스트의 Keychain 을 읽지 않습니다.
    pub fn authenticated(
        sso: Arc<SsoCredentials>,
        library: Option<Arc<LibraryClient>>,
    ) -> Result<Self, String> {
        let mut server = Self::new()?;
        server.remote = true;
        let _ = server.sso.set(sso);
        if let Some(library) = library {
            let _ = server.library.set(library);
        }
        Ok(server)
    }

    /// 도서관 클라이언트는 처음 필요할 때 Keychain 에서 계정을 읽어 만듭니다.
    async fn library(&self) -> Result<Arc<LibraryClient>, String> {
        if self.remote && !self.library.initialized() {
            return Err("도서관 계정이 연결되지 않았습니다. 커넥터를 다시 연결하고 도서관 계정도 입력하세요.".into());
        }
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
    /// login: 로그인 방법을 안내합니다. 비밀번호는 채팅에 입력하지 말고 터미널 또는 커넥터 연결 화면에서 입력하세요.
    #[tool(annotations(read_only_hint = true))]
    async fn login(&self) -> ToolResult {
        Ok(if self.remote {
            "현재 커넥터 연결 시 로그인한 계정을 사용 중입니다. 계정을 바꾸거나 도서관 계정을 추가하려면 Claude 설정에서 커넥터를 다시 연결해 브라우저 로그인 화면을 여세요."
        } else {
            "터미널에서 `rusaint-mcp login`을 실행하세요. 통합 계정만은 `rusaint-mcp login sso`, 도서관만은 `rusaint-mcp login library`, 저장 상태는 `rusaint-mcp login status`입니다. 저장 후 MCP를 사용하는 앱을 다시 시작하세요."
        }.into())
    }
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

    /// 성적이 나온 과목(이수구분별 성적표)과 이번 학기 수강 중인 과목. 과목번호로 대조합니다.
    async fn taken_courses(&self, u: &Usaint) -> Result<courses::Taken, String> {
        let mut taken = courses::Taken::default();
        let mut grades = u.app::<CourseGradesApplication>().await?;
        let record = grades
            .grades_by_classification(CourseType::Bachelor)
            .await
            .map_err(usaint::err)?;
        for item in record.grades() {
            taken.add(item.course_code(), item.course_name());
        }
        let (year, semester) = usaint::current_semester();
        let mut status = u.app::<CourseRegistrationStatusApplication>().await?;
        if let Ok(lectures) = status.lectures(year, semester).await {
            for l in lectures {
                taken.add(&l.code, &l.name);
            }
        }
        Ok(taken)
    }

    /// 내 소속(단과대·학부·전공)으로 전공 검색 조건을 만듭니다.
    async fn my_major(&self, u: &Usaint) -> Result<Kind, String> {
        let info = u.app::<StudentInformationApplication>().await?;
        let general = info.general().map_err(usaint::err)?;
        Ok(Kind::Major {
            collage: general.collage().to_string(),
            department: general.department().to_string(),
            major: general.major().map(str::to_string),
        })
    }

    /// u-saint 강의시간표 검색. 과목명·교수명 검색, 전공(생략하면 내 소속), 교양필수·교양선택·채플 등으로 찾습니다.
    /// 교양필수 과목명·교양선택 분야·단과대/학부 이름은 usaint_course_options 로 먼저 확인하세요.
    /// 다음 학기 시간표는 보통 수강신청 몇 주 전에 올라옵니다. 결과가 비면 아직 공개 전일 수 있습니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_course_search(
        &self,
        Parameters(req): Parameters<CourseSearchRequest>,
    ) -> ToolResult {
        let (year, semester) = SemesterRequest {
            year: req.year,
            semester: req.semester.clone(),
        }
        .resolve()?;
        let u = self.usaint().await?;
        let keyword = || {
            req.keyword
                .clone()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| format!("kind={} 은 keyword 가 필요합니다", req.kind))
        };
        let kind = match req.kind.as_str() {
            "lecture" => Kind::Lecture(keyword()?),
            "professor" => Kind::Professor(keyword()?),
            "required_elective" => Kind::RequiredElective(keyword()?),
            "optional_elective" => Kind::OptionalElective(keyword()?),
            "chapel" => Kind::Chapel(keyword()?),
            "connected_major" => Kind::ConnectedMajor(keyword()?),
            "united_major" => Kind::UnitedMajor(keyword()?),
            "education" => Kind::Education,
            "recognized_other_major" => match (&req.collage, &req.department) {
                (Some(collage), Some(department)) => Kind::RecognizedOtherMajor {
                    collage: collage.clone(),
                    department: department.clone(),
                    major: req.major.clone(),
                },
                _ => match self.my_major(&u).await? {
                    Kind::Major {
                        collage,
                        department,
                        major,
                    } => Kind::RecognizedOtherMajor {
                        collage,
                        department,
                        major,
                    },
                    other => other,
                },
            },
            "major" => match (&req.collage, &req.department) {
                (Some(collage), Some(department)) => Kind::Major {
                    collage: collage.clone(),
                    department: department.clone(),
                    major: req.major.clone(),
                },
                _ => self.my_major(&u).await?,
            },
            other => return Err(format!("알 수 없는 kind: {other}")),
        };
        let mut app = u.app::<CourseScheduleApplication>().await?;
        let mut found = courses::search(&mut app, year, semester, &kind).await?;
        if let Some(words) = req.filter.as_deref() {
            found = courses::filter(found, words);
        }
        let total = found.len();
        found.truncate(req.limit.unwrap_or(40).clamp(1, 200));
        to_json(&json!({
            "year": year,
            "semester": semester,
            "total": total,
            "returned": found.len(),
            "courses": found,
        }))
    }

    /// 강의 검색에 쓸 이름 목록: 단과대, (collage 를 주면) 학부, (department 도 주면) 전공, 교양필수 과목명, 교양선택 분야
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_course_options(
        &self,
        Parameters(req): Parameters<CourseOptionsRequest>,
    ) -> ToolResult {
        let (year, semester) = SemesterRequest {
            year: req.year,
            semester: req.semester.clone(),
        }
        .resolve()?;
        let u = self.usaint().await?;
        let mut app = u.app::<CourseScheduleApplication>().await?;
        let e = usaint::err;
        let collages = app.collages(year, semester).await.map_err(e)?;
        let departments = match &req.collage {
            Some(c) => Some(app.departments(year, semester, c).await.map_err(e)?),
            None => None,
        };
        let majors = match (&req.collage, &req.department) {
            (Some(c), Some(d)) => Some(app.majors(year, semester, c, d).await.map_err(e)?),
            _ => None,
        };
        let required_electives = app.required_electives(year, semester).await.map_err(e)?;
        let optional_elective_areas = app
            .optional_elective_categories(year, semester)
            .await
            .map_err(e)?;
        to_json(&json!({
            "year": year,
            "semester": semester,
            "collages": collages,
            "departments": departments,
            "majors": majors,
            "required_electives": required_electives,
            "optional_elective_areas": optional_elective_areas,
        }))
    }

    /// 과목별 여석·P/F·평가 방식·내 전공 인정 여부. 장바구니에 담을 과목을 확인할 때 씁니다.
    /// pass_fail 은 강의계획서의 성적 입력 방식("Pass/Fail 입력")으로 판단하며, 계획서가 없으면 null 입니다.
    /// counts_for_my_major 는 강의 상세의 전공별 이수구분에 내 주전공의 전기/전필/전선이 있거나, 그 학기 타전공인정과목 목록에 있으면 true 입니다.
    /// recognized_other_major=true 면 타 학과 과목이 내 전공으로 인정되는 경우이니 소속 학과 최소 이수학점(별표6)과 복수전공 여부(제20조)를 함께 안내하세요.
    /// 없으면 교양(교필/교선) 또는 일반선택으로 들어갑니다. 여석은 조회 시점 값입니다.
    /// syllabus_url 은 브라우저에서 강의계획서를 여는 학교 OZ 뷰어 주소입니다. 로그인 없이 열리지만 학교 서버가 요구해서 학번이 들어 있으니,
    /// 본인에게만 보여 주고 다른 사람과 공유할 때는 주의하라고 알려 주세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_course_info(
        &self,
        Parameters(req): Parameters<CourseInfoRequest>,
    ) -> ToolResult {
        if req.names.is_empty() || req.names.len() > 8 {
            return Err("names 는 1~8개여야 합니다".into());
        }
        let (year, semester) = SemesterRequest {
            year: req.year,
            semester: req.semester.clone(),
        }
        .resolve()?;
        let u = self.usaint().await?;
        let Kind::Major {
            collage,
            department,
            major,
        } = self.my_major(&u).await?
        else {
            unreachable!("my_major 는 전공 검색 조건을 돌려줍니다")
        };
        let keyword = courses::major_keyword(&department, major.as_deref());
        let max_sections = req.max_sections.unwrap_or(3).clamp(1, 8);
        let mut app = u.app::<CourseScheduleApplication>().await?;
        // 타 학과 과목은 강의 상세에 내 전공 이수구분이 없어도 타전공인정 목록에 있으면 인정됩니다.
        let recognized: std::collections::HashMap<String, String> = courses::search(
            &mut app,
            year,
            semester,
            &Kind::RecognizedOtherMajor {
                collage,
                department,
                major,
            },
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c.code, c.category))
        .collect();
        let mut results = Vec::new();
        for name in &req.names {
            let name = name.trim();
            // 그 학기에 없는 과목은 u-saint 가 오류로 알려 주므로 빈 결과로 둡니다.
            let found = courses::search(&mut app, year, semester, &Kind::Lecture(name.to_string()))
                .await
                .unwrap_or_default();
            // 같은 이름이 있으면 그것만, 없으면 이름에 검색어가 들어간 것 전부
            let exact: Vec<_> = found.iter().filter(|c| c.name.trim() == name).collect();
            let picked = if exact.is_empty() {
                found.iter().collect()
            } else {
                exact
            };
            let total = picked.len();
            let mut sections = Vec::new();
            for c in picked.into_iter().take(max_sections) {
                let categories = app
                    .lecture_detail(&c.code)
                    .await
                    .map(|d| d.categories)
                    .unwrap_or_default();
                let syllabus = app.lecture_syllabus(&c.code).await.ok();
                let mine = courses::my_major_category(&categories, &keyword).map(str::to_string);
                let other = recognized.get(&c.code);
                sections.push(json!({
                    "code": c.code,
                    "name": c.name,
                    "division": c.division,
                    "professor": c.professor,
                    "schedule": c.schedule,
                    "credits": c.credits,
                    "hours": c.hours,
                    "enrolled": c.enrolled,
                    "seats_left": c.seats_left,
                    "target": c.target,
                    "categories": categories,
                    "counts_for_my_major": mine.is_some() || other.is_some(),
                    "my_major_category": mine.clone().or_else(|| other.cloned()),
                    "recognized_other_major": mine.is_none() && other.is_some(),
                    "pass_fail": syllabus.as_ref().map(|s| s.grading_scale.contains("Pass")),
                    "grading_scale": syllabus.as_ref().map(|s| s.grading_scale.clone()),
                    "evaluation_type": syllabus.as_ref().map(|s| s.evaluation_type.clone()),
                    "syllabus_url": syllabus.as_ref().and_then(|s| s.viewer_url.clone()),
                }));
            }
            results.push(json!({"query": name, "sections_found": total, "sections": sections}));
        }
        to_json(
            &json!({"year": year, "semester": semester, "my_major": keyword, "courses": results}),
        )
    }

    /// 공식 교과과정표(u-saint "입학년도 별 교과과정 조회"): 입학년도·학과·전공 기준으로 학년·학기별 과목, 이수구분(전공기초/전공필수/전공선택),
    /// 학점·시간, 과목 개요를 돌려줍니다. 교과과정은 입학년도 기준으로 적용되며 개편되면 바뀔 수 있습니다(실시간 반영이 안 될 수 있음).
    /// taken 은 졸업사정표에 쓰인(이미 들었거나 듣는 중인) 과목입니다. 실제 개설 여부·시간은 usaint_course_search 로 확인하세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_major_curriculum(
        &self,
        Parameters(req): Parameters<CurriculumRequest>,
    ) -> ToolResult {
        let u = self.usaint().await?;
        let (my_year, my_collage, my_department, my_major) = {
            let info = u.app::<StudentInformationApplication>().await?;
            let g = info.general().map_err(usaint::err)?;
            (
                g.apply_year(),
                g.collage().to_string(),
                g.department().to_string(),
                g.major().map(str::to_string),
            )
        };
        let year = req.entry_year.unwrap_or(my_year);
        let collage = req.collage.clone().unwrap_or(my_collage);
        let department = req.department.clone().unwrap_or(my_department);
        let taken = self.taken_courses(&u).await?;
        let mut app = u.app::<CurriculumApplication>().await?;
        // 전공을 따로 고르지 않는 학과도 목록에 전공이 하나 있으면 그것을 골라야 조회됩니다.
        let major = match req.major.clone().or(my_major) {
            Some(m) => Some(m),
            None => {
                let majors = app
                    .majors(year, &collage, &department)
                    .await
                    .map_err(usaint::err)?;
                match majors.as_slice() {
                    [] => None,
                    [only] => Some(only.clone()),
                    many => {
                        return Err(format!(
                            "{department} 의 전공을 골라 주세요: {}",
                            many.join(", ")
                        ));
                    }
                }
            }
        };
        let rows = app
            .curriculum(year, &collage, &department, major.as_deref())
            .await
            .map_err(usaint::err)?;
        let courses: Vec<_> = rows
            .iter()
            .map(|c| {
                let taken = taken.contains(&c.code, &c.name);
                json!({
                    "grade": c.grade,
                    "semester": c.semester,
                    "code": c.code,
                    "name": c.name,
                    "category": c.category,
                    "credits": c.credits.trim().parse::<f32>().ok(),
                    "hours": c.hours.trim().parse::<f32>().ok(),
                    "taken": taken,
                    "description": c.description,
                })
            })
            .collect();
        to_json(&json!({
            "entry_year": year,
            "collage": collage,
            "department": department,
            "major": major,
            "total": courses.len(),
            "courses": courses,
        }))
    }

    /// 졸업사정표에서 아직 못 채운 요건과, 그 요건을 채울 수 있는 강의 후보.
    /// 이미 들은 과목(졸업사정표에 쓰인 과목)은 빼고, 전공은 내 소속 학부 강의에서 이수구분(전기/전필/전선)으로 고릅니다.
    /// 추천 학기 시간표가 아직 없으면 1년 전 같은 학기 시간표로 대신하며 offered_basis 에 표시합니다. 이때는 실제 개설 여부가 다를 수 있다고 알려 주세요.
    /// recognized_other_major 는 내 전공 학점으로 인정되는 타 학과 과목인데, 소속 학과 개설 전공을 최소 학점 이상 들어야 하고(학칙시행세칙 별표6,
    /// school_regulation attachment=별표6) 복수전공 중이면 인정되지 않습니다(제20조).
    /// 기독교 과목 후보는 과목명으로 추정한 것이고, 졸업논문·졸업확정신고 같은 요건은 학과 안내를 따르도록 안내하세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn usaint_graduation_plan(
        &self,
        Parameters(req): Parameters<GraduationPlanRequest>,
    ) -> ToolResult {
        let (cy, cs) = usaint::current_semester();
        let (ny, ns) = courses::next_regular(cy, cs);
        let year = req.year.unwrap_or(ny);
        let semester = match req.semester.as_deref() {
            Some(s) => usaint::parse_semester(s)?,
            None => ns,
        };
        let u = self.usaint().await?;
        let mut grad = u.app::<GraduationRequirementsApplication>().await?;
        let requirements = grad.requirements().await.map_err(usaint::err)?;
        let unmet: Vec<_> = requirements
            .requirements()
            .values()
            .filter(|r| !r.result())
            .collect();
        let need = |category: &str| unmet.iter().any(|r| r.category() == category);
        let taken = courses::taken(&requirements);

        let kind = self.my_major(&u).await?;
        let mut app = u.app::<CourseScheduleApplication>().await?;
        let mut basis = (year, semester);
        let mut majors = courses::search(&mut app, year, semester, &kind)
            .await
            .unwrap_or_default();
        if majors.is_empty() {
            basis = (year - 1, semester);
            majors = courses::search(&mut app, basis.0, basis.1, &kind).await?;
        }
        // 시간이 정해진 정규 강의를 앞에, 현장실습·Co-op 처럼 시간이 없는 과목을 뒤에 둡니다.
        majors.sort_by_key(|c| c.schedule.trim().is_empty());
        // 같은 과목의 분반을 하나로 묶고, 이미 들은 과목은 뺍니다.
        let group = |list: &[courses::Course], keep: &dyn Fn(&courses::Course) -> bool| -> Vec<_> {
            let mut seen = std::collections::BTreeSet::new();
            list.iter()
                .filter(|c| keep(c))
                .filter(|c| !courses::is_taken(&taken, &c.name))
                .filter(|c| seen.insert(c.name.clone()))
                .map(|c| {
                    let sections: Vec<_> = list
                        .iter()
                        .filter(|o| o.name == c.name)
                        .map(|o| json!({"code": o.code, "division": o.division, "professor": o.professor, "schedule": o.schedule, "seats_left": o.seats_left}))
                        .collect();
                    json!({"name": c.name, "category": c.category, "department": c.department, "credits": c.credits, "hours": c.hours, "target": c.target, "sections": sections})
                })
                .collect()
        };
        let pick = |category: &str| {
            group(&majors, &|c| {
                courses::matches_major_kind(category, &c.category)
            })
        };
        let major_basic = need("전공기초").then(|| pick("전공기초"));
        let major = need("전공").then(|| pick("전공"));
        let recognized = if need("전공") {
            let Kind::Major {
                collage,
                department,
                major,
            } = &kind
            else {
                unreachable!("my_major 는 전공 검색 조건을 돌려줍니다")
            };
            let other = Kind::RecognizedOtherMajor {
                collage: collage.clone(),
                department: department.clone(),
                major: major.clone(),
            };
            let mut list = courses::search(&mut app, basis.0, basis.1, &other)
                .await
                .unwrap_or_default();
            list.sort_by_key(|c| c.schedule.trim().is_empty());
            Some(group(&list, &|_| true))
        } else {
            None
        };

        let required_names = if need("교양필수") || need("졸업필수 요건") {
            app.required_electives(basis.0, basis.1)
                .await
                .map_err(usaint::err)?
        } else {
            Vec::new()
        };
        // 교양필수는 영역마다 하나씩 들으므로, 이미 들은 과목이 있는 영역은 done 으로 표시합니다.
        let mut areas: std::collections::BTreeMap<&str, (bool, Vec<&str>)> = Default::default();
        for name in required_names
            .iter()
            .filter(|n| !courses::is_for_international(n))
        {
            let (area, _) = courses::split_area(name);
            let entry = areas.entry(area.unwrap_or("기타")).or_default();
            if courses::is_taken(&taken, name) {
                entry.0 = true;
            } else {
                entry.1.push(name);
            }
        }
        let required_areas: Vec<_> = areas
            .iter()
            .map(|(area, (done, left))| json!({"area": area, "done": done, "not_taken": left}))
            .collect();
        let christian: Vec<_> = areas
            .values()
            .flat_map(|(_, left)| left)
            .filter(|n| ["기독", "성서", "성경"].iter().any(|k| n.contains(k)))
            .collect();
        let optional_areas = if need("교양선택") {
            Some(
                app.optional_elective_categories(basis.0, basis.1)
                    .await
                    .map_err(usaint::err)?,
            )
        } else {
            None
        };

        let mut unmet_json: Vec<_> = unmet
            .iter()
            .map(|r| {
                json!({
                    "name": r.name().trim(),
                    "category": r.category(),
                    "required": r.requirement(),
                    "earned": r.calculation(),
                    "short_by": r.difference().map(|d| -d),
                })
            })
            .collect();
        unmet_json.sort_by(|a, b| a["category"].as_str().cmp(&b["category"].as_str()));
        to_json(&json!({
            "target_semester": {"year": year, "semester": semester},
            "offered_basis": {
                "year": basis.0,
                "semester": basis.1,
                "is_previous_year": basis != (year, semester),
            },
            "is_graduatable": requirements.is_graduatable(),
            "unmet": unmet_json,
            "candidates": {
                "major_basic": major_basic,
                "major": major,
                "recognized_other_major": recognized,
                "required_elective_areas": need("교양필수").then_some(&required_areas),
                "christian_course_guess": (!christian.is_empty()).then_some(&christian),
                "optional_elective_areas": optional_areas,
            },
        }))
    }

    /// 숭실대학교 규정집(rule.ssu.ac.kr)의 학사·장학 규정. 졸업 요건, 이수구분별 학점, 타 학과 과목 전공 인정(학칙시행세칙 제20조), 복수·부전공, 수강·성적, 장학금 종류와 지급 기준을 찾을 때 씁니다.
    /// keyword·article 이 없으면 조문 제목 목록과 별표 목록을 돌려줍니다. 별표 표(학과별 학점 등)는 attachment 로 PDF 글자를 받는데,
    /// 표가 줄글로 풀려 병합된 칸의 숫자 순서가 섞일 수 있으니 중요한 숫자는 preview_url 원문을 확인하라고 안내하세요.
    /// 학생 본인의 졸업 기준 학점은 usaint_graduation 이 가장 정확합니다. 규정은 개정될 수 있으니 조문과 함께 url 을 알려 주세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn school_regulation(
        &self,
        Parameters(req): Parameters<RegulationRequest>,
    ) -> ToolResult {
        let wanted = req.regulation.as_deref().unwrap_or("학칙시행세칙").trim();
        let Some(&(id, _)) = rusaint_regulation::REGULATIONS
            .iter()
            .find(|(_, name)| *name == wanted)
        else {
            let names: Vec<_> = rusaint_regulation::REGULATIONS
                .iter()
                .map(|(_, n)| *n)
                .collect();
            return Err(format!("알 수 없는 규정: {wanted} ({})", names.join(", ")));
        };
        let regulation = self
            .regulation
            .regulation(id)
            .await
            .map_err(|e| e.to_string())?;
        let base = json!({"regulation": regulation.name, "url": regulation.url});

        if let Some(wanted) = req.attachment.as_deref().filter(|a| !a.trim().is_empty()) {
            // "별표1" 이 "별표10" 에 걸리지 않도록 "[별표1]"·"(별표 1)" 처럼 괄호까지 비교합니다.
            let key = wanted.trim().replace(' ', "");
            let attachment = regulation
                .attachments
                .iter()
                .find(|a| {
                    let name = a.name.replace(' ', "");
                    name.starts_with(&format!("[{key}]")) || name.starts_with(&format!("({key})"))
                })
                .or_else(|| {
                    regulation
                        .attachments
                        .iter()
                        .find(|a| a.name.contains(wanted.trim()))
                })
                .ok_or_else(|| format!("별표를 찾지 못했습니다: {wanted}"))?;
            let text = self
                .regulation
                .attachment_text(attachment)
                .await
                .map_err(|e| e.to_string())?;
            return to_json(&json!({
                "regulation": regulation.name,
                "attachment": attachment.name,
                "preview_url": attachment.preview_url,
                "text": text,
            }));
        }

        if let Some(number) = req.article.as_deref().filter(|a| !a.trim().is_empty()) {
            let number = number.replace(' ', "");
            let found: Vec<_> = regulation
                .articles
                .iter()
                .filter(|a| a.number().replace(' ', "") == number)
                .collect();
            return to_json(&json!({"regulation": base, "articles": found}));
        }

        if let Some(words) = req.keyword.as_deref().filter(|k| !k.trim().is_empty()) {
            let words: Vec<&str> = words.split_whitespace().collect();
            let found: Vec<_> = regulation
                .articles
                .iter()
                .filter(|a| {
                    let hay = format!("{} {}", a.title, a.body.join(" "));
                    words.iter().all(|w| hay.contains(w))
                })
                .collect();
            let total = found.len();
            let found: Vec<_> = found.into_iter().take(15).collect();
            return to_json(&json!({"regulation": base, "total": total, "articles": found}));
        }

        let titles: Vec<_> = regulation
            .articles
            .iter()
            .map(|a| json!({"title": a.title, "chapter": a.chapter}))
            .collect();
        to_json(
            &json!({"regulation": base, "articles": titles, "attachments": regulation.attachments}),
        )
    }

    /// 숭실대학교 학사일정(수강신청, 개강, 시험, 휴학·복학 신청, 성적 공개 등). 로그인 없이 학교 홈페이지에서 가져옵니다.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn academic_calendar(&self, Parameters(req): Parameters<CalendarRequest>) -> ToolResult {
        let from = parse_date(req.from.as_deref())?;
        let to = match req.to.as_deref() {
            Some(t) if !t.is_empty() => parse_date(Some(t))?,
            _ => from + chrono::Duration::days(60),
        };
        if to < from || (to - from).num_days() > 731 {
            return Err("기간은 시작일 이후, 2년 이내여야 합니다".into());
        }
        let mut events = self
            .calendar
            .between(from, to)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(k) = req.keyword.as_deref().filter(|k| !k.trim().is_empty()) {
            events.retain(|e| e.title.contains(k.trim()));
        }
        to_json(&json!({ "from": from, "to": to, "events": events }))
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

    /// SSU-PATH 비교과 마일리지: 올해 누적(this_year), 전체 획득·사용·잔여, 항목별 내역.
    /// 마일리지 장학금(2026학년도부터, SSU-PATH 공지 기준): 그 해 1~12월 누적 마일리지 순위로 신청 없이 연 1회 지급하며, 매 학년도 0점에서 시작합니다.
    /// 1등 100만원(1명), 2~11등 70만원, 12~41등 50만원, 42~71등 30만원, 72~121등 20만원, 122~221등 15만원(총 221명).
    /// 동점이면 비교과 참여 수가 많은 쪽이 우선이고 구간을 넘는 동점자가 있으면 금액이 일부 낮아질 수 있습니다. 0학점 등록자는 제외되고 u-saint 계좌 등록이 필요합니다.
    /// 내 순위는 SSU-PATH 가 공개하지 않으므로 추정하지 마세요. 금액·기준은 바뀔 수 있으니 notice_url 공지를 함께 알려 주세요.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    async fn path_mileage(&self) -> ToolResult {
        let client = self.path().await?;
        let mileage = client.mileage().await.map_err(|e| e.to_string())?;
        to_json(&json!({
            "mileage": mileage,
            "notice_url": "https://path.ssu.ac.kr/ptfol/comm/board/7d5781d33124f58f831fe856c5c86e53/view.do?dataSeq=1001629",
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
        if self.remote {
            return Err("원격 커넥터에서는 서버 파일시스템에 자료를 다운로드할 수 없습니다. LMS에서 직접 다운로드하세요.".into());
        }
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
        if self.remote {
            return Err("원격 커넥터에서는 서버 파일시스템에 자료를 다운로드할 수 없습니다. LMS에서 직접 다운로드하세요.".into());
        }
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
    use super::{SsuServer, safe_name};

    #[tokio::test]
    async fn remote_accounts_are_isolated_and_never_fall_back_to_keychain() {
        let first =
            SsuServer::authenticated(rusaint_sso::SsoCredentials::new("first", "unused"), None)
                .unwrap();
        let second =
            SsuServer::authenticated(rusaint_sso::SsoCredentials::new("second", "unused"), None)
                .unwrap();
        assert_eq!(first.sso().await.unwrap().student_id(), "first");
        assert_eq!(second.sso().await.unwrap().student_id(), "second");
        assert!(first.library().await.is_err());
        assert!(second.library().await.is_err());
        assert!(!std::sync::Arc::ptr_eq(&first.pending, &second.pending));
        assert!(!std::sync::Arc::ptr_eq(
            &first.automation,
            &second.automation
        ));
    }

    #[test]
    fn safe_name_blocks_paths() {
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("HCI (2150469601)"), "HCI (2150469601)");
        assert_eq!(safe_name("a/b:c.pdf"), "a_b_c.pdf");
        assert_eq!(safe_name(".."), "untitled");
        assert_eq!(safe_name(".hidden"), "hidden");
    }
}
