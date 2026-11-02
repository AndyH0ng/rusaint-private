use chrono::NaiveDate;
use rusaint_meal::{MenuStatus, menu_status, parse_campus, parse_dorm};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn student_cafeteria_menu() {
    let (meals, notices) = parse_campus(&fixture("student_20261102.html"));
    assert!(notices.is_empty());
    assert_eq!(menu_status(&meals, &notices), MenuStatus::Ok);
    let slots: Vec<_> = meals.iter().map(|m| m.slot.as_str()).collect();
    assert_eq!(slots, ["중식1", "중식2", "중식3", "석식1"]);
    assert_eq!(
        meals[0].lines,
        [
            "[Hot Pot]",
            "★ 등촌샤브칼국수 - 5.0",
            "Deungchon-Style Shabu Shabu Noodle Soup",
            "메밀전병",
            "미니밥",
            "배추김치",
        ]
    );
    assert_eq!(meals[0].section.as_deref(), Some("[Hot Pot]"));
}

#[test]
fn allergy_and_origin_lines_are_dropped() {
    for name in ["student_20261102.html", "dodam_20261023_partial.html"] {
        let (meals, _) = parse_campus(&fixture(name));
        for line in meals.iter().flat_map(|m| &m.lines) {
            assert!(
                !line.contains("알러지") && !line.contains("원산지") && !line.contains("국내산"),
                "{name}: {line}"
            );
        }
    }
}

#[test]
fn closure_notice_inside_menu_cell_marks_partial() {
    let (meals, notices) = parse_campus(&fixture("dodam_20261023_partial.html"));
    assert_eq!(menu_status(&meals, &notices), MenuStatus::Partial);
    let closed: Vec<_> = meals
        .iter()
        .filter(|m| m.closed)
        .map(|m| m.slot.as_str())
        .collect();
    assert_eq!(closed, ["중식4"]);
    // 첫 줄이 "Serve Kitchen"이어도 코너 이름을 찾는다
    assert_eq!(meals[0].section.as_deref(), Some("[대면코너]"));
}

#[test]
fn snack_corner_sections() {
    let (meals, _) = parse_campus(&fixture("snack_20261102.html"));
    assert!(meals.iter().all(|m| m.slot == "메뉴"));
    let sections: Vec<_> = meals.iter().filter_map(|m| m.section.as_deref()).collect();
    assert_eq!(
        sections,
        [
            "[밥 메뉴]",
            "[국수 메뉴]",
            "[샐러드 메뉴]",
            "[김밥코너]",
            "[떡볶이 메뉴]"
        ]
    );
}

#[test]
fn closed_day() {
    let (meals, notices) = parse_campus(&fixture("foodcourt_20261102_closed.html"));
    assert!(meals.is_empty());
    assert_eq!(notices, ["오늘은 쉽니다."]);
    assert_eq!(menu_status(&meals, &notices), MenuStatus::Closed);
}

#[test]
fn dorm_week() {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/dorm_20261102.euckr.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let (html, _, _) = encoding_rs::EUC_KR.decode(&bytes);
    let days = parse_dorm(&html);
    assert_eq!(days.len(), 7);
    assert_eq!(days[0].date, NaiveDate::from_ymd_opt(2026, 11, 2).unwrap());
    assert_eq!(days[6].date, NaiveDate::from_ymd_opt(2026, 11, 8).unwrap());
    assert_eq!(days[0].breakfast, ["미운영"]);
    assert_eq!(
        days[0].lunch.first().map(String::as_str),
        Some("맑은콩나물국")
    );
    assert_eq!(days[0].lunch.len(), 7);
    assert!(days[0].lunch_dinner.is_empty());
}

/// 실제 서버에 요청합니다: `cargo test -p rusaint-meal -- --ignored`
#[tokio::test]
#[ignore]
async fn live_fetch() {
    let client = rusaint_meal::MealClient::new().unwrap();
    let today = chrono::Local::now().date_naive();
    for cafeteria in rusaint_meal::Cafeteria::ALL {
        let menu = client.campus(cafeteria, today).await.unwrap();
        println!(
            "{} {:?} {} meals",
            menu.cafeteria_name,
            menu.status,
            menu.meals.len()
        );
    }
    let week = client.dorm_week(today).await.unwrap();
    println!("dorm {} days", week.len());
    assert!(!week.is_empty());
}
