//! KICA증명 화면 파싱. 폼 필드와 보관함 표만 읽고 네트워크는 다루지 않습니다.

use std::collections::BTreeMap;

use scraper::{Html, Selector};

use crate::model::Document;

/// 로그인된 화면에만 있는 로그아웃 링크
const LOGGED_IN_MARKER: &str = "javascript:logout();";

/// 화면이 로그인된 상태인지
pub fn logged_in(html: &str) -> bool {
    html.contains(LOGGED_IN_MARKER)
}

/// 서버가 그려 준 폼. 값은 화면에 있는 그대로 돌려주고, 보낼 때 고를 책임은 호출자에게 둡니다.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Form {
    pub action: String,
    /// `name=value` 입력. 체크되지 않은 checkbox·radio 는 제외합니다.
    pub fields: BTreeMap<String, String>,
    /// select 이름별 선택 가능한 값
    pub options: BTreeMap<String, Vec<String>>,
}

/// 첫 번째 폼. 화면마다 폼이 하나뿐인 것이 KICA증명의 관례입니다.
pub fn first_form(html: &str) -> Option<Form> {
    let document = Html::parse_document(html);
    let form = Selector::parse("form").expect("static selector");
    let input =
        Selector::parse("input[name], select[name], textarea[name]").expect("static selector");
    let option = Selector::parse("option").expect("static selector");
    let element = document.select(&form).next()?;
    let mut parsed = Form {
        action: element
            .value()
            .attr("action")
            .unwrap_or_default()
            .to_owned(),
        ..Form::default()
    };
    for field in element.select(&input) {
        let value = field.value();
        let Some(name) = value.attr("name") else {
            continue;
        };
        match value.name() {
            "input" => {
                let kind = value.attr("type").unwrap_or("text").to_ascii_lowercase();
                if matches!(kind.as_str(), "checkbox" | "radio") && value.attr("checked").is_none()
                {
                    continue;
                }
                parsed.fields.insert(
                    name.to_owned(),
                    value.attr("value").unwrap_or_default().to_owned(),
                );
            }
            "select" => {
                let mut values = Vec::new();
                let mut selected = None;
                for item in field.select(&option) {
                    let value = item.value().attr("value").unwrap_or_default().to_owned();
                    if item.value().attr("selected").is_some() {
                        selected = Some(value.clone());
                    }
                    values.push(value);
                }
                let chosen = selected
                    .or_else(|| values.first().cloned())
                    .unwrap_or_default();
                parsed.fields.insert(name.to_owned(), chosen);
                parsed.options.insert(name.to_owned(), values);
            }
            _ => {
                parsed.fields.insert(name.to_owned(), String::new());
            }
        }
    }
    Some(parsed)
}

/// 결제 화면은 조회 조건과 PG 입력을 서로 다른 폼에 담습니다.
pub fn forms(html: &str) -> Vec<Form> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("form").expect("static selector");
    document
        .select(&selector)
        .filter_map(|form| first_form(&form.html()))
        .collect()
}

/// `javascript:preview('종류','문서번호','null','통수','출력','국문','구분','가능')` 의 인자.
/// 사이트는 같은 링크를 `reportx.preview(...)` 로도 그리므로 두 형태를 모두 읽습니다.
fn preview_arguments(href: &str) -> Option<Vec<String>> {
    let rest = href
        .strip_prefix("javascript:preview(")
        .or_else(|| href.strip_prefix("javascript:reportx.preview("))?;
    let mut arguments = Vec::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let value: String = chars.by_ref().take_while(|c| *c != '\'').collect();
                arguments.push(value);
            }
            ')' => break,
            _ => {}
        }
    }
    (arguments.len() == 8).then_some(arguments)
}

