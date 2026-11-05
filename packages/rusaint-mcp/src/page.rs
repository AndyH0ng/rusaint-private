//! 원격 커넥터 로그인 화면. shadcn/ui(zinc)의 토큰과 Card·Input·Button 모양을 그대로 옮긴 CSS 입니다.
//! 외부 리소스를 쓰지 않고, 인라인 스타일·스크립트는 CSP 해시로만 허용합니다.

use std::sync::LazyLock;

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

const STYLE: &str = r#"
:root{--background:#fff;--foreground:#09090b;--card:#fff;--muted:#f4f4f5;--muted-foreground:#71717a;--border:#e4e4e7;--primary:#18181b;--primary-foreground:#fafafa;--ring:#a1a1aa;--destructive:#dc2626;--radius:.5rem;color-scheme:light}
@media (prefers-color-scheme:dark){:root{--background:#09090b;--foreground:#fafafa;--card:#0c0c0e;--muted:#27272a;--muted-foreground:#a1a1aa;--border:#27272a;--primary:#fafafa;--primary-foreground:#18181b;--ring:#52525b;--destructive:#f87171;color-scheme:dark}}
*{box-sizing:border-box;margin:0}
body{min-height:100dvh;display:grid;place-items:center;padding:24px 16px;background:var(--background);color:var(--foreground);font:14px/1.5 "Pretendard Variable",Pretendard,-apple-system,BlinkMacSystemFont,"Apple SD Gothic Neo","Noto Sans KR",system-ui,sans-serif;-webkit-font-smoothing:antialiased;word-break:keep-all}
.card{width:100%;max-width:400px;border:1px solid var(--border);border-radius:calc(var(--radius) + 4px);background:var(--card);box-shadow:0 1px 3px rgb(0 0 0/.06),0 1px 2px rgb(0 0 0/.04)}
.header{padding:24px 24px 0;display:grid;gap:6px}
.badge{justify-self:start;display:inline-flex;align-items:center;gap:6px;height:22px;padding:0 8px;border:1px solid var(--border);border-radius:999px;font-size:12px;font-weight:500;color:var(--muted-foreground)}
.badge i{width:6px;height:6px;border-radius:50%;background:#22c55e}
h1{font-size:20px;font-weight:600;letter-spacing:-.02em;line-height:1.3;margin-top:6px}
.desc{color:var(--muted-foreground)}
.content{padding:20px 24px 24px;display:grid;gap:16px}
.scope{display:flex;flex-wrap:wrap;gap:6px}
.scope span{height:24px;display:inline-flex;align-items:center;padding:0 8px;border-radius:calc(var(--radius) - 2px);background:var(--muted);font-size:12px;font-weight:500}
.field{display:grid;gap:6px}
label{font-size:14px;font-weight:500;line-height:1}
input{width:100%;height:36px;padding:4px 12px;border:1px solid var(--border);border-radius:calc(var(--radius) - 2px);background:transparent;color:inherit;font:inherit;box-shadow:0 1px 2px rgb(0 0 0/.04);transition:box-shadow .15s,border-color .15s}
input::placeholder{color:var(--muted-foreground)}
input:focus-visible{outline:none;border-color:var(--ring);box-shadow:0 0 0 3px color-mix(in srgb,var(--ring) 35%,transparent)}
details{border:1px solid var(--border);border-radius:var(--radius)}
summary{list-style:none;cursor:pointer;display:flex;justify-content:space-between;align-items:center;padding:10px 12px;font-weight:500}
summary::-webkit-details-marker{display:none}
summary small{color:var(--muted-foreground);font-weight:400;font-size:12px}
summary:after{content:"";width:7px;height:7px;border-right:1.5px solid var(--muted-foreground);border-bottom:1.5px solid var(--muted-foreground);transform:rotate(45deg) translateY(-2px);transition:transform .15s}
details[open] summary:after{transform:rotate(225deg) translateY(-2px)}
details .inner{display:grid;gap:12px;padding:0 12px 12px}
button{height:36px;width:100%;display:inline-flex;align-items:center;justify-content:center;gap:8px;border:0;border-radius:calc(var(--radius) - 2px);background:var(--primary);color:var(--primary-foreground);font:inherit;font-weight:500;cursor:pointer;box-shadow:0 1px 2px rgb(0 0 0/.1);transition:opacity .15s}
button:hover{opacity:.9}
button:disabled{opacity:.6;cursor:progress}
button:focus-visible{outline:none;box-shadow:0 0 0 3px color-mix(in srgb,var(--ring) 50%,transparent)}
.spin{display:none;width:14px;height:14px;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:s .7s linear infinite}
button:disabled .spin{display:inline-block}
@keyframes s{to{transform:rotate(360deg)}}
.footer{padding:12px 24px;border-top:1px solid var(--border);color:var(--muted-foreground);font-size:12px}
.alert{display:grid;gap:4px;padding:12px 16px;border:1px solid var(--border);border-radius:var(--radius)}
.alert.error{border-color:color-mix(in srgb,var(--destructive) 40%,transparent);color:var(--destructive)}
.alert p{color:var(--muted-foreground)}
a.button{height:44px;width:100%;display:inline-flex;align-items:center;justify-content:center;border-radius:calc(var(--radius) - 2px);background:var(--primary);color:var(--primary-foreground);font-size:15px;font-weight:600;text-decoration:none;box-shadow:0 1px 2px rgb(0 0 0/.1);transition:opacity .15s}
a.button:hover{opacity:.9}
a.button:focus-visible{outline:none;box-shadow:0 0 0 3px color-mix(in srgb,var(--ring) 50%,transparent)}
.hint{color:var(--muted-foreground);font-size:13px}
.hint b{color:var(--foreground);font-weight:500}
details form{display:grid;gap:12px;padding:0 12px 12px}
button.secondary{background:transparent;color:var(--foreground);border:1px solid var(--border);box-shadow:none}
"#;

/// 제출 중 버튼을 잠가 두 번 보내지 않게 합니다(로그인 화면 티켓은 한 번만 쓸 수 있음).
const SCRIPT: &str = r#"document.querySelector("form").addEventListener("submit",function(){var b=this.querySelector("button");b.disabled=true;b.lastChild.textContent="학교 계정 확인 중…"})"#;

fn hash(source: &str) -> String {
    format!(
        "'sha256-{}'",
        STANDARD.encode(Sha256::digest(source.as_bytes()))
    )
}

/// 로그인 화면용 Content-Security-Policy
pub static CSP: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; style-src {}; script-src {}; form-action 'self' https://claude.ai http://localhost:* http://127.0.0.1:*; frame-ancestors 'none'; base-uri 'none'",
        hash(STYLE),
        hash(SCRIPT)
    )
});

fn document(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="ko"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title><style>{STYLE}</style></head><body>{body}</body></html>"#
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const SCOPE: &str = r#"<div class="scope"><span>u-saint</span><span>LMS</span><span>SSU-PATH</span><span>학식</span><span>도서관</span></div>"#;

/// 첫 화면. 기본은 학교 SmartID 화면으로 보내 비밀번호를 이 서버가 받지 않게 합니다.
/// `ticket` 은 서버가 만든 16진수 값이라 그대로 넣어도 안전합니다.
pub fn login(ticket: &str, sso_url: &str) -> String {
    let sso_url = escape(sso_url);
    document(
        "숭실대학교 계정 연결",
        &format!(
            r#"<main class="card">
<div class="header">
<span class="badge"><i></i>rusaint MCP · 비공식</span>
<h1>숭실대학교 계정 연결</h1>
<p class="desc">AI 앱이 요청에 따라 학교 정보를 조회하고, 도서관 예약은 확인을 받은 뒤 실행합니다.</p>
</div>
<div class="content">
{SCOPE}
<a class="button" href="{sso_url}">숭실대 통합 로그인으로 연결</a>
<p class="hint"><b>smartid.ssu.ac.kr</b> 학교 로그인 화면으로 이동합니다. 비밀번호는 학교 화면에만 입력하며, 이 서버는 학교가 발급한 로그인 토큰(약 하루 유효)만 받습니다. 하루가 지나면 다시 연결해야 합니다.</p>
<details><summary><span>학번·비밀번호로 직접 연결 <small>서버 운영자용 · 오래 유지</small></span></summary>
<form method="post" action="/login">
<p class="hint">비밀번호를 이 서버 메모리에 맡겨 학교 세션이 끊겨도 다시 로그인합니다. 이 서버를 운영하는 본인만 쓰세요.</p>
<input type="hidden" name="ticket" value="{ticket}">
<div class="field"><label for="id">학번</label><input id="id" name="id" inputmode="numeric" autocomplete="username" placeholder="20240000" required maxlength="128"></div>
<div class="field"><label for="pw">통합 계정 비밀번호</label><input id="pw" type="password" name="password" autocomplete="current-password" required maxlength="1024"></div>
<div class="field"><label for="lid">도서관 아이디 <small>선택</small></label><input id="lid" name="library_id" autocomplete="off" maxlength="128"></div>
<div class="field"><label for="lpw">도서관 비밀번호 <small>선택</small></label><input id="lpw" type="password" name="library_password" autocomplete="off" maxlength="1024"></div>
<button type="submit" class="secondary"><span class="spin"></span><span>비밀번호로 연결</span></button>
</form></details>
</div>
<p class="footer">계정과 토큰은 이 서버 메모리에만 있고 서버가 재시작되면 사라집니다.</p>
</main><script>{SCRIPT}</script>"#
        ),
    )
}

/// SmartID 로그인 뒤 도서관 계정(선택)을 받는 화면. `id` 는 허용 목록을 통과한 학번입니다.
pub fn library_step(ticket: &str, id: &str, error: Option<&str>) -> String {
    let id = escape(id);
    let alert = error
        .map(|e| {
            format!(
                r#"<div class="alert error"><strong>도서관 로그인 실패</strong><p>{}</p></div>"#,
                escape(e)
            )
        })
        .unwrap_or_default();
    document(
        "도서관 계정 연결",
        &format!(
            r#"<main class="card">
<div class="header">
<span class="badge"><i></i>통합 로그인 완료 · {id}</span>
<h1>도서관 계정 <small>선택</small></h1>
<p class="desc">열람실 좌석·스터디룸·대출을 쓰려면 도서관 계정이 필요합니다. 도서관(oasis)은 학교 통합 로그인을 지원하지 않아, 쓰려면 이 서버에 아이디·비밀번호를 입력해야 합니다.</p>
</div>
<form class="content" method="post" action="/sso/finish">
{alert}
<input type="hidden" name="ticket" value="{ticket}">
<div class="field"><label for="lid">도서관 아이디</label><input id="lid" name="library_id" autocomplete="off" maxlength="128"></div>
<div class="field"><label for="lpw">도서관 비밀번호</label><input id="lpw" type="password" name="library_password" autocomplete="off" maxlength="1024"></div>
<button type="submit"><span class="spin"></span><span>연결 완료</span></button>
<p class="hint">도서관 기능을 쓰지 않으면 <b>비워 두고 연결 완료</b>를 누르세요.</p>
</form>
<p class="footer">도서관 비밀번호를 입력하면 이 서버 메모리에만 보관되고 서버가 재시작되면 사라집니다.</p>
</main><script>{SCRIPT}</script>"#
        ),
    )
}

/// 로그인 실패·거부 화면
pub fn message(title: &str, detail: &str) -> String {
    document(
        title,
        &format!(
            r#"<main class="card"><div class="content"><div class="alert error"><strong>{title}</strong><p>{detail}</p></div></div></main>"#
        ),
    )
}
