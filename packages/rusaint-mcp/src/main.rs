//! 숭실대학교 u-saint·LMS·학식·중앙도서관 MCP 서버 (stdio)
//!
//! stdout 은 MCP 프로토콜 전용이므로 진단 메시지는 stderr 로만 씁니다.

mod automation;
mod keychain;
mod pending;
mod server;
mod usaint;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = server::SsuServer::new()?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
