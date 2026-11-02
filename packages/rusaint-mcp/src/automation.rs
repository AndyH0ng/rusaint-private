//! 자동 연장. MCP 서버 프로세스가 살아 있는 동안만 동작합니다.
//!
//! 학교 서버 부담을 고려해 조회 간격은 60초, 최대 12시간으로 제한하고 동시에 1건만 돌립니다.
//!
//! 빈자리 대기 자동 배정은 넣지 않습니다. PC 배정은 임시배정으로 시작해 20분 안에 게이트를
//! 통과하지 않으면 노쇼가 되고, 좌석 취소(DELETE)는 웹 방화벽에 막혀 원격으로 되돌릴 수 없습니다.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use rusaint_library::{ArrivalCheck, Charge, LibraryClient};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
pub const MAX_RENEW_HOURS: u64 = 12;
/// 같은 예약의 연장을 실패한 뒤 다시 시도하기까지 기다리는 시간
const RENEW_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_LOG: usize = 30;

/// 백그라운드 작업 상태
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub kind: &'static str,
    pub running: bool,
    pub started_at: DateTime<Local>,
    pub until: DateTime<Local>,
    pub checks: u32,
    pub log: Vec<String>,
}

impl JobStatus {
    fn new(kind: &'static str, duration: Duration) -> Self {
        let now = Local::now();
        Self {
            kind,
            running: true,
            started_at: now,
            until: now + chrono::Duration::from_std(duration).unwrap_or_default(),
            checks: 0,
            log: Vec::new(),
        }
    }

    fn push(&mut self, message: impl Into<String>) {
        self.log.push(format!(
            "{} {}",
            Local::now().format("%H:%M"),
            message.into()
        ));
        if self.log.len() > MAX_LOG {
            self.log.remove(0);
        }
    }

    fn finish(&mut self, message: impl Into<String>) {
        self.push(message);
        self.running = false;
    }
}

struct Job {
    status: Arc<Mutex<JobStatus>>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
pub struct Automation {
    renew: Mutex<Option<Job>>,
}

impl Automation {
    /// 내 좌석·스터디룸이 연장 가능해지면 연장합니다.
    pub async fn start_renew(
        &self,
        client: Arc<LibraryClient>,
        hours: u64,
    ) -> Result<JobStatus, String> {
        let mut slot = self.renew.lock().await;
        if slot.as_ref().is_some_and(|j| !j.handle.is_finished()) {
            return Err("자동 연장이 이미 켜져 있습니다".into());
        }
        let duration = Duration::from_secs(hours.clamp(1, MAX_RENEW_HOURS) * 3600);
        let status = Arc::new(Mutex::new(JobStatus::new("auto_renew", duration)));
        status.lock().await.push("자동 연장 시작");
        let handle = tokio::spawn(renew_loop(client, duration, status.clone()));
        let snapshot = status.lock().await.clone();
        *slot = Some(Job { status, handle });
        Ok(snapshot)
    }

    pub async fn stop_renew(&self) -> Result<JobStatus, String> {
        let slot = self.renew.lock().await;
        let job = slot.as_ref().ok_or("진행 중인 작업이 없습니다")?;
        job.handle.abort();
        let mut status = job.status.lock().await;
        if status.running {
            status.finish("사용자가 중지함");
        }
        Ok(status.clone())
    }

    pub async fn renew_status(&self) -> Option<JobStatus> {
        match self.renew.lock().await.as_ref() {
            Some(job) => Some(job.status.lock().await.clone()),
            None => None,
        }
    }
}

async fn renew_loop(client: Arc<LibraryClient>, duration: Duration, status: Arc<Mutex<JobStatus>>) {
    let deadline = tokio::time::Instant::now() + duration;
    let mut last_attempt: HashMap<(char, u64), tokio::time::Instant> = HashMap::new();
    loop {
        status.lock().await.checks += 1;
        let mut charges: Vec<(char, Charge)> = Vec::new();
        match client.my_seats().await {
            Ok(seats) => charges.extend(seats.into_iter().map(|c| ('s', c))),
            Err(e) => status.lock().await.push(format!("좌석 조회 실패: {e}")),
        }
        match client.my_rooms().await {
            Ok(rooms) => {
                for room in rooms {
                    // 연장 가능 여부는 상세 응답에만 있습니다.
                    let detail = client.my_room(room.id).await.ok().flatten();
                    charges.push(('r', detail.unwrap_or(room)));
                }
            }
            Err(e) => status.lock().await.push(format!("스터디룸 조회 실패: {e}")),
        }

        for (kind, charge) in charges {
            if charge.is_renewable != Some(true) {
                continue;
            }
            let key = (kind, charge.id);
            if last_attempt
                .get(&key)
                .is_some_and(|at| at.elapsed() < RENEW_RETRY_AFTER)
            {
                continue;
            }
            last_attempt.insert(key, tokio::time::Instant::now());
            let label = match kind {
                's' => "좌석",
                _ => "스터디룸",
            };
            let message = match client.confirm_arrival(&charge).await {
                Ok(ArrivalCheck::NotRequired | ArrivalCheck::Passed) => {
                    let result = if kind == 's' {
                        client.renew_seat(charge.id).await
                    } else {
                        client.renew_room(charge.id).await
                    };
                    match result {
                        Ok(_) => format!("{label} {} 연장 완료", charge.id),
                        Err(e) => format!("{label} {} 연장 실패: {e}", charge.id),
                    }
                }
                Ok(ArrivalCheck::Failed) => {
                    format!(
                        "{label} {} 도착 확인 실패 (게이트 출입 기록 없음)",
                        charge.id
                    )
                }
                Ok(ArrivalCheck::Unsupported(methods)) => format!(
                    "{label} {} 는 {methods:?} 도착 확인이 필요해 직접 연장해야 합니다",
                    charge.id
                ),
                Err(e) => format!("{label} {} 도착 확인 오류: {e}", charge.id),
            };
            status.lock().await.push(message);
        }

        if tokio::time::Instant::now() + POLL_INTERVAL > deadline {
            return status
                .lock()
                .await
                .finish("설정한 시간이 지나 자동 연장 종료");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
