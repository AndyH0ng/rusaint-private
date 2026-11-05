//! 채플 결석 가능 횟수와 유고결석 현황. 기준 비율은 규정집 본문에서 읽은 값(`ChapelRules`)을 씁니다.

use std::collections::BTreeMap;

use rusaint::application::chapel::model::ChapelInformation;
use rusaint_regulation::ChapelRules;
use serde::Serialize;

/// 날짜 문자열("2026.03.10", "2026-03-10" 등)의 숫자 8자리를 yyyymmdd 로 읽습니다.
fn day(s: &str) -> Option<u32> {
    let digits: String = s.chars().filter(char::is_ascii_digit).take(8).collect();
    (digits.len() == 8).then(|| digits.parse().ok()).flatten()
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Approved,
    Rejected,
    Pending,
}

fn approval(status: &str) -> Approval {
    let s = status.replace(' ', "");
    if s.contains("반려") || s.contains("불승인") || s.contains("미승인") || s.contains("거절")
    {
        Approval::Rejected
    } else if s.contains("승인") || s.contains("완료") {
        Approval::Approved
    } else {
        Approval::Pending
    }
}

#[derive(Debug, Serialize)]
pub struct Request {
    pub reason: String,
    pub detail: String,
    pub start: String,
    pub end: String,
    pub status: String,
    pub approval: Approval,
    /// 보건결석은 채플 출석으로 인정되지 않습니다(내규 제7조).
    pub health: bool,
    /// 출결표 회차 중 이 신청 기간에 들어가는 횟수
    pub sessions_covered: u32,
    /// 승인됐고 보건결석이 아니어서 채플 출석으로 인정될 수 있는지
    pub counts_for_chapel: bool,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    /// 계산에 쓴 학기 총 회차
    pub total_sessions: u32,
    pub total_source: &'static str,
    /// u-saint 출결표에 나온 회차 수
    pub sessions_listed: u32,
    /// u-saint 기본 정보의 결석일수 (학교 집계)
    pub absences: u32,
    /// 출결표의 출결상태별 횟수
    pub attendance_counts: BTreeMap<String, u32>,
    /// 이 횟수만큼 결석하면 F (규정 비율 "이상")
    pub fail_at: Option<u32>,
    /// F 없이 할 수 있는 최대 결석
    pub max_absences: Option<u32>,
    /// 앞으로 더 결석해도 되는 횟수 (음수면 이미 기준 초과)
    pub remaining_absences: Option<i64>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct Excused {
    pub requests: Vec<Request>,
    /// 승인돼 채플 출석으로 인정될 수 있는 회차 (보건결석 제외)
    pub approved_sessions: u32,
    /// 유고결석 승인 상한 (총 회차 기준)
    pub cap: Option<u32>,
    /// 상한까지 더 승인받을 수 있는 회차 (보건결석 포함 모든 유고결석 기준)
    pub remaining_cap: Option<i64>,
}

pub fn summarize(
    info: &ChapelInformation,
    rules: Option<&ChapelRules>,
    total_override: Option<u32>,
) -> (Summary, Excused) {
    let listed = info.attendances().len() as u32;
    let (total, total_source) = match total_override {
        Some(n) if n > 0 => (n, "사용자가 알려 준 학기 총 회차"),
        _ => (listed, "u-saint 출결표에 나온 회차 수"),
    };
    let absences = info.general_information().absence_time();
    let mut attendance_counts = BTreeMap::new();
    for a in info.attendances() {
        let key = match a.attendance().trim() {
            "" => "(미기록)".to_string(),
            s => s.to_string(),
        };
        *attendance_counts.entry(key).or_insert(0) += 1;
    }

    let fail_ratio = rules.and_then(|r| r.fail_ratio);
    let fail_at = fail_ratio.filter(|_| total > 0).map(|r| r.at_least(total));
    let max_absences = fail_at.map(|n| n.saturating_sub(1));
    let remaining = max_absences.map(|m| m as i64 - absences as i64);
    let status = match (remaining, fail_at) {
        (Some(r), _) if r < 0 => "F 기준 도달 (규정상 F 대상)".to_string(),
        (Some(0), _) => "주의: 더 결석하면 F".to_string(),
        (Some(r), _) => format!("안전: {r}회 더 결석 가능"),
        (None, _) if total == 0 => "출결표가 비어 있어 계산하지 못함".to_string(),
        _ => "규정에서 결석 기준을 읽지 못함 (조문 원문 참고)".to_string(),
    };

    let health_not_excused = rules.is_none_or(|r| r.health_not_excused);
    let class_days: Vec<u32> = info
        .attendances()
        .iter()
        .filter_map(|a| day(a.class_date()))
        .collect();
    let requests: Vec<Request> = info
        .absence_requests()
        .iter()
        .filter(|r| r.year() == info.year() && r.semester() == info.semester())
        .map(|r| {
            let health =
                r.absence_reason_kr().contains("보건") || r.absence_detail().contains("보건");
            let covered = match (day(r.absence_start()), day(r.absence_end())) {
                (Some(s), Some(e)) => class_days.iter().filter(|d| (s..=e).contains(d)).count(),
                (Some(s), None) => class_days.iter().filter(|d| **d == s).count(),
                _ => 0,
            } as u32;
            let approval = approval(r.status());
            let counts = approval == Approval::Approved && !(health && health_not_excused);
            Request {
                reason: r.absence_reason_kr().to_string(),
                detail: r.absence_detail().to_string(),
                start: r.absence_start().to_string(),
                end: r.absence_end().to_string(),
                status: r.status().to_string(),
                approval,
                health,
                sessions_covered: covered,
                counts_for_chapel: counts,
            }
        })
        .collect();
    let approved_sessions = requests
        .iter()
        .filter(|r| r.counts_for_chapel)
        .map(|r| r.sessions_covered)
        .sum();
    let approved_any: u32 = requests
        .iter()
        .filter(|r| r.approval == Approval::Approved)
        .map(|r| r.sessions_covered)
        .sum();
    let cap = rules
        .and_then(|r| r.excused_cap)
        .filter(|_| total > 0)
        .map(|r| r.at_most(total));
    let remaining_cap = cap.map(|c| c as i64 - approved_any as i64);

    (
        Summary {
            total_sessions: total,
            total_source,
            sessions_listed: listed,
            absences,
            attendance_counts,
            fail_at,
            max_absences,
            remaining_absences: remaining,
            status,
        },
        Excused {
            requests,
            approved_sessions,
            cap,
            remaining_cap,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_approvals() {
        assert_eq!(day("2026.03.10"), Some(20260310));
        assert_eq!(day("2026-03-10 10:30"), Some(20260310));
        assert_eq!(day(""), None);
        assert_eq!(approval("승인"), Approval::Approved);
        assert_eq!(approval("승인완료"), Approval::Approved);
        assert_eq!(approval("미승인"), Approval::Rejected);
        assert_eq!(approval("반려"), Approval::Rejected);
        assert_eq!(approval("신청"), Approval::Pending);
    }
}
