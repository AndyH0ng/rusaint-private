# rusaint MCP

## 로컬 로그인

```sh
cargo run -p rusaint-mcp -- login
# 통합 계정 / 도서관을 따로 설정할 수도 있습니다.
cargo run -p rusaint-mcp -- login sso
cargo run -p rusaint-mcp -- login library
cargo run -p rusaint-mcp -- login status
```

macOS 터미널에서 계정과 비밀번호를 입력하면 실제 로그인에 성공한 계정만
Keychain에 저장합니다. 비밀번호 입력은 화면에 표시되지 않습니다.
Claude Code의 `!` 처럼 터미널이 아닌 곳에서 실행하면 새 Terminal 창을 열어 그 창에서 입력받습니다.
설정 후 Claude 등의 앱을 다시 시작하세요. 인자 없이 실행하면 기존 stdio MCP가 시작됩니다.
대화에서 `login` 도구를 호출해 로그인 방법을 확인할 수 있습니다.

## 모바일 Claude용 원격 커넥터

Claude 원격 커넥터는 Anthropic 클라우드에서 서버로 접속합니다.
항상 실행되는 서버와 외부에서 접근 가능한 HTTPS 주소가 필요합니다.
로컬 stdio 등록만으로는 모바일에서 연결되지 않습니다.

```sh
cargo build --release -p rusaint-mcp
RUSAINT_MCP_PUBLIC_URL=https://mcp.example.com \
  RUSAINT_ALLOWED_IDS=20240000 \
  RUSAINT_MCP_BIND=127.0.0.1:3000 \
  ./target/release/rusaint-mcp serve
```

`mcp.example.com`은 실제 서버 도메인으로 바꾸세요. HTTPS reverse proxy에서
해당 도메인의 모든 경로를 `127.0.0.1:3000`으로 전달합니다.
SSE 응답 버퍼링은 끄고 연결 제한 시간은 길게 설정하세요.
인증 헤더를 전달하고, 로그인 폼 본문·인증 코드·토큰을 로그에 저장하지 마세요.
`RUSAINT_MCP_PUBLIC_URL`은 경로나 `/mcp`가 없는 HTTPS origin입니다.
`RUSAINT_ALLOWED_IDS`(쉼표 구분)에 없는 학번은 SmartID에 보내지 않고 거부합니다.

테스트는 맥에서 서버를 띄우고 `cloudflared tunnel --url http://127.0.0.1:3000`이 알려 주는
`https://….trycloudflare.com` 주소를 `RUSAINT_MCP_PUBLIC_URL`로 쓰면 됩니다. 이 주소는 터널을
다시 띄울 때마다 바뀌므로 그때마다 서버를 다시 시작하고 커넥터를 다시 연결해야 합니다.
서버는 요청마다 메서드·경로·상태 코드만 stderr에 남깁니다.

1. Claude 웹의 설정 → 커넥터에서 사용자 지정 커넥터를 추가합니다.
2. URL에 `https://mcp.example.com/mcp`를 입력합니다.
3. 연결할 때 열리는 브라우저 화면에서 통합 계정과 선택적으로 도서관 계정을 입력하고 연결을 허용합니다.
4. 같은 Claude 계정의 모바일 앱에서 연결한 커넥터를 사용합니다.

OAuth discovery, 동적 클라이언트 등록, authorization code와 S256 PKCE를 지원합니다.
현재 허용하는 콜백은 Claude의 `https://claude.ai/api/mcp/auth_callback`입니다.
학교 자체의 OAuth 연동이 아니라 이 MCP 서버가 학교 로그인을 검증하는 방식이므로,
본인이 관리하고 신뢰하는 서버에서만 운영하세요.

원격 계정은 호스트의 Keychain을 읽거나 변경하지 않습니다. 사용자마다 분리된
클라이언트를 사용하며 계정과 토큰은 메모리에만 보관합니다. 접속 토큰은 1시간,
갱신 토큰은 30일이며 갱신할 때마다 새 갱신 토큰으로 바뀌므로 30일 안에 한 번이라도 쓰면 연결이 유지됩니다.
학교 쪽 세션(LMS, SSU-PATH 등)이 끊기면 서버가 메모리의 계정으로 다시 로그인합니다.
서버를 재시작하면 모두 사라지므로 커넥터를 다시 연결해야 합니다.
로그인 화면·인증 코드는 5분 뒤 만료되고 한 번만 사용할 수 있습니다.
로그인은 서버 전체에서 분당 10회로 제한됩니다. 네트워크 오류 등으로 로그인에
실패해도 자동 재시도하지 않습니다. 원격 다운로드는 서버에 개인정보 파일을
남기지 않도록 차단됩니다. 자동 연장은 서버 실행 중에만 동작합니다.

Cloudflare quick tunnel을 거쳐 Claude iPhone 앱에서 연결과 도구 호출을 확인했습니다.

참고: [Claude 원격 커넥터 안내](https://support.claude.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp),
[MCP 인증 명세](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization).
