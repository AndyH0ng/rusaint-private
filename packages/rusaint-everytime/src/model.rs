//! 에브리타임 강의평 조회 결과 모델

use serde::{Deserialize, Serialize};

/// 강의평 등록 과목 검색 결과 한 건
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LectureHit {
    /// 에타 강의 id (강의평 조회에 사용)
    pub id: String,
    /// 과목명
    pub name: String,
    /// 교수명 (없을 수 있음)
    pub professor: Option<String>,
    /// 평균 평점 (0.0 ~ 5.0, 없을 수 있음)
    pub rate: Option<f64>,
}

/// 강의평 한 건(학생이 직접 작성한 후기)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReviewArticle {
    /// 수강 년도 (없을 수 있음)
    pub year: Option<i64>,
    /// 수강 학기 (1·2 등, 없을 수 있음)
    pub semester: Option<i64>,
    /// 후기 본문 (태그를 제거한 평문)
    pub text: String,
    /// 이 후기가 매긴 평점 (0.0 ~ 5.0, 없을 수 있음)
    pub rate: Option<f64>,
    /// 공감 수 (없을 수 있음)
    pub posvote: Option<i64>,
}

/// 강의평 집계 항목 하나의 선택지(예: "출석: 직접호명")와 응답 수
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReviewStatItem {
    pub text: String,
    pub count: i64,
}

/// 강의평 집계 항목(예: "출석", "시험 횟수")
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReviewStat {
    pub name: Option<String>,
    pub count: i64,
    pub items: Vec<ReviewStatItem>,
}

/// 한 과목의 강의평 모음
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LectureReview {
    pub lecture_id: String,
    /// 평균 평점 (0.0 ~ 5.0, 없을 수 있음)
    pub average: Option<f64>,
    /// 전체 강의평 수
    pub count: i64,
    /// 최신 강의평 본문 (최대 limit 개)
    pub articles: Vec<ReviewArticle>,
    /// 객관식/주관식 집계
    pub statistics: Vec<ReviewStat>,
}
