//! 숭실대학교 학사일정(`ssu.ac.kr/학사/학사일정/?years=YYYY`).
//!
//! 로그인 없이 볼 수 있는 공개 페이지입니다. 연도별 페이지에 월별 블록(`#calendarYYYYMM`)이 있고,
//! 블록마다 "01.05 (월) ~ 01.28 (수)" 같은 날짜와 행사명이 나옵니다.

use std::sync::LazyLock;
use std::time::Duration;

use chrono::{Datelike, NaiveDate};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;

const URL: &str = "https://ssu.ac.kr/%ED%95%99%EC%82%AC/%ED%95%99%EC%82%AC%EC%9D%BC%EC%A0%95/";
const USER_AGENT: &str = concat!(
    "rusaint-calendar/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AndyH0ng/rusaint)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 학사일정 하나
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub start: NaiveDate,
    /// 하루짜리 일정은 `start` 와 같습니다.
    pub end: NaiveDate,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CalendarError {
    #[error("요청 실패: {0}")]
    Request(#[from] reqwest::Error),
    #[error("서버가 HTTP {0} 을(를) 반환했습니다")]
    Status(u16),
}

static MONTHS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"#calendar div[id^="calendar"]"#).unwrap());
static ITEMS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("li .row").unwrap());
static COLUMNS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(":scope > div").unwrap());

fn text(el: ElementRef<'_>) -> String {
    el.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// "01.05" → 그 달의 날짜. 연도는 블록에서 정합니다.
fn month_day(raw: &str) -> Option<(u32, u32)> {
    let mut parts = raw.trim().get(..5)?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// 연도별 페이지를 읽습니다. 연말에 시작해 이듬해에 끝나는 일정(12.28 ~ 01.03)은 끝 날짜를 다음 해로 둡니다.
pub fn parse(html: &str) -> Vec<Event> {
    let doc = Html::parse_document(html);
    let mut events = Vec::new();
    for block in doc.select(&MONTHS) {
        // id="calendar202601"
        let Some(year) = block
            .value()
            .id()
            .and_then(|id| id.strip_prefix("calendar"))
            .and_then(|ym| ym.get(..4))
            .and_then(|y| y.parse::<i32>().ok())
        else {
            continue;
        };
        for row in block.select(&ITEMS) {
            let columns: Vec<_> = row.select(&COLUMNS).collect();
            let [date, title] = columns.as_slice() else {
                continue;
            };
            let date = text(*date);
            let mut range = date.split('~');
            let Some((sm, sd)) = range.next().and_then(month_day) else {
                continue;
            };
            let Some(start) = NaiveDate::from_ymd_opt(year, sm, sd) else {
                continue;
            };
            let end = match range.next().and_then(month_day) {
                Some((em, ed)) => {
                    let end_year = if em < sm { year + 1 } else { year };
                    NaiveDate::from_ymd_opt(end_year, em, ed).unwrap_or(start)
                }
                None => start,
            };
            let title = text(*title);
            if !title.is_empty() {
                events.push(Event { start, end, title });
            }
        }
    }
    events
}

pub struct CalendarClient {
    http: reqwest::Client,
}

impl CalendarClient {
    pub fn new() -> Result<Self, CalendarError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { http })
    }

    /// 한 해의 학사일정
    pub async fn year(&self, year: i32) -> Result<Vec<Event>, CalendarError> {
        let response = self
            .http
            .get(URL)
            .query(&[("years", year.to_string())])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(CalendarError::Status(response.status().as_u16()));
        }
        Ok(parse(&response.text().await?))
    }

    /// `from`..=`to` 기간과 겹치는 일정. 두 해에 걸치면 두 페이지를 읽습니다.
    pub async fn between(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Event>, CalendarError> {
        let mut events = Vec::new();
        for year in from.year()..=to.year() {
            events.extend(self.year(year).await?);
        }
        events.retain(|e| e.end >= from && e.start <= to);
        events.sort_by_key(|e| (e.start, e.end));
        events.dedup();
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events() -> Vec<Event> {
        parse(include_str!("../tests/fixtures/calendar_2026.html"))
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn parses_ranges_and_single_days() {
        let events = events();
        assert!(events.len() > 50, "{}", events.len());
        let leave = events
            .iter()
            .find(|e| e.title == "2026학년도 1학기 휴학/복학 신청기간(1차)")
            .unwrap();
        assert_eq!(
            (leave.start, leave.end),
            (date(2026, 1, 5), date(2026, 1, 28))
        );
        let opening = events
            .iter()
            .find(|e| e.title == "2026학년도 1학기 개강")
            .unwrap();
        assert_eq!(opening.start, opening.end);
    }

    #[test]
    fn year_end_ranges_roll_over() {
        let event = events()
            .into_iter()
            .find(|e| e.start == date(2026, 12, 28) && e.end.year() == 2027)
            .unwrap();
        assert_eq!(event.end, date(2027, 1, 3));
    }

    #[test]
    fn every_event_is_in_its_year() {
        assert!(
            events()
                .iter()
                .all(|e| e.start.year() == 2026 && e.end >= e.start)
        );
    }
}
