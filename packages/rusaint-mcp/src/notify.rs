//! 진행 상황과 상태를 클라이언트에 알립니다.
//!
//! 미국 리전에서 학교까지 왕복이 길어 증명서 발급처럼 단계가 많은 작업은 몇 초가 걸립니다.
//! 아무 신호가 없으면 멈춘 것처럼 보이므로 단계마다 진행 알림을 보냅니다.
//!
//! 진행 알림은 클라이언트가 `_meta` 에 progressToken 을 준 요청에만 보냅니다(MCP 규약).
//!
//! 서버 로그 알림(`notifications/message`)은 쓰지 않습니다. MCP 가 SEP-2577 로 폐기했고
//! 곧 제거됩니다. 사용자에게 알릴 내용은 진행 메시지와 도구 결과·오류 문구에 담습니다.

use rmcp::RoleServer;
use rmcp::model::ProgressNotificationParam;
use rmcp::service::RequestContext;

/// 한 작업의 진행 단계를 세어 알립니다.
pub struct Progress<'a> {
    context: &'a RequestContext<RoleServer>,
    /// 전체 단계 수. 알 수 없으면 `None`.
    total: Option<f64>,
    done: f64,
}

impl<'a> Progress<'a> {
    pub fn new(context: &'a RequestContext<RoleServer>, total: u32) -> Self {
        Self {
            context,
            total: Some(f64::from(total)),
            done: 0.0,
        }
    }

    /// 다음 단계를 알립니다. 클라이언트가 진행 토큰을 주지 않았으면 아무 일도 하지 않습니다.
    pub async fn step(&mut self, message: &str) {
        self.done += 1.0;
        let Some(progress_token) = self.context.meta.get_progress_token() else {
            return;
        };
        // 알림이 실패해도 작업은 계속합니다. 알림 때문에 발급을 멈추지 않습니다.
        let _ = self
            .context
            .peer
            .notify_progress({
                let param =
                    ProgressNotificationParam::new(progress_token, self.done).with_message(message);
                match self.total {
                    Some(total) => param.with_total(total),
                    None => param,
                }
            })
            .await;
    }
}
