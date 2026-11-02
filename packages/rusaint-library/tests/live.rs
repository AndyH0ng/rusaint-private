//! 실제 도서관 서버에 요청합니다. 기본으로는 건너뜁니다.
//!
//! ```sh
//! LIBRARY_LOGIN_ID=... LIBRARY_PASSWORD="$(security find-generic-password -s rusaint-library -w)" \
//!   cargo test -p rusaint-library --test live -- --ignored --nocapture
//! ```

use rusaint_library::LibraryClient;

fn client() -> LibraryClient {
    let id = std::env::var("LIBRARY_LOGIN_ID").unwrap_or_default();
    let password = std::env::var("LIBRARY_PASSWORD").unwrap_or_default();
    LibraryClient::new(id, password).unwrap()
}

#[tokio::test]
#[ignore]
async fn public_seat_rooms() {
    let rooms = client().seat_rooms().await.unwrap();
    for room in &rooms {
        println!(
            "{} 잔여 {}/{}",
            room.name, room.seats.available, room.seats.total
        );
    }
    assert!(!rooms.is_empty());
}

/// 읽기 전용 요청만 보냅니다.
#[tokio::test]
#[ignore]
async fn logged_in_reads() {
    let client = client();
    client.login().await.unwrap();
    println!("loans: {}", client.loans().await.unwrap().len());
    println!("my seats: {:?}", client.my_seats().await.unwrap());
    println!("my rooms: {:?}", client.my_rooms().await.unwrap());
    let today = chrono::Local::now().date_naive();
    for room in client.study_rooms(today).await.unwrap() {
        println!(
            "{} {:?} {}~{}명 {:?}",
            room.name, room.floor, room.min_quota, room.max_quota, room.free_ranges
        );
    }
    let first = &client.seat_rooms().await.unwrap()[0];
    let seats = client.seats(first.id).await.unwrap();
    println!(
        "{}: 좌석 {}개, 빈 좌석 {}개",
        first.name,
        seats.len(),
        seats
            .iter()
            .filter(|s| s.is_active && !s.is_occupied)
            .count()
    );
}
