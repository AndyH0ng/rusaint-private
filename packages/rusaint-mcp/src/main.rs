//! 숭실대학교 u-saint·LMS·학식·중앙도서관 MCP 서버 (stdio)
//!
//! stdout 은 MCP 프로토콜 전용이므로 진단 메시지는 stderr 로만 씁니다.
//! `rusaint-mcp login` 은 MCP 서버 대신 계정 저장 명령을 실행합니다.

mod automation;
mod keychain;
mod login;
mod page;
mod pending;
mod remote;
mod server;
mod usaint;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("login") {
        return Ok(login::run(&args[1..]).await);
    }
    if args.first().map(String::as_str) == Some("serve") {
        remote::run().await?;
        return Ok(std::process::ExitCode::SUCCESS);
    }
    if !args.is_empty() {
        return Err("사용법: rusaint-mcp [login [sso|library|status]|serve]".into());
    }
    let server = server::SsuServer::new()?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(std::process::ExitCode::SUCCESS)
}
