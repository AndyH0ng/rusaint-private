use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// 목록 응답 (`data.list`)
#[derive(Deserialize)]
pub(crate) struct Page<T> {
    #[serde(default = "Vec::new")]
    pub list: Vec<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdName {
    pub id: u64,
    pub name: String,
}

/// 열람실 좌석 집계
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatCounts {
    pub total: u32,
    pub occupied: u32,
    pub waiting: u32,
    pub available: u32,
    pub unavailable: u32,
}

/// 열람실
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatRoom {
    pub id: u64,
    pub name: String,
    pub room_type: IdName,
    pub floor: Option<i32>,
    pub is_chargeable: bool,
    /// 이용 불가 사유 (운영 시간 외 등)
    pub unable_message: Option<String>,
    pub seats: SeatCounts,
}

/// 열람실 좌석
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seat {
    pub id: u64,
    /// 좌석 번호
    pub code: String,
    pub is_active: bool,
    pub is_reservable: bool,
    pub is_occupied: bool,
    pub seat_charge_state: Option<String>,
    /// 이용 중인 좌석의 남은 시간(분)
    pub remaining_time: Option<i64>,
    pub seat_type: Option<IdName>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloorLabel {
    pub value: i32,
    pub label: String,
}

/// 스터디룸 이용 규칙
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomRule {
    /// 예약 단위 (`H`: 1시간)
    pub time_unit: String,
    pub use_companion_registration: bool,
    /// 최소 이용 시간(분)
    pub min_time: u32,
    /// 최대 이용 시간(분)
    pub max_time: u32,
}

#[derive(Deserialize)]
pub(crate) struct TimeLineHour {
    hour: u32,
    minutes: Vec<TimeLineSlot>,
}

#[derive(Deserialize)]
pub(crate) struct TimeLineSlot {
    #[serde(default)]
    class: String,
    #[serde(default)]
    selectable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawStudyRoom {
    id: u64,
    name: String,
    floor: Option<FloorLabel>,
    min_quota: u32,
    max_quota: u32,
    hope_date: Option<NaiveDate>,
    is_chargeable: bool,
    #[serde(default)]
    time_line: Vec<TimeLineHour>,
    rule: Option<RoomRule>,
    #[serde(default)]
    reservable_dates: Vec<NaiveDate>,
    attention: Option<String>,
    note: Option<String>,
}

/// 스터디룸과 그날의 빈 시간대
#[derive(Debug, Clone, Serialize)]
pub struct StudyRoom {
    pub id: u64,
    pub name: String,
    pub floor: Option<String>,
    /// 최소·최대 이용 인원(본인 포함)
    pub min_quota: u32,
    pub max_quota: u32,
    pub date: Option<NaiveDate>,
    pub is_chargeable: bool,
    /// 비어 있는 시간대 (`"13:00-15:00"`)
    pub free_ranges: Vec<String>,
    /// 예약을 시작할 수 있는 시각 (`"13:00"`). 이용 단위가 1시간이면 정각만 나옵니다.
    pub start_times: Vec<String>,
    pub rule: Option<RoomRule>,
    pub reservable_dates: Vec<NaiveDate>,
    pub attention: Option<String>,
    pub note: Option<String>,
}

/// 시간표 한 칸의 길이(분). 사이트는 1시간을 6칸으로 그립니다.
const SLOT_MINUTES: u32 = 10;

impl From<RawStudyRoom> for StudyRoom {
    fn from(raw: RawStudyRoom) -> Self {
        Self {
            id: raw.id,
            name: raw.name,
            floor: raw.floor.map(|f| f.label),
            min_quota: raw.min_quota,
            max_quota: raw.max_quota,
            date: raw.hope_date,
            is_chargeable: raw.is_chargeable,
            free_ranges: free_ranges(&raw.time_line),
            start_times: start_times(&raw.time_line),
            rule: raw.rule,
            reservable_dates: raw.reservable_dates,
            attention: raw.attention,
            note: raw.note,
        }
    }
}

fn hm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// 시간표의 칸을 (시작 분, 칸) 순서로 펼칩니다.
fn slots(time_line: &[TimeLineHour]) -> impl Iterator<Item = (u32, &TimeLineSlot)> {
    time_line.iter().flat_map(|hour| {
        hour.minutes
            .iter()
            .enumerate()
            .map(move |(i, slot)| (hour.hour * 60 + i as u32 * SLOT_MINUTES, slot))
    })
}

/// `class`가 빈 칸(예약·지난 시간·운영 종료가 아닌 칸)을 연속 구간으로 합칩니다.
/// `selectable`은 시작 가능 시각 표시라 빈 칸 판단에 쓰지 않습니다.
pub(crate) fn free_ranges(time_line: &[TimeLineHour]) -> Vec<String> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for (start, slot) in slots(time_line) {
        if !slot.class.is_empty() {
            continue;
        }
        match ranges.last_mut() {
            Some((_, end)) if *end == start => *end += SLOT_MINUTES,
            _ => ranges.push((start, start + SLOT_MINUTES)),
        }
    }
    ranges
        .into_iter()
        .map(|(s, e)| format!("{}-{}", hm(s), hm(e)))
        .collect()
}

/// 사이트와 같은 기준(`class`가 비어 있고 `selectable`)으로 시작 가능한 시각을 고릅니다.
pub(crate) fn start_times(time_line: &[TimeLineHour]) -> Vec<String> {
    slots(time_line)
        .filter(|(_, slot)| slot.class.is_empty() && slot.selectable)
        .map(|(start, _)| hm(start))
        .collect()
}

/// 내 좌석 또는 스터디룸 이용·예약 건. 화면에 필요한 필드 외에는 원본 그대로 `extra`에 담습니다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Charge {
    pub id: u64,
    pub room: Option<IdName>,
    pub begin_time: Option<String>,
    pub end_time: Option<String>,
    pub is_renewable: Option<bool>,
    pub is_returnable: Option<bool>,
    pub is_checkinable: Option<bool>,
    /// 연장·입실 전 도착 확인 방식 (`GATE`, `GPS`, `BEACON`, `RF_TAG`). 없으면 확인 불필요
    pub arrival_confirm_methods: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Charge {
    pub fn room_id(&self) -> Option<u64> {
        self.room.as_ref().map(|r| r.id)
    }
}

/// 스터디룸 예약 요청
#[derive(Debug, Clone)]
pub struct RoomReservation {
    pub room_id: u64,
    pub date: NaiveDate,
    pub begin: NaiveTime,
    pub end: NaiveTime,
    /// [`crate::LibraryClient::check_companion`]으로 확인한 동반 이용자 id (본인 제외)
    pub companion_ids: Vec<u64>,
    pub message: Option<String>,
}

impl RoomReservation {
    pub(crate) fn to_body(&self, use_section_id: u64) -> Value {
        let at = |t: NaiveTime| format!("{} {}", self.date, t.format("%H:%M"));
        json!({
            "roomId": self.room_id,
            "roomUseSectionId": use_section_id,
            "beginTime": at(self.begin),
            "endTime": at(self.end),
            "companionCnt": self.companion_ids.len(),
            "companionPatrons": self.companion_ids,
            "roomChargeOutsiders": [],
            "roomChargeEquipments": [],
            "patronMessage": self.message,
            "smufMethodCode": "PC",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(class: &str, selectable: bool) -> TimeLineSlot {
        TimeLineSlot {
            class: class.into(),
            selectable,
        }
    }

    /// 실측 형태: 정각 칸만 selectable, 나머지 칸은 class 로만 상태를 나타냄
    fn hour(hour: u32, class: &str) -> TimeLineHour {
        let mut minutes = vec![slot(class, true)];
        minutes.extend((0..5).map(|_| slot(class, false)));
        TimeLineHour { hour, minutes }
    }

    #[test]
    fn free_ranges_use_class_only() {
        let time_line = vec![
            hour(11, "disabled"),
            hour(12, ""),
            hour(13, ""),
            hour(14, "reserved"),
            hour(15, ""),
        ];
        assert_eq!(free_ranges(&time_line), ["12:00-14:00", "15:00-16:00"]);
        assert_eq!(start_times(&time_line), ["12:00", "13:00", "15:00"]);
    }

    #[test]
    fn real_timeline_fixture() {
        let page: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/rooms.json")).unwrap();
        let room: RawStudyRoom = serde_json::from_value(page["data"]["list"][0].clone()).unwrap();
        // 세미나룸1, 2026-11-02 10:56 기준: 12~17시, 20~21시 비어 있음
        assert_eq!(free_ranges(&room.time_line), ["12:00-17:00", "20:00-21:00"]);
        assert_eq!(
            start_times(&room.time_line),
            ["12:00", "13:00", "14:00", "15:00", "16:00", "20:00"]
        );
    }

    #[test]
    fn reservation_body_matches_site_format() {
        let request = RoomReservation {
            room_id: 7,
            date: NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
            begin: NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(15, 0, 0).unwrap(),
            companion_ids: vec![11, 12],
            message: None,
        };
        let body = request.to_body(1);
        assert_eq!(body["beginTime"], "2026-11-03 13:00");
        assert_eq!(body["endTime"], "2026-11-03 15:00");
        assert_eq!(body["companionCnt"], 2);
        assert_eq!(body["smufMethodCode"], "PC");
    }
}
