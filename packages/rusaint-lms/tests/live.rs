//! 실제 LMS 에 로그인해 읽기 전용 요청만 보냅니다. 기본으로는 건너뜁니다.
//!
//! ```sh
//! SSO_ID=... SSO_PASSWORD="$(security find-generic-password -s rusaint-sso -w)" \
//!   cargo test -p rusaint-lms --test live -- --ignored --nocapture
//! ```

use rusaint_lms::LmsClient;

/// LMS_VIDEO_CONTENT_ID 영상 전체를 임시 파일로 받아 검증한 뒤 삭제합니다.
#[tokio::test]
#[ignore]
async fn download_video() {
    use std::io::Read;

    let content_id = std::env::var("LMS_VIDEO_CONTENT_ID").expect("LMS_VIDEO_CONTENT_ID");
    let client = LmsClient::new(rusaint_sso::SsoCredentials::new("", "")).unwrap();
    let response = client.open_material(&content_id).await.unwrap();
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "video/mp4"
    );
    let expected = response.content_length();
    drop(response);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dest = std::env::temp_dir().join(format!("rusaint-video-{unique}.mp4"));
    let bytes = client.download(&content_id, &dest).await.unwrap();
    let mut file = std::fs::File::open(&dest).unwrap();
    assert_eq!(file.metadata().unwrap().len(), bytes);
    if let Some(expected) = expected {
        assert_eq!(bytes, expected);
    }
    let mut head = [0u8; 12];
    file.read_exact(&mut head).unwrap();
    assert_eq!(&head[4..8], b"ftyp");
    assert!(!std::path::PathBuf::from(format!("{}.part", dest.display())).exists());
    println!("영상 전체 다운로드 검증 완료: {bytes} bytes, MP4, .part 파일 없음");
    drop(file);
    std::fs::remove_file(dest).unwrap();
}

/// LMS_VIDEO_CONTENT_ID 영상의 앞부분만 받아 대용량 전체 다운로드 없이 검증합니다.
#[tokio::test]
#[ignore]
async fn open_video() {
    let content_id = std::env::var("LMS_VIDEO_CONTENT_ID").expect("LMS_VIDEO_CONTENT_ID");
    let client = LmsClient::new(rusaint_sso::SsoCredentials::new("", "")).unwrap();
    let mut response = client.open_material(&content_id).await.unwrap();
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "video/mp4"
    );
    let mut head = Vec::new();
    while head.len() < 12 {
        head.extend_from_slice(&response.chunk().await.unwrap().expect("video bytes"));
    }
    assert_eq!(&head[4..8], b"ftyp");
}

#[tokio::test]
#[ignore]
async fn login_and_read() {
    let client = LmsClient::new(rusaint_sso::SsoCredentials::new(
        std::env::var("SSO_ID").unwrap_or_default(),
        std::env::var("SSO_PASSWORD").unwrap_or_default(),
    ))
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

#[tokio::test]
#[ignore]
async fn announcements_and_boards() {
    let client = LmsClient::new(rusaint_sso::SsoCredentials::new(
        std::env::var("SSO_ID").unwrap_or_default(),
        std::env::var("SSO_PASSWORD").unwrap_or_default(),
    ))
    .unwrap();
    let course = 49096; // HCI
    let announcements = client.announcements(course).await.unwrap();
    println!("announcements: {}", announcements.len());
    for a in announcements.iter().take(3) {
        println!(
            "  [{}] {} ({:?}, {:?})",
            a.id, a.title, a.posted_at, a.read_state
        );
    }
    if let Some(first) = announcements.first() {
        let full = client.announcement(course, first.id).await.unwrap();
        let body = full.message.unwrap_or_default();
        println!(
            "  body {} chars: {}",
            body.chars().count(),
            body.chars()
                .take(80)
                .collect::<String>()
                .replace('\n', " / ")
        );
    }
    let boards = client.boards(course).await.unwrap();
    for b in &boards {
        println!(
            "board [{}] {} {:?} posts={:?} unread={:?}",
            b.id, b.title, b.kind, b.total_post_count, b.unread_post_count
        );
    }
    let qna = boards
        .iter()
        .find(|b| b.total_post_count.unwrap_or(0) > 0)
        .unwrap();
    let page = client.posts(course, qna.id, 1).await.unwrap();
    println!(
        "posts page: {:?}",
        page.pagination
            .as_ref()
            .map(|p| (p.total, p.current_page, p.last_page))
    );
    let post = &page.items[0];
    println!(
        "  post [{}] {} comments={:?} content={:?}",
        post.id,
        post.title,
        post.comment_count,
        post.content
            .as_deref()
            .map(|c| c.chars().take(60).collect::<String>())
    );
    let comments = client.comments(course, qna.id, post.id).await.unwrap();
    println!("  comments: {}", comments.len());
}
