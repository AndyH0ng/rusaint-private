//! u-saint 강의시간표 검색과, 졸업사정표의 부족한 요건에 맞춘 강의 추천.

use std::collections::BTreeSet;

use rusaint::application::course_schedule::CourseScheduleApplication;
use rusaint::application::course_schedule::model::{Lecture, LectureCategory};
use rusaint::application::graduation_requirements::model::GraduationRequirements;
use rusaint::model::SemesterType;
use serde::Serialize;

use crate::usaint;

/// 검색 결과로 내보낼 강의 정보
#[derive(Debug, Serialize)]
pub struct Course {
    pub code: String,
    pub name: String,
    pub division: Option<String>,
    pub professor: String,
    /// 이수구분(주전공), 예: 전필-글로벌미디어
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// 시간/학점(설계)
    pub hours_credits: String,
    pub enrolled: String,
    pub seats_left: String,
    /// 강의시간(강의실)
    pub schedule: String,
    pub target: String,
    pub department: String,
}

impl From<Lecture> for Course {
    fn from(l: Lecture) -> Self {
        Self {
            code: l.code,
            name: l.name,
            division: l.division,
            professor: l.professor,
            category: l.category,
            sub_category: l.sub_category,
            field: l.field,
            hours_credits: l.time_points,
            enrolled: l.personeel,
            seats_left: l.remaining_seats,
            schedule: l.schedule_room,
            target: l.target,
            department: l.department,
        }
    }
}

/// 검색 종류
pub enum Kind {
    Lecture(String),
    Professor(String),
    Major {
        collage: String,
        department: String,
        major: Option<String>,
    },
    RequiredElective(String),
    OptionalElective(String),
    Chapel(String),
    ConnectedMajor(String),
    UnitedMajor(String),
    Education,
    /// 내 전공에서 인정하는 타전공 과목
    RecognizedOtherMajor {
        collage: String,
        department: String,
        major: Option<String>,
    },
}

impl Kind {
    pub fn category(&self) -> LectureCategory {
        match self {
            Kind::Lecture(k) => LectureCategory::find_by_lecture(k),
            Kind::Professor(k) => LectureCategory::find_by_professor(k),
            Kind::Major {
                collage,
                department,
                major,
            } => LectureCategory::major(collage, department, major.as_deref()),
            Kind::RequiredElective(n) => LectureCategory::required_elective(n),
            Kind::OptionalElective(c) => LectureCategory::optional_elective(c),
            Kind::Chapel(n) => LectureCategory::chapel(n),
            Kind::ConnectedMajor(m) => LectureCategory::connected_major(m),
            Kind::UnitedMajor(m) => LectureCategory::united_major(m),
            Kind::Education => LectureCategory::education(),
            Kind::RecognizedOtherMajor {
                collage,
                department,
                major,
            } => LectureCategory::recognized_other_major(collage, department, major.as_deref()),
        }
    }
}

pub async fn search(
    app: &mut CourseScheduleApplication,
    year: u32,
    semester: SemesterType,
    kind: &Kind,
) -> Result<Vec<Course>, String> {
    Ok(app
        .find_lectures(year, semester, &kind.category())
        .await
        .map_err(usaint::err)?
        .map(Course::from)
        .collect())
}

/// 과목명·교수·강의시간·이수구분에 모든 단어가 들어 있는 강의만 남깁니다. ("월 정보과학관" 처럼 공백으로 여러 조건)
pub fn filter(courses: Vec<Course>, words: &str) -> Vec<Course> {
    let words: Vec<&str> = words.split_whitespace().collect();
    courses
        .into_iter()
        .filter(|c| {
            let hay = format!(
                "{} {} {} {} {}",
                c.name, c.professor, c.schedule, c.category, c.code
            );
            words.iter().all(|w| hay.contains(w))
        })
        .collect()
}

/// 다음 정규 학기. 계절학기는 건너뜁니다.
pub fn next_regular(year: u32, semester: SemesterType) -> (u32, SemesterType) {
    match semester {
        SemesterType::One | SemesterType::Summer => (year, SemesterType::Two),
        SemesterType::Two | SemesterType::Winter => (year + 1, SemesterType::One),
    }
}

