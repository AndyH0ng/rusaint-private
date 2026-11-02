//! 저장해 둔 soongguri.com 페이지들을 파싱해 JSON으로 출력합니다. 파서 회귀 확인용입니다.
//!
//! cargo run -p rusaint-meal --example parse_files -- FILE...

use std::collections::BTreeMap;

use serde_json::json;

fn main() {
    let mut out = BTreeMap::new();
    for path in std::env::args().skip(1) {
        let html = std::fs::read_to_string(&path).expect("read file");
        let (meals, notices) = rusaint_meal::parse_campus(&html);
        let status = rusaint_meal::menu_status(&meals, &notices);
        let name = path.rsplit('/').next().unwrap().to_string();
        out.insert(
            name,
            json!({ "status": status, "notices": notices, "meals": meals }),
        );
    }
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
