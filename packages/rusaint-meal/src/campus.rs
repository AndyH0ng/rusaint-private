use std::sync::LazyLock;

use regex_lite::Regex;
use scraper::{ElementRef, Html, Node, Selector};

use crate::text::{block_text, collapse_whitespace, lines};
use crate::{Meal, MenuStatus};

/// 이 줄부터는 알레르기·원산지 정보라 버립니다.
static CUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("알러지유발식품|알레르기|원산지").unwrap());
/// 미운영 안내 문구
static CLOSURE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("쉽니다|휴무|공휴일|어린이날|운영하지|운영 안|미운영").unwrap());
static SECTION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[^\]]+\]$").unwrap());
static MENU_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("td.menu_nm").unwrap());

/// soongguri.com 메뉴 페이지를 끼니 목록과 메뉴 칸 밖의 휴무 안내로 나눕니다.
pub fn parse_campus(html: &str) -> (Vec<Meal>, Vec<String>) {
    let doc = Html::parse_document(html);
    let meals = doc
        .select(&MENU_NAME)
        .map(|name_cell| {
            let lines = name_cell
                .next_siblings()
                .filter_map(ElementRef::wrap)
                .find(|el| {
                    el.value()
                        .has_class("menu_list", scraper::CaseSensitivity::CaseSensitive)
                })
                .map(|list_cell| menu_lines(&block_text(*list_cell)))
                .unwrap_or_default();
            let section = lines
                .iter()
                .take(3)
                .find(|l| SECTION_RE.is_match(l))
                .cloned();
            let closed = lines.iter().any(|l| CLOSURE_RE.is_match(l));
            Meal {
                slot: collapse_whitespace(&name_cell.text().collect::<String>()),
                section,
                closed,
                lines,
            }
        })
        .collect();

    let mut notices: Vec<String> = Vec::new();
    for node in doc.root_element().descendants() {
        let Node::Text(text) = node.value() else {
            continue;
        };
        let text = collapse_whitespace(text);
        if text.is_empty() || !CLOSURE_RE.is_match(&text) || notices.contains(&text) {
            continue;
        }
        let inside_menu_or_script = node.ancestors().filter_map(ElementRef::wrap).any(|el| {
            let el = el.value();
            matches!(el.name(), "script" | "style")
                || el.classes().any(|c| c == "menu_nm" || c == "menu_list")
        });
        if !inside_menu_or_script {
            notices.push(text);
        }
    }
    (meals, notices)
}

/// 알레르기·원산지 정보 앞까지의 줄을 연속 중복 없이 모읍니다.
fn menu_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines(&text.replace('\u{a0}', " ")) {
        if CUT_RE.is_match(&line) {
            break;
        }
        if out.last() != Some(&line) {
            out.push(line);
        }
    }
    out
}

/// [`parse_campus`] 결과로 하루 운영 상태를 판단합니다.
pub fn menu_status(meals: &[Meal], notices: &[String]) -> MenuStatus {
    if meals.iter().all(|m| m.closed) {
        MenuStatus::Closed
    } else if !notices.is_empty() || meals.iter().any(|m| m.closed) {
        MenuStatus::Partial
    } else {
        MenuStatus::Ok
    }
}
