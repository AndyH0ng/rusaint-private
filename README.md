<h1 align="center">rusaint-mcp</h1>
<p align="center"><i>숭실대학교 u-saint·LMS·도서관·에브리타임을 Claude 등 MCP 클라이언트에서 쓰는 서버</i></p>

---

rusaint-mcp 는 [rusaint](https://github.com/EATSTEAK/rusaint)(숭실대 u-saint Rust 클라이언트) 위에
숭실대의 학사·학습·생활 기능을 **[Model Context Protocol](https://modelcontextprotocol.io) 도구**로 노출하는 서버입니다.
Claude 데스크톱/모바일, Claude Code, Codex 등 MCP 를 지원하는 클라이언트에 붙여 "이번 학기 성적 알려줘",
"빈 열람실 좌석 잡아줘", "이 과목 강의평 보여줘" 같은 요청을 자연어로 처리합니다.

- **로컬(stdio) · 원격(HTTP) 둘 다** — 내 컴퓨터에서 stdio 로 쓰거나, 서버로 띄워 모바일 Claude 원격 커넥터로 연결합니다.
- **비밀번호를 서버에 두지 않음** — 기본 로그인은 학교 SmartID 화면에서 직접 하고, 서버는 단기 세션 토큰만 받습니다.
- **JS 런타임 불필요** — rusaint 가 u-saint(SAP Web Dynpro)를 네이티브로 파싱해 빠릅니다.

## 기능 (48개 도구)

### u-saint 학사정보 — 학교 로그인 필요
| 도구 | 설명 |
|---|---|
| `usaint_student_info` | 학적 기본 정보(학과·전공·학년·학기)와 변동 이력 |
| `usaint_timetable` | 개인 시간표 (요일·교시·강의실) |
| `usaint_registered_lectures` | 수강신청 내역 (과목·학점·교수·분반) |
| `usaint_grades_summary` | 학부 성적 요약(평점·이수학점)과 학기별 성적 |
| `usaint_semester_grades` | 특정 학기의 과목별 성적 |
| `usaint_scholarships` | 장학금 수혜 내역 |
| `usaint_graduation` | 졸업사정표: 이수 구분별 기준·취득 학점, 남은 요건 |
| `usaint_graduation_plan` | 못 채운 졸업 요건과 채울 수 있는 강의 후보 |
| `usaint_major_curriculum` | 입학년도별 공식 교과과정표 (학년·학기·이수구분) |
| `usaint_course_search` | 강의시간표 검색 (과목·교수·교양·채플 등) |
| `usaint_course_info` | 과목별 여석·P/F·평가 방식·전공 인정 여부 |
| `usaint_course_options` | 강의 검색용 단과대·학부·전공·교양 분야 목록 |
| `usaint_chapel` | 채플 이수 현황(출결·유고결석·성적)과 기준 계산 |

### LMS (유비온) — 학교 로그인 필요
| 도구 | 설명 |
|---|---|
| `lms_courses` | 학기 목록과 수강 강의(교수·수강 상태) |
| `lms_materials` | 주차별 자료·영상·과제·퀴즈와 완료 여부 |
| `lms_todos` | 미제출 과제·퀴즈·미시청 영상과 마감 |
| `lms_announcements` / `lms_announcement` | 강의 공지 목록 / 본문 |
| `lms_boards` / `lms_posts` / `lms_post` / `lms_comments` | 과목 게시판·글·댓글 |
| `lms_download_material` | 강의 자료 파일 내려받기 |
| `lms_download_post_attachment` | 게시판 첨부파일 내려받기 |

### 도서관 (oasis) — 도서관 로그인 필요
| 도구 | 설명 |
|---|---|
| `library_my_status` | 내 대출·좌석·스터디룸 현황 (반납·연장용 id 포함) |
| `library_seat_rooms` / `library_seats` | 열람실별 좌석 수 / 빈 좌석 목록 |
| `library_study_rooms` | 날짜별 스터디룸 빈 시간대 |
| `library_check_companion` | 스터디룸 동반 이용자 확인 |
| `prepare_library_seat_action` | 좌석 배정·반납·연장 준비 |
| `prepare_library_room_reservation` / `prepare_library_room_action` | 스터디룸 예약 / 취소·연장 준비 |
| `confirm_library_action` | 준비하고 동의한 도서관 작업 실행 |
| `library_auto_renew` / `library_auto_renew_status` / `library_stop_auto_renew` | 좌석·스터디룸 자동 연장 |

### 에브리타임 강의평 — 에타 쿠키 필요
| 도구 | 설명 |
|---|---|
| `everytime_search_lectures` | 강의평 등록된 과목 검색(과목명·교수명) |
| `everytime_lecture_reviews` | 한 과목의 강의평(집계 + 최근 후기) |
| `everytime_relink` | 세션 만료 시 학교 로그인 없이 에타만 다시 연결하는 링크 발급 |

### SSU-PATH 비교과 — 학교 로그인 필요
| 도구 | 설명 |
|---|---|
| `path_programs` | 지금 모집 중이고 내 학년이 신청 가능한 비교과 프로그램 |
| `path_program_detail` | 프로그램 상세(대상·선정·정원·일정) |
| `path_mileage` | 비교과 마일리지(누적·획득·사용·잔여·내역) |

### 로그인 없이 쓰는 도구
| 도구 | 설명 |
|---|---|
| `get_meals` / `get_dorm_meals` / `get_cafeteria_hours` | 교내·기숙사 식단과 식당 운영 시간 |
| `school_regulation` | 규정집(학사·장학·졸업 요건·타 학과 전공 인정 등) |
| `academic_calendar` | 학사일정(수강신청·개강·시험·휴복학·성적 공개) |
| `login` | 로그인 방법 안내 |

## 빠른 시작

### 로컬 (stdio)

```bash
cargo install --path packages/rusaint-mcp
# 계정 설정 (비밀번호는 화면에 표시되지 않습니다)
rusaint-mcp login            # sso / library / everytime 개별 설정도 가능
rusaint-mcp login status
```

MCP 클라이언트에 stdio 서버로 등록합니다. 예) Claude Code · Codex:

```toml
# ~/.codex/config.toml
[mcp_servers.rusaint]
command = "/path/to/.cargo/bin/rusaint-mcp"
```

설정 후 앱을 다시 시작하세요. 계정은 macOS Keychain 에 저장되며, 로그인에 성공한 것만 보관합니다.

### 원격 (모바일 Claude 커넥터)

모바일 Claude 는 Anthropic 클라우드에서 서버로 접속하므로, 항상 떠 있는 서버와 HTTPS 주소가 필요합니다.

```bash
cargo build --release -p rusaint-mcp
RUSAINT_MCP_PUBLIC_URL=https://mcp.example.com \
  RUSAINT_ALLOWED_IDS=20240000 \
  RUSAINT_MCP_BIND=127.0.0.1:3000 \
  ./target/release/rusaint-mcp serve
```

서버 구성·배포·보안 주의사항은 [`packages/rusaint-mcp/README.md`](packages/rusaint-mcp/README.md) 를 참고하세요.

## 배포

`mcp-v*` 태그를 push 하면 GitHub Actions 가 리눅스 바이너리를 빌드해 Release 로 올리고,
서버는 `update.timer` 로 최신 릴리스를 받아 자동 교체합니다. 자세한 건
[`packages/rusaint-mcp/deploy`](packages/rusaint-mcp/deploy) 를 참고하세요.

## 라이선스

MIT. u-saint 파싱 코어는 [EATSTEAK/rusaint](https://github.com/EATSTEAK/rusaint) 를 따릅니다.
