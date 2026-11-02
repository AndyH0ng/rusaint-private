//! 숭실대학교 학생식당(soongguri.com)과 기숙사 식당 메뉴를 가져오는 클라이언트입니다.
//!
//! 메뉴 칸은 식당에서 웹 에디터로 자유롭게 작성하므로 가격이나 메뉴명을 구조화하지 않고,
//! 화면에 보이는 줄 단위 텍스트로 정리해서 돌려줍니다. 알레르기·원산지 정보는 제외합니다.

mod campus;
mod dorm;
mod text;

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use serde::Serialize;
use tokio::sync::Mutex;

pub use campus::{menu_status, parse_campus};
pub use dorm::parse_dorm;

const CAMPUS_MENU_URL: &str = "https://soongguri.com/m/m_req/m_menu.php";
const DORM_MENU_URL: &str = "https://ssudorm.ssu.ac.kr:444/SShostel/mall_main.php";
const USER_AGENT: &str = concat!(
    "rusaint-meal/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// soongguri.com 에서 메뉴를 제공하는 교내 식당
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Cafeteria {
    /// 학생식당
    Student,
    /// 숭실도담식당
    Dodam,
    /// 스낵코너
    Snack,
    /// 푸드코트
    FoodCourt,
    /// THE KITCHEN
    TheKitchen,
    /// FACULTY LOUNGE
    FacultyLounge,
}

impl Cafeteria {
    /// 모든 교내 식당
    pub const ALL: [Cafeteria; 6] = [
        Cafeteria::Student,
        Cafeteria::Dodam,
        Cafeteria::Snack,
        Cafeteria::FoodCourt,
        Cafeteria::TheKitchen,
        Cafeteria::FacultyLounge,
    ];

    /// soongguri.com 의 식당 코드(`rcd`)
    pub fn code(self) -> &'static str {
        match self {
            Cafeteria::Student => "1",
            Cafeteria::Dodam => "2",
            Cafeteria::Snack => "4",
            Cafeteria::FoodCourt => "5",
            Cafeteria::TheKitchen => "6",
            Cafeteria::FacultyLounge => "7",
        }
    }

    /// 식당 표시 이름
    pub fn name(self) -> &'static str {
        match self {
            Cafeteria::Student => "학생식당",
            Cafeteria::Dodam => "숭실도담식당",
            Cafeteria::Snack => "스낵코너",
            Cafeteria::FoodCourt => "푸드코트",
            Cafeteria::TheKitchen => "THE KITCHEN",
            Cafeteria::FacultyLounge => "FACULTY LOUNGE",
        }
    }
}

impl fmt::Display for Cafeteria {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 알 수 없는 식당 이름
#[derive(Debug, thiserror::Error)]
#[error("알 수 없는 식당: {0}")]
pub struct UnknownCafeteria(String);

impl FromStr for Cafeteria {
    type Err = UnknownCafeteria;

    /// 표시 이름, 식당 코드, 변형 이름(`student`, `dodam` …)을 모두 받습니다.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let key = key.to_lowercase();
        Cafeteria::ALL
            .into_iter()
            .find(|c| {
                let name: String = c.name().chars().filter(|c| !c.is_whitespace()).collect();
                key == name.to_lowercase()
                    || key == c.code()
                    || key == format!("{c:?}").to_lowercase()
            })
            .or(match key.as_str() {
                "학식" => Some(Cafeteria::Student),
                "도담" | "도담식당" => Some(Cafeteria::Dodam),
                "스낵" => Some(Cafeteria::Snack),
                "교직원식당" => Some(Cafeteria::FacultyLounge),
                _ => None,
            })
            .ok_or_else(|| UnknownCafeteria(s.to_string()))
    }
}

/// 하루 메뉴의 운영 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuStatus {
    /// 모든 끼니가 정상 운영
    Ok,
    /// 일부 끼니·코너만 운영하지 않음
    Partial,
    /// 운영하지 않음. soongguri.com 은 메뉴가 아직 등록되지 않은 날도 같은 문구를 보여주므로
    /// 미래 날짜라면 "미등록"일 수 있습니다.
    Closed,
}

/// 끼니(또는 코너) 하나의 메뉴
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meal {
    /// 페이지의 구분 칸 (`중식1`, `석식1`, `메뉴` …). 실제 내용과 맞지 않는 경우가 있습니다.
    pub slot: String,
    /// 본문 앞부분의 `[대면코너]` 같은 코너 이름
    pub section: Option<String>,
    /// 메뉴 대신 미운영 안내가 들어 있는 경우
    pub closed: bool,
    /// 화면에 보이는 줄 단위 텍스트
    pub lines: Vec<String>,
}

