use std::sync::LazyLock;

use chrono::NaiveDate;
use scraper::{ElementRef, Html, Selector};

use crate::DormDay;
use crate::text::{block_text, lines};

static ROWS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("table.boxstyle02 tr").unwrap());

/// 기숙사 식단표(날짜, 조식, 중식, 석식, 중·석식)를 날짜별로 읽습니다.
pub fn parse_dorm(html: &str) -> Vec<DormDay> {
    let doc = Html::parse_document(html);
    doc.select(&ROWS)
        .filter_map(|row| {
            let cells: Vec<Vec<String>> = row
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|el| matches!(el.value().name(), "th" | "td"))
                .map(|cell| lines(&block_text(*cell)).collect())
                .collect();
            let [date, breakfast, lunch, dinner, lunch_dinner] =
                <[Vec<String>; 5]>::try_from(cells).ok()?;
            // "2026-11-02 (월)"
            let date = NaiveDate::parse_from_str(date.first()?.get(..10)?, "%Y-%m-%d").ok()?;
            Some(DormDay {
                date,
                breakfast,
                lunch,
                dinner,
                lunch_dinner,
            })
        })
        .collect()
}
