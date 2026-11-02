//! 실제 LMS 에 로그인해 읽기 전용 요청만 보냅니다. 기본으로는 건너뜁니다.
//!
//! ```sh
//! SSO_ID=... SSO_PASSWORD="$(security find-generic-password -s rusaint-sso -w)" \
//!   cargo test -p rusaint-lms --test live -- --ignored --nocapture
//! ```

use rusaint_lms::LmsClient;

#[tokio::test]
#[ignore]
async fn login_and_read() {
    let client = LmsClient::new(
        std::env::var("SSO_ID").unwrap_or_default(),
        std::env::var("SSO_PASSWORD").unwrap_or_default(),
    )
    .unwrap();
    client.login().await.unwrap();
    println!("login ok");

    let terms = client.terms().await.unwrap();
    println!(
        "terms: {:?}",
        terms
            .iter()
            .map(|t| (&t.name, t.default))
            .collect::<Vec<_>>()
    );
    let term = terms
        .iter()
        .find(|t| t.default && t.name.contains("학기"))
        .unwrap();

    let courses = client.courses(term.id).await.unwrap();
    for course in &courses {
        println!(
            "course {} {} ({:?})",
            course.id, course.name, course.professors
        );
    }
    for todo in client.todos(term.id).await.unwrap() {
        for item in &todo.todo_list {
            println!(
                "todo [{}] {:?} {} due {:?}",
                todo.course_id, item.component_type, item.title, item.due_date
            );
        }
    }

    let mut first_pdf = None;
    for course in &courses {
        let modules = client.modules(course.id).await.unwrap();
        let materials: Vec<_> = modules
            .iter()
            .flat_map(|m| &m.module_items)
            .filter_map(|i| i.material())
            .collect();
        println!(
            "modules {}: {} weeks, {} materials",
            course.name,
            modules.len(),
            materials.len()
        );
        if first_pdf.is_none() {
            first_pdf = materials.into_iter().find(|m| m.kind == "pdf");
        }
    }

    if let Some(pdf) = first_pdf {
        let dest = std::env::temp_dir().join(format!("rusaint-lms-test-{}.pdf", pdf.content_id));
        let bytes = client.download(&pdf.content_id, &dest).await.unwrap();
        let head = std::fs::read(&dest).unwrap();
        println!(
            "download {} bytes, starts with %PDF: {}",
            bytes,
            head.starts_with(b"%PDF")
        );
        std::fs::remove_file(dest).unwrap();
    }
}
