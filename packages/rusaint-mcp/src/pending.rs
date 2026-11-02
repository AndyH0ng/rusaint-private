//! 쓰기 작업의 2단계 승인(prepare → confirm)을 위한 대기 목록입니다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusaint_library::RoomReservation;
use tokio::sync::Mutex;

/// 준비한 작업은 이 시간 안에 확인해야 합니다.
pub const TTL: Duration = Duration::from_secs(5 * 60);

/// 확인을 기다리는 도서관 쓰기 작업
#[derive(Debug, Clone)]
pub enum Action {
    ReserveSeat { seat_id: u64 },
    ReturnSeat { charge_id: u64 },
    CancelSeat { charge_id: u64 },
    RenewSeat { charge_id: u64 },
    ReserveRoom(RoomReservation),
    CancelRoom { charge_id: u64 },
    RenewRoom { charge_id: u64 },
}

struct Entry {
    action: Action,
    summary: String,
    created: Instant,
}

/// 1회용 작업 id 를 발급하고 보관합니다.
pub struct Pending {
    entries: Mutex<HashMap<String, Entry>>,
    counter: AtomicU64,
}

impl Pending {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
        Self {
            entries: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(seed),
        }
    }

    /// 작업을 등록하고 id 를 돌려줍니다. 만료된 작업은 이때 정리합니다.
    pub async fn insert(&self, action: Action, summary: String) -> String {
        let n = self
            .counter
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        let id = format!("act-{:06x}", (n ^ (n >> 29)) & 0xFF_FFFF);
        let mut entries = self.entries.lock().await;
        entries.retain(|_, e| e.created.elapsed() < TTL);
        entries.insert(
            id.clone(),
            Entry {
                action,
                summary,
                created: Instant::now(),
            },
        );
        id
    }

    /// 작업을 꺼냅니다. 한 번 꺼낸 id 는 다시 쓸 수 없습니다.
    pub async fn take(&self, id: &str) -> Result<(Action, String), String> {
        let entry = self
            .entries
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| format!("{id}: 없는 작업이거나 이미 처리되었습니다"))?;
        if entry.created.elapsed() >= TTL {
            return Err(format!(
                "{id}: 5분이 지나 만료되었습니다. 다시 준비해 주세요"
            ));
        }
        Ok((entry.action, entry.summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ids_are_single_use() {
        let pending = Pending::new();
        let a = pending
            .insert(Action::ReturnSeat { charge_id: 1 }, "반납".into())
            .await;
        let b = pending
            .insert(Action::ReturnSeat { charge_id: 2 }, "반납".into())
            .await;
        assert_ne!(a, b);
        assert!(pending.take(&a).await.is_ok());
        assert!(pending.take(&a).await.is_err());
        assert!(pending.take(&b).await.is_ok());
    }
}
