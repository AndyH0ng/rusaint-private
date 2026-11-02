//! macOS Keychain 의 generic password 항목에서 계정 정보를 읽습니다.

use tokio::process::Command;

/// 도서관 계정 항목 이름
pub const LIBRARY_SERVICE: &str = "rusaint-library";

/// `service` 항목의 (계정, 비밀번호)를 돌려줍니다. 값은 로그나 오류 메시지에 넣지 않습니다.
pub async fn credentials(service: &str) -> Result<(String, String), String> {
    let missing = || {
        format!(
            "Keychain 에 '{service}' 항목이 없습니다. 터미널에서 \
             `security add-generic-password -U -s {service} -a <아이디> -w` 로 저장해 주세요."
        )
    };
    let attributes = Command::new("security")
        .args(["find-generic-password", "-s", service])
        .output()
        .await
        .map_err(|e| format!("security 실행 실패: {e}"))?;
    if !attributes.status.success() {
        return Err(missing());
    }
    let account = String::from_utf8_lossy(&attributes.stdout)
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("\"acct\"<blob>=\"")
                .and_then(|rest| rest.strip_suffix('"'))
                .map(str::to_owned)
        })
        .ok_or_else(missing)?;

    let secret = Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .await
        .map_err(|e| format!("security 실행 실패: {e}"))?;
    if !secret.status.success() {
        return Err(missing());
    }
    let password = String::from_utf8(secret.stdout)
        .map_err(|_| "Keychain 비밀번호가 UTF-8 이 아닙니다".to_string())?
        .trim_end_matches('\n')
        .to_string();
    if password.is_empty() {
        return Err(format!(
            "Keychain '{service}' 항목의 비밀번호가 비어 있습니다. \
             별도 터미널 창에서 다시 저장해 주세요."
        ));
    }
    Ok((account, password))
}
