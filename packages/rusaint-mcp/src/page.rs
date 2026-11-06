//! 원격 커넥터 로그인 화면. shadcn/ui(zinc)의 토큰과 Card·Input·Button 모양을 그대로 옮긴 CSS 입니다.
//! 외부 리소스를 쓰지 않고, 인라인 스타일·스크립트는 CSP 해시로만 허용합니다.

use std::sync::LazyLock;

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

const STYLE: &str = r#"
:root{--background:#fff;--foreground:#09090b;--card:#fff;--muted:#f4f4f5;--muted-foreground:#71717a;--border:#e4e4e7;--primary:#18181b;--primary-foreground:#fafafa;--ring:#a1a1aa;--destructive:#dc2626;--radius:.5rem;color-scheme:light}
@media (prefers-color-scheme:dark){:root{--background:#09090b;--foreground:#fafafa;--card:#0c0c0e;--muted:#27272a;--muted-foreground:#a1a1aa;--border:#27272a;--primary:#fafafa;--primary-foreground:#18181b;--ring:#52525b;--destructive:#f87171;color-scheme:dark}}
*{box-sizing:border-box;margin:0}
body{min-height:100dvh;display:grid;grid-template-columns:minmax(0,1fr);place-items:center;padding:24px 16px;background:var(--background);color:var(--foreground);font:14px/1.5 "Pretendard Variable",Pretendard,-apple-system,BlinkMacSystemFont,"Apple SD Gothic Neo","Noto Sans KR",system-ui,sans-serif;-webkit-font-smoothing:antialiased;word-break:keep-all}
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
button:disabled .spin+span{display:none}
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
.safe{display:flex;gap:10px;align-items:flex-start;padding:12px 14px;border-radius:var(--radius);background:color-mix(in srgb,#22c55e 10%,transparent);border:1px solid color-mix(in srgb,#22c55e 30%,transparent)}
.safe svg{flex:none;margin-top:2px;color:#16a34a}
.safe strong{display:block;font-weight:600}
.safe p{color:var(--muted-foreground);font-size:13px;margin-top:2px}
.row{display:grid;grid-template-columns:1fr 1.4fr;gap:8px}.row>.secondary{order:-1}
h1 small{font-size:14px;font-weight:500;color:var(--muted-foreground)}
"#;

/// 제출 중 누른 버튼을 잠가 두 번 보내지 않게 합니다(로그인 화면 티켓은 한 번만 쓸 수 있음).
/// 폼 값이 만들어진 뒤에 잠그도록 한 박자 늦춥니다(잠근 버튼의 name 은 전송되지 않음).
const SCRIPT: &str = r#"document.querySelectorAll("form").forEach(function(f){f.addEventListener("submit",function(e){var b=e.submitter||f.querySelector("button");setTimeout(function(){b.disabled=true},0)})})"#;

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

const SCOPE: &str = r#"<div class="scope"><span>성적·시간표</span><span>과제·공지</span><span>비교과</span><span>학식</span><span>도서관</span></div>"#;

const LOCK: &str = r#"<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/></svg>"#;

/// 첫 화면. 기본은 학교 SmartID 화면으로 보내 비밀번호를 이 서버가 받지 않게 합니다.
/// `ticket` 은 서버가 만든 16진수 값이라 그대로 넣어도 안전합니다.
pub fn login(ticket: &str, sso_url: &str) -> String {
    let sso_url = escape(sso_url);
    document(
        "내 학교 정보를 Claude와 연결해요",
        &format!(
            r#"<main class="card">
<div class="header">
<h1>내 학교 정보를 Claude와 연결해요</h1>
</div>
<div class="content">
{SCOPE}
<div class="safe">{LOCK}<div><strong>비밀번호는 이 서버로 오지 않아요</strong><p>학교는 이 서버에 로그인 결과만 보내기에, 이 서버 어디에도 비밀번호가 남지 않아요.</p></div></div>
<a class="button" href="{sso_url}">숭실대학교 SSO로 로그인</a>
<details><summary><span>대신 비밀번호로 연결하기</span></summary>
<form method="post" action="/login">
<p class="hint">이 서버에 비밀번호를 저장해서, 세션이 끊겨도 서버가 갱신해요. 덕분에 매일 로그인하지 않아도 돼요.<br>등록한 학번만 쓸 수 있어요.</p>
<input type="hidden" name="ticket" value="{ticket}">
<div class="field"><label for="id">학번</label><input id="id" name="id" inputmode="numeric" autocomplete="username" placeholder="20240000" required maxlength="128"></div>
<div class="field"><label for="pw">학교 비밀번호</label><input id="pw" type="password" name="password" autocomplete="current-password" required maxlength="1024"></div>
<div class="field"><label for="lid">도서관 아이디 <small>(선택)</small></label><input id="lid" name="library_id" inputmode="numeric" autocomplete="off" placeholder="20240000" maxlength="128"></div>
<div class="field"><label for="lpw">도서관 비밀번호 <small>(선택)</small></label><input id="lpw" type="password" name="library_password" autocomplete="off" maxlength="1024"></div>
<button type="submit" class="secondary"><span class="spin"></span><span>비밀번호로 연결하기</span></button>
</form></details>
</div>
<p class="footer">서버가 다시 켜지면 한 번 더 로그인해야 해요.</p>
</main><script>{SCRIPT}</script>"#
        ),
    )
}

/// SmartID 로그인 뒤 도서관 계정(선택)을 받는 화면
pub fn everytime_step(ticket: &str, failed: bool) -> String {
    let alert = if failed {
        r#"<div class="alert error"><strong>에브리타임 아이디나 비밀번호를 확인해 주세요</strong></div>"#
    } else {
        ""
    };
    document(
        "에브리타임도 연결할까요?",
        &format!(
            r#"<main class="card">
<div class="header">
<span class="badge"><i></i>마지막 단계</span>
<h1>에브리타임도 연결할까요?</h1>
<p class="desc">강의평을 물어보려면 에브리타임 로그인이 필요해요.</p>
</div>
<form class="content" method="post" action="/sso/everytime">
{alert}
<input type="hidden" name="ticket" value="{ticket}">
<div class="field"><label for="eid">에브리타임 아이디</label><input id="eid" name="everytime_id" autocomplete="off" maxlength="128"></div>
<div class="field"><label for="epw">에브리타임 비밀번호</label><input id="epw" type="password" name="everytime_password" autocomplete="off" maxlength="1024"></div>
<div class="row">
<button type="submit"><span class="spin"></span><span>에브리타임 로그인하기</span></button>
<button type="submit" name="skip" value="1" class="secondary" formnovalidate><span class="spin"></span><span>건너뛰기</span></button>
</div>
</form>
<p class="footer">에브리타임 비밀번호는 이 서버에만 잠깐 보관해요. 서버가 다시 켜지면 사라져요.</p>
</main><script>{SCRIPT}</script>"#
        ),
    )
}

pub fn library_step(ticket: &str, failed: bool) -> String {
    let alert = if failed {
        r#"<div class="alert error"><strong>도서관 아이디나 비밀번호를 확인해 주세요</strong></div>"#
    } else {
        ""
    };
    document(
        "도서관도 연결할까요?",
        &format!(
            r#"<main class="card">
<div class="header">
<span class="badge"><i></i>SSO 로그인 완료</span>
<h1>도서관도 연결할까요?</h1>
<p class="desc">열람실 자리와 스터디룸을 예약하려면 도서관 로그인이 필요해요.</p>
</div>
<form class="content" method="post" action="/sso/finish">
{alert}
<input type="hidden" name="ticket" value="{ticket}">
<div class="field"><label for="lid">도서관 아이디</label><input id="lid" name="library_id" inputmode="numeric" autocomplete="off" placeholder="20240000" maxlength="128"></div>
<div class="field"><label for="lpw">도서관 비밀번호</label><input id="lpw" type="password" name="library_password" autocomplete="off" maxlength="1024"></div>
<div class="row">
<button type="submit"><span class="spin"></span><span>도서관 로그인하기</span></button>
<button type="submit" name="skip" value="1" class="secondary" formnovalidate><span class="spin"></span><span>건너뛰기</span></button>
</div>
</form>
<p class="footer">도서관 비밀번호는 이 서버에만 잠깐 보관해요. 서버가 다시 켜지면 사라져요.</p>
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
