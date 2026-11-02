//! `rusaint-mcp login`: 계정을 터미널에서 입력받아 실제 로그인으로 확인한 뒤 Keychain 에 저장합니다.
//!
//! ```text
//! rusaint-mcp login            통합(SSO) 계정과 도서관 계정을 차례로
//! rusaint-mcp login sso        통합 계정만
//! rusaint-mcp login library    도서관 계정만
//! rusaint-mcp login status     저장된 항목 확인 (비밀번호는 보여 주지 않음)
//! ```

use std::io::{BufRead, IsTerminal, Write};
use std::process::{ExitCode, Stdio};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::keychain::{self, LIBRARY_SERVICE, SSO_SERVICE};

#[derive(Clone, Copy)]
enum Account {
    Sso,
    Library,
}

impl Account {
    fn service(self) -> &'static str {
        match self {
            Account::Sso => SSO_SERVICE,
            Account::Library => LIBRARY_SERVICE,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Account::Sso => "통합(SSO) 계정 — u-saint·LMS·SSU-PATH",
            Account::Library => "중앙도서관 계정",
        }
    }
}

pub async fn run(args: &[String]) -> ExitCode {
    let accounts: &[Account] = match args.first().map(String::as_str) {
        None | Some("all") => &[Account::Sso, Account::Library],
        Some("sso") => &[Account::Sso],
        Some("library") => &[Account::Library],
        Some("status") => return status().await,
        Some(other) => {
            eprintln!("알 수 없는 대상: {other} (sso, library, status 중 하나)");
            return ExitCode::from(2);
        }
    };
    // `!` 실행처럼 터미널이 아니면 비밀번호를 받을 수 없어 빈 값이 저장될 수 있습니다.
    // 그런 곳(Claude Code 의 `!` 등)에서 부르면 새 Terminal 창을 열어 거기서 입력받습니다.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return open_terminal(args).await;
    }
    let mut ok = true;
    for &account in accounts {
        ok &= login_one(account).await;
    }
    if ok {
        eprintln!(
            "\n완료. 이미 켜져 있는 Claude Code 세션·데스크톱 앱·Codex 는 다시 시작해야 새 계정을 읽습니다."
        );
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// 같은 명령을 새 Terminal 창에서 다시 실행합니다. 인자는 위에서 sso/library/all 로 검증된 값뿐입니다.
async fn open_terminal(args: &[String]) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe.display().to_string(),
        Err(e) => {
            eprintln!("실행 파일 경로를 찾지 못했습니다: {e}");
            return ExitCode::FAILURE;
        }
    };
    let command = format!("'{}' login {}", exe.replace('\'', r"'\''"), args.join(" "));
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        command.replace('\\', "\\\\").replace('"', "\\\"")
    );
    match Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::null())
        .status()
        .await
    {
        Ok(s) if s.success() => {
            eprintln!(
                "새 Terminal 창을 열었습니다. 그 창에서 아이디와 비밀번호를 입력하세요.\n끝나면 `rusaint-mcp login status` 로 확인하고, MCP 를 쓰는 앱(Claude Code 는 /mcp 에서 재연결)을 다시 시작하세요."
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "Terminal 창을 열지 못했습니다. 터미널 앱에서 직접 `rusaint-mcp login` 을 실행해 주세요."
            );
            ExitCode::from(2)
        }
    }
}

async fn status() -> ExitCode {
    for account in [Account::Sso, Account::Library] {
        match keychain::credentials(account.service()).await {
            Ok((id, password)) => eprintln!(
                "{}: {id} (비밀번호 {}자 저장됨)",
                account.label(),
                password.chars().count()
            ),
            Err(_) => eprintln!("{}: 저장되지 않음", account.label()),
        }
    }
    ExitCode::SUCCESS
}

async fn login_one(account: Account) -> bool {
    eprintln!("\n[{}]", account.label());
    let existing = keychain::credentials(account.service())
        .await
        .ok()
        .map(|(id, _)| id);

    let id = match prompt_line(match &existing {
        Some(id) => format!("아이디 [{id}] (Enter 는 그대로, - 는 건너뛰기): "),
        None => "아이디 (- 는 건너뛰기): ".to_string(),
    }) {
        Some(s) if s == "-" => {
            eprintln!("건너뜀");
            return true;
        }
        Some(s) if !s.is_empty() => s,
        _ => match existing.clone() {
            Some(id) => id,
            None => {
                eprintln!("아이디가 비어 있어 건너뜁니다.");
                return true;
            }
        },
    };
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
    {
        eprintln!("아이디에 쓸 수 없는 문자가 있습니다.");
        return false;
    }
    let password = match rpassword::prompt_password("비밀번호 (화면에 보이지 않음): ") {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            eprintln!("비밀번호가 비어 있어 저장하지 않습니다.");
            return false;
        }
        Err(e) => {
            eprintln!("비밀번호를 읽지 못했습니다: {e}");
            return false;
        }
    };

    eprint!("실제로 한 번 로그인해서 확인하는 중… ");
    let _ = std::io::stderr().flush();
    if let Err(reason) = verify(account, &id, &password).await {
        eprintln!("실패\n  {reason}\n  저장하지 않았습니다.");
        return false;
    }
    eprintln!("성공");

    if let Some(old) = existing.filter(|old| old != &id) {
        // 같은 서비스에 항목이 둘 있으면 어느 쪽을 읽을지 모호해지므로 이전 아이디 항목을 지웁니다.
        if let Err(e) = security(&format!(
            "delete-generic-password -s {} -a {old}",
            account.service()
        ))
        .await
        {
            eprintln!("이전 항목({old})을 지우지 못했습니다: {e}");
        }
    }
    match save(account.service(), &id, &password).await {
        Ok(()) => {
            eprintln!("Keychain '{}' 에 저장했습니다.", account.service());
            true
        }
        Err(e) => {
            eprintln!("Keychain 저장 실패: {e}");
            false
        }
    }
}

/// 저장 전에 실제로 한 번만 로그인해 봅니다. 실패해도 재시도하지 않습니다(계정 잠금 방지).
async fn verify(account: Account, id: &str, password: &str) -> Result<(), String> {
    match account {
        Account::Sso => rusaint_sso::SsoCredentials::new(id, password)
            .token()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Account::Library => rusaint_library::LibraryClient::new(id, password)
            .map_err(|e| e.to_string())?
            .login()
            .await
            .map_err(|e| e.to_string()),
    }
}

fn prompt_line(prompt: String) -> Option<String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}

/// 비밀번호가 프로세스 목록(명령줄 인자)에 보이지 않도록 `security -i` 의 표준입력으로 명령을 넘기고,
/// 비밀번호는 16진수(-X)로 보냅니다. 항목은 /usr/bin/security 가 만들어, 서버가 같은 도구로 읽을 때 권한 팝업이 뜨지 않습니다.
async fn save(service: &str, id: &str, password: &str) -> Result<(), String> {
    let hex: String = password.bytes().map(|b| format!("{b:02x}")).collect();
    security(&format!(
        "add-generic-password -U -s {service} -a {id} -X {hex}"
    ))
    .await
}

async fn security(command: &str) -> Result<(), String> {
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("security 실행 실패: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin
        .write_all(format!("{command}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // 대화형 모드는 명령이 실패해도 종료 코드가 0일 수 있어 오류 출력도 확인합니다.
    if !output.status.success() || stderr.contains("error") || stderr.contains("SecKeychain") {
        return Err(stderr.trim().to_string());
    }
    Ok(())
}