/// 교내 식당 하루 메뉴
#[derive(Debug, Clone, Serialize)]
pub struct CafeteriaMenu {
    pub date: NaiveDate,
    pub cafeteria: Cafeteria,
    pub cafeteria_name: &'static str,
    pub status: MenuStatus,
    /// 메뉴 칸 밖의 휴무 안내 (`오늘은 쉽니다.` 등)
    pub notices: Vec<String>,
    pub meals: Vec<Meal>,
}

/// 기숙사 식당 하루 메뉴. 각 끼니는 줄 단위 텍스트이며 `미운영`이 들어갈 수 있습니다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DormDay {
    pub date: NaiveDate,
    pub breakfast: Vec<String>,
    pub lunch: Vec<String>,
    pub dinner: Vec<String>,
    /// 중·석식 (주말 통합 운영 등)
    pub lunch_dinner: Vec<String>,
}

/// 메뉴 조회 오류
#[derive(Debug, thiserror::Error)]
pub enum MealError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("서버가 HTTP {0} 을(를) 반환했습니다")]
    Status(u16),
}

/// 식당 메뉴 클라이언트. 학교 서버 부담을 줄이기 위해 요청 사이에 최소 1초 간격을 둡니다.
pub struct MealClient {
    http: reqwest::Client,
    last_request: Mutex<Option<Instant>>,
}

impl MealClient {
    pub fn new() -> Result<Self, MealError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            last_request: Mutex::new(None),
        })
    }

    /// 교내 식당의 하루 메뉴를 가져옵니다.
    pub async fn campus(
        &self,
        cafeteria: Cafeteria,
        date: NaiveDate,
    ) -> Result<CafeteriaMenu, MealError> {
        let sdt = date.format("%Y%m%d").to_string();
        let body = self
            .get(CAMPUS_MENU_URL, &[("rcd", cafeteria.code()), ("sdt", &sdt)])
            .await?;
        let html = String::from_utf8_lossy(&body);
        let (meals, notices) = parse_campus(&html);
        Ok(CafeteriaMenu {
            date,
            cafeteria,
            cafeteria_name: cafeteria.name(),
            status: menu_status(&meals, &notices),
            notices,
            meals,
        })
    }

    /// `date`가 속한 주(월~일)의 기숙사 식당 메뉴를 가져옵니다. 아직 등록되지 않은 주는 빈 목록입니다.
    pub async fn dorm_week(&self, date: NaiveDate) -> Result<Vec<DormDay>, MealError> {
        let (year, month, day) = (
            date.format("%Y").to_string(),
            date.format("%m").to_string(),
            date.format("%d").to_string(),
        );
        let body = self
            .get(
                DORM_MENU_URL,
                &[
                    ("viewform", "B0001_foodboard_list"),
                    ("board_no", "1"),
                    ("gyear", &year),
                    ("gmonth", &month),
                    ("gday", &day),
                ],
            )
            .await?;
        let (html, _, _) = encoding_rs::EUC_KR.decode(&body);
        Ok(parse_dorm(&html))
    }

    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Vec<u8>, MealError> {
        let mut last = self.last_request.lock().await;
        if let Some(at) = *last {
            let elapsed = at.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        let result = self.http.get(url).query(query).send().await;
        *last = Some(Instant::now());
        let response = result?;
        if !response.status().is_success() {
            return Err(MealError::Status(response.status().as_u16()));
        }
        Ok(response.bytes().await?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cafeteria_from_str() {
        assert_eq!("학생식당".parse::<Cafeteria>().unwrap(), Cafeteria::Student);
        assert_eq!("도담".parse::<Cafeteria>().unwrap(), Cafeteria::Dodam);
        assert_eq!(
            "the kitchen".parse::<Cafeteria>().unwrap(),
            Cafeteria::TheKitchen
        );
        assert_eq!("7".parse::<Cafeteria>().unwrap(), Cafeteria::FacultyLounge);
        assert_eq!("faculty_lounge".parse::<Cafeteria>().ok(), None);
        assert_eq!(
            "FacultyLounge".parse::<Cafeteria>().unwrap(),
            Cafeteria::FacultyLounge
        );
    }
}
