//! 여러 서비스(u-saint, LMS, SSU-PATH)가 함께 쓰는 SmartID 통합 계정입니다.
//!
//! SmartID 는 비밀번호를 여러 번 틀리면 계정을 잠급니다. 그래서 로그인 시도는 이 객체 하나를 거쳐
//! 한 번에 하나씩만 보내고, 계정 정보가 한 번 거부되면 어느 서비스에서도 다시 보내지 않습니다.
//!
//! 비밀번호 대신 사용자가 SmartID 화면에서 직접 로그인해 받은 `sToken` 만 맡길 수도 있습니다
//! ([`SsoCredentials::from_token`]). 이때는 비밀번호를 모르므로 토큰이 만료되면 다시 로그인해야 합니다.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// SmartID 토큰에 기간이 적혀 있지 않을 때 쓰는 유효 기간
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);
/// 만료 직전 토큰으로 새 세션을 열다 실패하지 않도록 남겨 두는 여유
const TOKEN_MARGIN: Duration = Duration::from_secs(10 * 60);

/// 토큰 모드에서 만료됐을 때 보여 주는 안내
pub const TOKEN_EXPIRED: &str = "학교 통합 로그인(SSO)이 만료됐습니다. 이 연결은 비밀번호를 모르기 때문에 다시 로그인해야 합니다. Claude 설정 → 커넥터에서 다시 연결하세요.";

enum Secret {
    Password(String),
    /// SmartID 가 발급한 `sToken` 과 만료 예상 시각
    Token {
        token: String,
        expires: Instant,
    },
}

/// SmartID `sToken` 안의 `Py<초>` 값(예: `zPy86400z`)으로 유효 기간을 읽습니다. 없으면 하루로 봅니다.
fn token_lifetime(token: &str) -> Duration {
    token
        .split('z')
        .find_map(|part| part.strip_prefix("Py")?.parse::<u64>().ok())
        .filter(|s| (60..=7 * 86400).contains(s))
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME)
}

/// 공유 통합 계정
pub struct SsoCredentials {
    student_id: String,
    secret: Secret,
    /// 거부 사유. 잠금 겸 로그인 직렬화에 씁니다.
    rejected: Mutex<Option<String>>,
}

impl SsoCredentials {
    pub fn new(student_id: impl Into<String>, password: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            student_id: student_id.into(),
            secret: Secret::Password(password.into()),
            rejected: Mutex::new(None),
        })
    }

    /// 사용자가 SmartID 화면에서 직접 로그인해 받은 `sToken` 으로 만듭니다. 비밀번호는 받지 않습니다.
    pub fn from_token(student_id: impl Into<String>, token: impl Into<String>) -> Arc<Self> {
        let token = token.into();
        let expires = Instant::now() + token_lifetime(&token).saturating_sub(TOKEN_MARGIN);
        Arc::new(Self {
            student_id: student_id.into(),
            secret: Secret::Token { token, expires },
            rejected: Mutex::new(None),
        })
    }

    /// 토큰 모드라면 남은 시간. 비밀번호 모드는 `None`.
    pub fn token_expires_in(&self) -> Option<Duration> {
        match &self.secret {
            Secret::Password(_) => None,
            Secret::Token { expires, .. } => {
                Some(expires.saturating_duration_since(Instant::now()))
            }
        }
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
        let password = match &self.secret {
            Secret::Token { token, expires } => {
                return if Instant::now() < *expires {
                    Ok(token.clone())
                } else {
                    Err(SsoError::CredentialUnusable(TOKEN_EXPIRED.into()))
                };
            }
            Secret::Password(password) => password,
        };
        if self.student_id.trim().is_empty() || password.is_empty() {
            return Err(SsoError::CredentialUnusable(
                "학번 또는 비밀번호가 비어 있습니다".into(),
            ));
        }
        match rusaint::obtain_ssu_sso_token(&self.student_id, password).await {
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
    async fn token_mode_returns_the_token_until_it_expires() {
        let sso = SsoCredentials::from_token("20240000", "abczPy86400zdef");
        assert_eq!(sso.token().await.unwrap(), "abczPy86400zdef");
        let left = sso.token_expires_in().unwrap();
        assert!(left > Duration::from_secs(23 * 3600) && left < Duration::from_secs(24 * 3600));

        let expired = Arc::new(SsoCredentials {
            student_id: "20240000".into(),
            secret: Secret::Token {
                token: "t".into(),
                expires: Instant::now(),
            },
            rejected: Mutex::new(None),
        });
        assert!(matches!(
            expired.token().await,
            Err(SsoError::CredentialUnusable(m)) if m == TOKEN_EXPIRED
        ));
    }

    #[test]
    fn token_lifetime_reads_py_seconds() {
        assert_eq!(token_lifetime("xzPy3600zy"), Duration::from_secs(3600));
        assert_eq!(token_lifetime("no-period"), DEFAULT_TOKEN_LIFETIME);
        assert_eq!(token_lifetime("zPy1z"), DEFAULT_TOKEN_LIFETIME);
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
