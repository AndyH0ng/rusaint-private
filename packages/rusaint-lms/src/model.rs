//! LearningX API 응답 타입. 필드는 2026-11 실측 응답을 기준으로 하며, 없는 경우가 있는 필드는 `Option` 입니다.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Deserialize)]
pub(crate) struct TermPage {
    #[serde(default)]
    pub enrollment_terms: Vec<Term>,
}

/// 수강 학기
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Term {
    pub id: u64,
    pub name: String,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    /// 현재 학기 여부. 정규 학기와 비정규과정이 함께 true 일 수 있습니다.
    #[serde(default)]
    pub default: bool,
}

/// 수강 강의
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Course {
    pub id: u64,
    pub term_id: Option<u64>,
    pub name: String,
    pub professors: Option<String>,
    pub enrolled_status: Option<String>,
    pub ended: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct TodoPage {
    #[serde(default)]
    pub to_dos: Vec<CourseTodos>,
}

/// 강의 하나의 할 일
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseTodos {
    pub course_id: u64,
    /// 미제출 과제 수, 미시청 영상 수 등 (`total_unsubmitted_assignments` …)
    #[serde(default)]
    pub activities: Map<String, Value>,
    #[serde(default)]
    pub todo_list: Vec<TodoItem>,
}

/// 마감이 있는 할 일 항목
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// `assignment`, `quiz`, `commons` 등
    pub component_type: Option<String>,
    pub title: String,
    pub due_date: Option<String>,
    pub assignment_id: Option<u64>,
}

/// 주차 모듈
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module {
    pub module_id: u64,
    pub title: String,
    pub week_position: Option<i64>,
    #[serde(default)]
    pub module_items: Vec<ModuleItem>,
}

/// 모듈 안의 항목(자료, 출석 영상, 과제, 퀴즈 …)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleItem {
    pub module_item_id: u64,
    pub title: String,
    /// `attendance_item`(commons 자료·영상), `assignment`, `quiz` 등
    pub content_type: String,
    pub completed: Option<bool>,
    #[serde(default)]
    pub content_data: Value,
}

/// 강의 공지 (Canvas announcement)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub id: u64,
    pub title: String,
    pub posted_at: Option<String>,
    pub user_name: Option<String>,
    /// `read` / `unread`
    pub read_state: Option<String>,
    pub html_url: Option<String>,
    /// 본문. 목록에는 없고 상세에서만 텍스트로 채워집니다.
    pub message: Option<String>,
}

/// 과목 게시판 (LearningX board)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: u64,
    pub title: String,
    /// `qna`, `normal` 등
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub total_post_count: Option<u64>,
    pub unread_post_count: Option<u64>,
    pub latest_post_created_at: Option<String>,
}

/// 게시판 글 목록 한 페이지
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostPage {
    #[serde(default)]
    pub items: Vec<Post>,
    pub pagination: Option<Pagination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pagination {
    pub total: u64,
    pub current_page: u32,
    pub last_page: u32,
}

/// 게시글
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: u64,
    pub title: String,
    pub user_name: Option<String>,
    #[serde(default)]
    pub is_notice: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub comment_count: Option<u64>,
    pub attachment_count: Option<u64>,
    pub created_at: Option<String>,
    /// 본문 (텍스트로 변환됨)
    pub content: Option<String>,
}

/// 게시글 댓글
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub depth: Option<u32>,
    pub user_name: Option<String>,
    pub content: Option<String>,
    pub created_at: Option<String>,
    #[serde(default)]
    pub is_deleted: bool,
}

/// commons 에 올라간 강의 자료
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Material {
    pub content_id: String,
    /// `pdf`, `movie` 등
    pub kind: String,
    pub file_name: Option<String>,
    pub size_bytes: Option<u64>,
}

impl ModuleItem {
    /// commons 자료라면 다운로드에 필요한 정보를 돌려줍니다.
    pub fn material(&self) -> Option<Material> {
        let data = self.content_data.get("item_content_data")?;
        Some(Material {
            content_id: data.get("content_id")?.as_str()?.to_string(),
            kind: data
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            file_name: data
                .get("file_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            size_bytes: data
                .get("total_file_size")
                .and_then(Value::as_u64)
                .filter(|&n| n > 0),
        })
    }

    /// 과제·퀴즈·자료의 마감 시각
    pub fn due_at(&self) -> Option<&str> {
        self.content_data.get("due_at").and_then(Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn material_from_attendance_item() {
        let item: ModuleItem = serde_json::from_value(json!({
            "module_item_id": 1,
            "title": "1주차 강의자료",
            "content_type": "attendance_item",
            "completed": false,
            "content_data": {
                "item_content_type": "commons",
                "due_at": "2026-09-08T14:59:59Z",
                "item_content_data": {
                    "content_id": "6a966fdc4caf2",
                    "content_type": "pdf",
                    "file_name": "week1.pdf",
                    "total_file_size": 22710743
                }
            }
        }))
        .unwrap();
        assert_eq!(
            item.material(),
            Some(Material {
                content_id: "6a966fdc4caf2".into(),
                kind: "pdf".into(),
                file_name: Some("week1.pdf".into()),
                size_bytes: Some(22710743),
            })
        );
        assert_eq!(item.due_at(), Some("2026-09-08T14:59:59Z"));
    }

    #[test]
    fn assignment_has_no_material() {
        let item: ModuleItem = serde_json::from_value(json!({
            "module_item_id": 2, "title": "과제 1", "content_type": "assignment",
            "content_data": { "due_at": "2026-10-01T14:59:59Z" }
        }))
        .unwrap();
        assert!(item.material().is_none());
    }
}
