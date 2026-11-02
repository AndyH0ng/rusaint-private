//! 식당·매장 운영 시간.
//!
//! - 매장시간(`main.php?mkey=2&w=1`): 식당·편의점·카페·서점의 위치, 연락처, 평일/주말 운영 시간
//! - 오늘의 식단(`main.php?mkey=2&w=3`, `sdt` POST): 코너별 배식 시간. 이번 주 날짜만 나옵니다.
//! - 기숙사 식단 머리말의 배식 시간 안내
//!
//! 두 페이지의 시간이 서로 다를 때가 있어(학생식당 아침 08:00~09:00 / 08:10~09:20) 출처를 나눠 둡니다.

use std::sync::LazyLock;

use scraper::{ElementRef, Html, Selector};
use serde::Serialize;

use crate::text::{block_text, collapse_whitespace, lines};

/// 매장 하나의 기본 정보
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreHours {
    /// 식당, 편의점, 커피점, 서점/문구
    pub category: String,
    pub name: String,
    pub location: Option<String>,
    pub phone: Option<String>,
    /// 평일 운영 시간 (여러 줄일 수 있음)
    pub weekday: Vec<String>,
    pub weekend: Vec<String>,
}

/// 식당 하나의 코너별 배식 시간
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CornerHours {
    pub cafeteria: String,
    pub corners: Vec<Corner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Corner {
    /// 조식(천원의 아침), 점심 1코너, 석식 1코너 …
    pub name: String,
    /// "평일 : 08:10~09:20" 같은 원문
    pub hours: Option<String>,
}

static TABLE_ROWS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("tr").unwrap());
static CELLS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(":scope > td").unwrap());
static REST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td.rest_nm, td.rest_mn").unwrap());
static TIME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span.time_mn").unwrap());

fn class_is(el: ElementRef<'_>, class: &str) -> bool {
    el.value()
        .has_class(class, scraper::CaseSensitivity::CaseSensitive)
}

/// 매장시간 페이지의 표를 읽습니다. 구분 칸은 보통 `rowspan` 으로 여러 행에 걸쳐 있습니다.
pub fn parse_store_hours(html: &str) -> Vec<StoreHours> {
    let doc = Html::parse_document(html);
    let mut category = String::new();
    let mut out = Vec::new();
    for row in doc.select(&TABLE_ROWS) {
        let cells: Vec<ElementRef<'_>> = row.select(&CELLS).collect();
        let mut cells = cells.as_slice();
        // 구분이 있는 행은 6칸(구분, 매장명, 위치, 연락처, 운영시간, 상세보기), 이어지는 행은 5칸입니다.
        // 한 줄짜리 구분은 rowspan 이 없어서 칸 수로 판단합니다.
        if cells.len() >= 6
            && let Some(first) = cells.first()
            && class_is(*first, "sh_name")
        {
            category = collapse_whitespace(&first.text().collect::<String>());
            cells = &cells[1..];
        }
        // 매장명, 위치, 연락처, 운영시간, (상세보기)
        let [name, location, phone, hours, ..] = cells else {
            continue;
        };
        if !class_is(*name, "sh_name") {
            continue;
        }
        let (mut weekday, mut weekend) = (Vec::new(), Vec::new());
        for line in hours.select(&TABLE_ROWS) {
            let tds: Vec<ElementRef<'_>> = line.select(&CELLS).collect();
            let [label, value] = tds.as_slice() else {
                continue;
            };
            let values: Vec<String> = lines(&block_text(**value)).collect();
            match collapse_whitespace(&label.text().collect::<String>()).as_str() {
                l if l.starts_with("평일") => weekday = values,
                l if l.starts_with("주말") => weekend = values,
                _ => {}
            }
        }
        let text = |el: &ElementRef<'_>| {
            let t: Vec<String> = lines(&block_text(**el)).collect();
            (!t.is_empty()).then(|| t.join(", "))
        };
        out.push(StoreHours {
            category: category.clone(),
            name: collapse_whitespace(&name.text().collect::<String>()),
            location: text(location),
            phone: text(phone),
            weekday,
            weekend,
        });
    }
    out
}

/// 오늘의 식단 페이지에서 식당별 코너 이름과 배식 시간을 읽습니다.
pub fn parse_corner_hours(html: &str) -> Vec<CornerHours> {
    let doc = Html::parse_document(html);
    let mut out: Vec<CornerHours> = Vec::new();
    for cell in doc.select(&REST) {
        if class_is(cell, "rest_nm") {
            out.push(CornerHours {
                cafeteria: collapse_whitespace(&cell.text().collect::<String>()),
                corners: Vec::new(),
            });
            continue;
        }
        let Some(current) = out.last_mut() else {
            continue;
        };
        let hours = cell
            .select(&TIME)
            .next()
            .map(|t| collapse_whitespace(&t.text().collect::<String>()));
        // 시간 줄을 뺀 나머지가 코너 이름 (스낵코너는 "(추억의도시락,…)" 같은 부연이 붙음)
        let name: Vec<String> = lines(&block_text(*cell))
            .filter(|l| Some(l) != hours.as_ref())
            .collect();
        current.corners.push(Corner {
            name: name.join(" "),
            hours,
        });
    }
    out
}

/// 기숙사 식단 머리말의 "*기숙사식당 배식시간 안내*" 부분을 줄 단위로 돌려줍니다.
pub fn parse_dorm_hours(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let text = block_text(*doc.root_element());
    let Some(start) = text.find("배식시간 안내") else {
        return Vec::new();
    };
    let rest = &text[start..];
    let end = rest.find("날짜").unwrap_or(rest.len().min(400));
    rest[..end]
        .split('*')
        .map(collapse_whitespace)
        .filter(|l| !l.is_empty() && !l.contains("배식시간 안내"))
        .collect()
}
