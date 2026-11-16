use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

fn fixture() -> (PathBuf, Arc<Job>) {
    let dir = std::env::temp_dir().join(format!(
        "rusaint-pay-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&dir).unwrap();
    let stored = Stored {
        owner: "20240000".into(),
        id: uuid::Uuid::new_v4().simple().to_string(),
        request: OrderRequest {
            kind: rusaint_certi::payment::Kind::Enrollment,
            start: None,
            end: None,
            receiver: "기관".into(),
            purpose: "제출".into(),
        },
        order: Some(PreparedOrder {
            fields: Fields::from([
                ("SERVICE_TYPE".into(), "INTERNET".into()),
                ("LGD_OID".into(), "school-order".into()),
                ("LGD_MID".into(), "school-mid".into()),
                ("LGD_AMOUNT".into(), "1000".into()),
                ("LGD_HASHDATA".into(), "signed-by-school".into()),
            ]),
            amount: 1000,
            name: "재학증명서".into(),
            document_numbers: vec!["owned-document".into()],
            before: vec![],
        }),
        stage: Stage::Ready,
        approval_attempted: false,
        created: 0,
        continuation: false,
        document_id: None,
    };
    let path = dir.join("order.json");
    stored.save(&path).unwrap();
    let client = Arc::new(CertiClient::new(
        rusaint_sso::SsoCredentials::from_token("20240000", "not-used"),
        "192.0.2.1".parse().unwrap(),
    ));
    let job = Arc::new(Job {
        path,
        data: Mutex::new(stored),
        operation: Mutex::new(()),
        contact: Mutex::new(None),
        client,
        files: FileLinks::new("https://mcp.example.com"),
        born: Instant::now(),
    });
    (dir, job)
}
fn success() -> Vec<(String, String)> {
    vec![
        ("LGD_RESPCODE".into(), "0000".into()),
        ("LGD_PAYKEY".into(), "private-paykey".into()),
        ("LGD_OID".into(), "school-order".into()),
        ("LGD_AMOUNT".into(), "1000".into()),
        ("LGD_MID".into(), "school-mid".into()),
    ]
}

#[tokio::test]
async fn simultaneous_replayed_callbacks_claim_approval_once_and_persist_before_returning() {
    let (dir, job) = fixture();
    job.open(
        "01012345678".into(),
        "https://mcp.example.com/return".into(),
    )
    .await
    .unwrap();
    let mut handles = Vec::new();
    for _ in 0..12 {
        let job = job.clone();
        handles.push(tokio::spawn(async move {
            job.accept(success()).await.unwrap().is_some()
        }));
    }
    let mut claims = 0;
    for handle in handles {
        if handle.await.unwrap() {
            claims += 1;
        }
    }
    assert_eq!(claims, 1);
    let saved = Stored::load(&job.path, "20240000").unwrap();
    assert!(saved.approval_attempted);
    assert_eq!(saved.stage, Stage::ApprovalPending);
    let raw = std::fs::read_to_string(&job.path).unwrap();
    assert!(!raw.contains("01012345678"));
    assert!(!raw.contains("private-paykey"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn forged_order_amount_mid_and_duplicate_fields_never_claim_approval() {
    for key in ["LGD_OID", "LGD_AMOUNT", "LGD_MID"] {
        let (dir, job) = fixture();
        job.open(
            "01012345678".into(),
            "https://mcp.example.com/return".into(),
        )
        .await
        .unwrap();
        let mut result = success();
        result.iter_mut().find(|(k, _)| k == key).unwrap().1 = "forged".into();
        assert!(job.accept(result).await.is_err());
        assert!(!job.data.lock().await.approval_attempted);
        let mut duplicate = success();
        duplicate.push(("LGD_PAYKEY".into(), "another-key".into()));
        assert!(job.accept(duplicate).await.is_err());
        assert!(!job.data.lock().await.approval_attempted);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn cancel_does_not_approve_and_discards_contact() {
    let (dir, job) = fixture();
    job.open(
        "01012345678".into(),
        "https://mcp.example.com/return".into(),
    )
    .await
    .unwrap();
    assert!(
        job.accept(vec![("LGD_RESPCODE".into(), "S053".into())])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(job.data.lock().await.stage, Stage::Cancelled);
    assert!(job.contact.lock().await.is_none());
    assert!(
        !Stored::load(&job.path, "20240000")
            .unwrap()
            .approval_attempted
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn attempted_approval_cannot_be_reopened_after_restart() {
    let (dir, job) = fixture();
    job.open(
        "01012345678".into(),
        "https://mcp.example.com/return".into(),
    )
    .await
    .unwrap();
    job.accept(success()).await.unwrap();
    let saved = Stored::load(&job.path, "20240000").unwrap();
    let restarted = Job {
        path: job.path.clone(),
        data: Mutex::new(saved),
        operation: Mutex::new(()),
        contact: Mutex::new(None),
        client: job.client.clone(),
        files: job.files.clone(),
        born: Instant::now(),
    };
    assert!(restarted.accept(success()).await.unwrap().is_none());
    assert!(
        restarted
            .open(
                "01012345678".into(),
                "https://mcp.example.com/return".into()
            )
            .await
            .is_err()
    );
    assert!(Stored::load(&job.path, "another-owner").is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn only_pg_transfer_page_allows_toss_and_unknown_tickets_do_nothing() {
    let (dir, job) = fixture();
    let payments = Payments::new("https://mcp.example.com", job.files.clone());
    let ticket = payments.ticket(job.clone()).await;
    let app = router::<()>(payments);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/certificate-pay/{ticket}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("tosspayments")
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/certificate-pay/{ticket}/start"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("phone=01012345678"))
                .unwrap(),
        )
        .await
        .unwrap();
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("form-action https://checkout.tosspayments.com;"));
    assert!(!csp.contains("unsafe-inline"));
    let body = String::from_utf8(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("xpay/Request.do"));
    assert!(!body.contains("sToken"));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/certificate-pay/{ticket}/status"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("tosspayments")
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/certificate-pay/not-a-ticket/return")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("LGD_RESPCODE=0000&LGD_PAYKEY=forged"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!job.data.lock().await.approval_attempted);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn approval_evidence_removes_contact_without_removing_order_id() {
    let raw=br#"<form onload="document.forms[0].submit()"><input name="USERMOBILE2" value="1234"><input value='5678' name='USERMOBILE3'><input name=BUYERTEL value=01012345678><input name=LGD_OID value=school-order></form><script>USERMOBILE1='010';const phone='010-1234-5678';</script>"#;
    assert!(needs_continuation(raw));
    let cleaned = String::from_utf8(scrub_contact(raw, Some("01012345678"))).unwrap();
    for secret in ["1234", "5678", "01012345678", "'010'"] {
        assert!(!cleaned.contains(secret), "{cleaned}");
    }
    assert!(cleaned.contains("school-order"));
}

#[tokio::test]
async fn file_products_and_prices_above_the_users_limit_are_rejected() {
    let (dir, job) = fixture();
    let mut order = job.data.lock().await.order.clone().unwrap();
    assert!(validate_order(&order, 1000).is_ok());
    assert!(validate_order(&order, 999).is_err());
    order.fields.insert("SERVICE_TYPE".into(), "FILE".into());
    assert!(validate_order(&order, 1000).is_err());
    assert!(!job.data.lock().await.approval_attempted);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cancellation_revokes_an_open_checkout_but_cannot_reset_attempted_approval() {
    let (dir, job) = fixture();
    job.open(
        "01012345678".into(),
        "https://mcp.example.com/return".into(),
    )
    .await
    .unwrap();
    job.cancel().await.unwrap();
    assert!(job.accept(success()).await.unwrap().is_none());
    assert!(job.contact.lock().await.is_none());
    assert!(!job.data.lock().await.approval_attempted);
    std::fs::remove_dir_all(dir).unwrap();
    let (dir, job) = fixture();
    job.open(
        "01012345678".into(),
        "https://mcp.example.com/return".into(),
    )
    .await
    .unwrap();
    job.accept(success()).await.unwrap();
    assert!(job.cancel().await.is_err());
    assert_eq!(job.data.lock().await.stage, Stage::ApprovalPending);
    std::fs::remove_dir_all(dir).unwrap();
}