/// 시간표의 교양필수 이름 "[인간과성서]인문학과성서" 를 영역과 과목명으로 나눕니다.
pub fn split_area(name: &str) -> (Option<&str>, &str) {
    let name = name.trim();
    match (name.strip_prefix('['), name.find(']')) {
        (Some(_), Some(i)) => (Some(&name[1..i]), &name[i + 1..]),
        _ => (None, name),
    }
}

/// 졸업사정표의 과목명은 10자 안팎에서 잘리고 "(교필)" 같은 머리말이, 시간표의 교양필수는 "[영역]" 이 붙습니다.
fn normalize(name: &str) -> String {
    let mut name = name.trim();
    loop {
        let close = match name.chars().next() {
            Some('(') => ')',
            Some('[') => ']',
            _ => break,
        };
        match name.find(close) {
            Some(i) => name = name[i + close.len_utf8()..].trim_start(),
            None => break,
        }
    }
    name.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 외국인 유학생 전용 교양필수
pub fn is_for_international(name: &str) -> bool {
    ["외국인", "대학한국어", "학문목적한국어"]
        .iter()
        .any(|k| name.contains(k))
}

/// 졸업사정표에 쓰인(이미 들은) 과목명들
pub fn taken(requirements: &GraduationRequirements) -> BTreeSet<String> {
    requirements
        .requirements()
        .values()
        .flat_map(|r| r.lectures())
        .map(|n| normalize(n))
        .filter(|n| !n.is_empty())
        .collect()
}

/// 잘린 이름끼리도 맞도록 앞부분이 같으면 같은 과목으로 봅니다.
pub fn is_taken(taken: &BTreeSet<String>, name: &str) -> bool {
    let name = normalize(name);
    !name.is_empty()
        && taken
            .iter()
            .any(|t| name.starts_with(t.as_str()) || t.starts_with(name.as_str()))
}

/// 전공 강의의 이수구분이 요건 종류와 맞는지. 전공기초는 "전기", 전공은 "전필"/"전선" 입니다.
pub fn matches_major_kind(requirement_category: &str, lecture_category: &str) -> bool {
    match requirement_category {
        "전공기초" => lecture_category.starts_with("전기"),
        "전공" => lecture_category.starts_with("전필") || lecture_category.starts_with("전선"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_names_match() {
        let taken: BTreeSet<String> = [" (교필)비판적사고와학술", "(글미)UX/UI디자인", "HCI"]
            .iter()
            .map(|n| normalize(n))
            .collect();
        assert!(is_taken(&taken, "비판적사고와학술적글쓰기"));
        assert!(is_taken(&taken, "(글미)UX/UI디자인"));
        assert!(is_taken(&taken, "HCI"));
        assert!(!is_taken(&taken, "컴퓨터비전"));
        assert!(!is_taken(&taken, ""));
    }

    #[test]
    fn area_prefixes_are_ignored() {
        let taken: BTreeSet<String> = [" (교필)AI개발과실전", "(교필)컴퓨팅적사고와알"]
            .iter()
            .map(|n| normalize(n))
            .collect();
        assert!(is_taken(&taken, "[SW와AI]AI개발과실전"));
        assert!(is_taken(&taken, "[컴퓨팅적사고]컴퓨팅적사고와알고리즘"));
        assert!(!is_taken(&taken, "[컴퓨팅적사고]컴퓨팅적사고와코딩기초"));
        assert_eq!(
            split_area("[인간과성서]인문학과성서"),
            (Some("인간과성서"), "인문학과성서")
        );
    }

    #[test]
    fn next_semester_skips_seasonal_terms() {
        assert!(matches!(
            next_regular(2026, SemesterType::Two),
            (2027, SemesterType::One)
        ));
        assert!(matches!(
            next_regular(2026, SemesterType::Summer),
            (2026, SemesterType::Two)
        ));
    }

    #[test]
    fn major_kinds() {
        assert!(matches_major_kind("전공기초", "전기-글로벌미디어"));
        assert!(matches_major_kind("전공", "전선-글로벌미디어"));
        assert!(!matches_major_kind("전공", "전기-글로벌미디어"));
    }
}
