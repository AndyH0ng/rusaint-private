//! 여러 서비스(u-saint, LMS, SSU-PATH)가 함께 쓰는 SmartID 통합 계정입니다.
//!
//! SmartID 는 비밀번호를 여러 번 틀리면 계정을 잠급니다. 그래서 로그인 시도는 이 객체 하나를 거쳐
//! 한 번에 하나씩만 보내고, 계정 정보가 한 번 거부되면 어느 서비스에서도 다시 보내지 않습니다.

use std::sync::Arc;

use tokio::sync::Mutex;

#[derive(Debug, Clone, thiserror::Error)]
pub enum SsoError {
    /// 계정 정보가 비어 있거나 SmartID 가 거부함. 프로세스를 다시 시작하기 전까지 재시도하지 않습니다.
    #[error("통합 로그인 계정 정보를 사용할 수 없습니다: {0}")]
    CredentialUnusable(String),
    /// 네트워크 오류, 로그인 화면 변경 등 계정 정보와 무관한 실패
    #[error("SmartID 로그인 실패: {0}")]
    Failed(String),
}

/// 공유 통합 계정
pub struct SsoCredentials {
    student_id: String,
    password: String,
    /// 거부 사유. 잠금 겸 로그인 직렬화에 씁니다.
    rejected: Mutex<Option<String>>,
}

impl SsoCredentials {
    pub fn new(student_id: impl Into<String>, password: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            student_id: student_id.into(),
            password: password.into(),
            rejected: Mutex::new(None),
        })
    }

    pub fn student_id(&self) -> &str {
        &self.student_id
    }

    /// 새 SmartID `sToken` 을 받습니다. 동시에 들어온 요청은 차례로 처리됩니다.
    pub async fn token(&self) -> Result<String, SsoError> {
        let mut rejected = self.rejected.lock().await;
        if let Some(reason) = rejected.as_ref() {
            return Err(SsoError::CredentialUnusable(reason.clone()));
        }
        if self.student_id.trim().is_empty() || self.password.is_empty() {
            return Err(SsoError::CredentialUnusable(
                "학번 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        match rusaint::obtain_ssu_sso_token(&self.student_id, &self.password).await {
            Ok(token) => Ok(token),
            Err(rusaint::SsuSsoError::CantFindToken(message)) => {
                // SmartID 가 경고창으로 거부한 경우(비밀번호 오류 등)
                *rejected = Some(message.clone());
                Err(SsoError::CredentialUnusable(message))
            }
            Err(e) => Err(SsoError::Failed(e.to_string())),
        }
    }

    /// 이미 거부된 상태인지
    pub async fn rejection(&self) -> Option<String> {
        self.rejected.lock().await.clone()
    }

    #[cfg(test)]
    async fn mark_rejected_for_test(&self, reason: &str) {
        *self.rejected.lock().await = Some(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_password_is_never_sent() {
        let sso = SsoCredentials::new("20240000", "");
        assert!(matches!(
            sso.token().await,
            Err(SsoError::CredentialUnusable(_))
        ));
    }

    #[tokio::test]
    async fn rejection_is_shared() {
        let sso = SsoCredentials::new("20240000", "pw");
        let other_service = sso.clone();
        sso.mark_rejected_for_test("ID or password is not correct")
            .await;
        assert!(matches!(
            other_service.token().await,
            Err(SsoError::CredentialUnusable(_))
        ));
        assert!(other_service.rejection().await.is_some());
    }
}
