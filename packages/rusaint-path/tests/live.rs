//! 실제 SSU-PATH 에 로그인해 읽기 전용 요청만 보냅니다. 기본으로는 건너뜁니다.
//!
//! ```sh
//! SSO_ID=... SSO_PASSWORD="$(security find-generic-password -s rusaint-sso -w)" \
//!   cargo test -p rusaint-path --test live -- --ignored --nocapture
//! ```

use rusaint_path::{PathClient, ProgramQuery, eligibility};

#[tokio::test]
#[ignore]
async fn list_programs() {
    let client = PathClient::new(rusaint_sso::SsoCredentials::new(
        std::env::var("SSO_ID").unwrap_or_default(),
        std::env::var("SSO_PASSWORD").unwrap_or_default(),
    ));
    let me = client.profile().await.unwrap();
    println!("profile: {me:?}");
    let query = ProgramQuery {
        recruiting_only: true,
        grade: me.grade,
        keyword: None,
        page: 1,
    };
    let page = client.programs(&query).await.unwrap();
    println!(
        "page {}/{} items {}",
        page.page,
        page.last_page,
        page.items.len()
    );
    for p in &page.items {
        println!(
            "  [{:?}] {} | {:?} | 대상 {:?} / {:?} | {:?}/{:?} | 남은 자리 {:?}",
            eligibility(p, &me),
            p.title,
            p.status,
            p.target,
            p.target_identity,
            p.applicants,
            p.capacity,
            p.seats_left
        );
    }
    let detail = client.program_detail(&page.items[0].id).await.unwrap();
    for (k, v) in detail.iter().filter(|(k, _)| {
        [
            "신청대상",
            "학생 신청대상 상세정보",
            "정원/대기자",
            "선정방법",
        ]
        .contains(&k.as_str())
    }) {
        println!("  detail {k}: {v}");
    }
}