/// 일반 출력 보관함의 발급 내역. 출력 매수를 읽을 수 없는 행은 버립니다.
pub fn documents(html: &str) -> Vec<Document> {
    let page = Html::parse_document(html);
    let link = Selector::parse("a[href]").expect("static selector");
    let mut found = Vec::new();
    for anchor in page.select(&link) {
        let Some(arguments) = preview_arguments(anchor.value().attr("href").unwrap_or_default())
        else {
            continue;
        };
        let (Ok(copies), Ok(printed)) = (arguments[3].parse::<u32>(), arguments[4].parse::<u32>())
        else {
            continue;
        };
        if arguments[1].is_empty() {
            continue;
        }
        found.push(Document {
            document_id: arguments[1].clone(),
            name: arguments[0].clone(),
            copies,
            printed,
            korean: arguments[5] == "Y",
            print_kind: arguments[6].clone(),
            printable: arguments[7].clone(),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASKET: &str = r#"<html><body><table>
        <tr><td><a href="javascript:certdown('0','B');">PDF 다운로드</a></td></tr>
        <tr><td>장학금지급증명서<a href="javascript:preview('장학금지급증명서','2026ABCD','null','1','0','Y','1','Y')">문서출력</a></td></tr>
        <tr><td>재학증명서<a href="javascript:reportx.preview('재학증명서','2026WXYZ','null','2','1','N','1','Y')">문서출력</a></td></tr>
        <tr><td>처리진행<a href="javascript:preview('성적증명서','2026NOPE','null','x','0','Y','1','Y')">문서출력</a></td></tr>
        </table><a href="javascript:logout();">로그아웃</a></body></html>"#;

    #[test]
    fn reads_both_preview_link_shapes() {
        let found = documents(BASKET);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].document_id, "2026ABCD");
        assert_eq!((found[0].copies, found[0].printed), (1, 0));
        assert!(found[0].korean && found[0].available());
        assert_eq!(found[1].name, "재학증명서");
        assert!(!found[1].korean);
        // 2통 중 1통을 출력했으므로 아직 남아 있습니다.
        assert!(found[1].available());
    }

    #[test]
    fn skips_rows_without_counts() {
        assert!(
            !documents(BASKET)
                .iter()
                .any(|d| d.document_id == "2026NOPE")
        );
    }

    #[test]
    fn exhausted_document_is_not_available() {
        let html = r#"<a href="javascript:preview('재학증명서','2026DONE','null','1','1','Y','1','Y')">x</a>"#;
        let found = documents(html);
        assert_eq!(found.len(), 1);
        assert!(!found[0].available());
    }

    #[test]
    fn empty_basket_has_no_documents() {
        assert!(
            documents("<table><tr><td>신청한 증명서가 존재하지 않습니다.</td></tr></table>")
                .is_empty()
        );
    }

    #[test]
    fn login_marker_is_required() {
        assert!(logged_in(BASKET));
        assert!(!logged_in("<html><body>로그인이 필요합니다</body></html>"));
    }

    #[test]
    fn reads_form_fields_and_options() {
        let html = r#"<form action="/servlet/WMCERT?COMMAND=CERTILIST">
            <input type="hidden" name="GIWANNO" value="021163">
            <input type="checkbox" name="issue5" value="issue5">
            <input type="checkbox" name="issue6" value="issue6" checked>
            <select name="minwoncount5"><option value="01">1</option><option value="02">2</option></select>
            <select name="Inner_JANG_YEAR5"><option value="0">선택</option><option value="20241" selected>2024-1</option></select>
            </form><form action="/second"><input name="ignored" value="1"></form>"#;
        let form = first_form(html).expect("form");
        assert_eq!(form.action, "/servlet/WMCERT?COMMAND=CERTILIST");
        assert_eq!(
            form.fields.get("GIWANNO").map(String::as_str),
            Some("021163")
        );
        // 체크되지 않은 신청 항목은 보내지 않습니다.
        assert!(!form.fields.contains_key("issue5"));
        assert_eq!(
            form.fields.get("issue6").map(String::as_str),
            Some("issue6")
        );
        assert_eq!(
            form.fields.get("minwoncount5").map(String::as_str),
            Some("01")
        );
        assert_eq!(
            form.fields.get("Inner_JANG_YEAR5").map(String::as_str),
            Some("20241")
        );
        assert_eq!(
            form.options.get("Inner_JANG_YEAR5").map(Vec::as_slice),
            Some(["0".to_owned(), "20241".to_owned()].as_slice())
        );
        assert!(!form.fields.contains_key("ignored"));
    }

    #[test]
    fn no_form_is_none() {
        assert!(first_form("<html><body>없음</body></html>").is_none());
    }
}
