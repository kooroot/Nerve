# Nerve 터미널 업그레이드 제안서

> 작성일: 2026-05-22
> 대상 버전: Nerve v0.1.9 → v0.2.0
> 검토 범위: Claude Code 2.1.x / Codex CLI v0.130 신규 기능을 Nerve 인터랙티브 터미널에 차용

---

## 0. 요약 (TL;DR)

Nerve는 이미 슬래시 명령 21종, 서브커맨드 11종, raw TTY 라인 에디터,
세션·패치 영속성을 갖추고 있다. 그러나 Claude Code와 Codex가 최근 1년 사이
추가한 **두 축**이 비어 있다.

1. **가시성** — 진행 중인 lead/reviewer 작업의 실시간 상태(라운드, 비용, ETA)
   가 정적 `/status` 출력에만 의존한다.
2. **목표 지향 자동화** — `max_refinement_rounds`만으로 종료를 판정하므로
   "조건 충족 시 자동 stop / 미충족 시 자동 재시도" 워크플로우가 없다.

이 문서는 위 두 격차를 메우기 위한 3단 우선순위 로드맵을 정의한다.
Tier 1(상태 바 + `/goal` + 템플릿 검색)을 v0.2.0으로 묶는 것이 ROI가
가장 크다.

---

## 1. 현재 Nerve 터미널 구현 인벤토리

### 1.1 슬래시 명령 (21개, `crates/nerve-cli/src/main.rs`)

| 명령 | 설명 | 위치 |
|------|------|------|
| `/login` | Claude/Codex 인증 | main.rs:1243 |
| `/doctor` | 설정·어댑터·인증 검사 | main.rs:1244 |
| `/status` | 워크스페이스 상태 | main.rs:1245 |
| `/mode <dry-run\|apply>` | 패치 적용 모드 전환 | main.rs:1261 |
| `/adapter <real\|mock>` | 어댑터 전환 | main.rs:1270 |
| `/pwd`, `/cd <path>` | 디렉터리 조작 | main.rs:1279~1280 |
| `/clear` | 화면 클리어 | main.rs:1291 |
| `/paste` ... `/end` | 멀티라인 입력 | main.rs:735, 767 |
| `/history` | 최근 10개 세션 | main.rs:1296 |
| `/resume <id>` | 저장된 세션 리포트 | main.rs:1308 |
| `/list` | 패치 인덱스 | main.rs:1313 |
| `/diff` | 마지막 리뷰된 패치 | main.rs:1363 |
| `/apply [patch-id]` | 패치 적용 | main.rs:1372 |
| `/rollback [patch-id]` | 패치 롤백 | main.rs:1382 |
| `/templates`, `/template <id> [args]` | 템플릿 목록·실행 | main.rs:1321~1323 |
| `/benchmark pi [n]` | Pi 워크플로우 벤치 | main.rs:1349 |
| `/help` | 도움말 | main.rs:1242 |
| `/quit`, `/exit`, `/q` | 종료 | main.rs:1392 |
| `!<shell>` | 셸 명령 | main.rs:745 |

### 1.2 서브커맨드 (11개)

```
nv benchmark pi [--iterations N] [--live] [--json]
nv config validate
nv history [--json] [--applied] [--blocked] [--named]
nv resume <task-id> [--json]
nv list [--json]
nv apply <patch-id>
nv rollback <patch-id>
nv doctor
nv daemon [--once] [--rpc]
nv setup
nv login [all|claude|codex]
nv interactive
nv name <task-id> <name>
nv rerun <task-id> <prompt>
nv template list|run
```

### 1.3 TTY 입력 (raw mode, `InteractiveLineEditor` main.rs:904)

- POSIX raw mode: `libc::tcsetattr` (main.rs:1692~1705),
  `RawTerminalGuard`로 suspend/resume
- 히스토리: Up/Down 화살표로 prompt history 순회 (main.rs:981~989)
- 자동완성: `/` 입력 → 슬래시 명령 팔레트(20개 하드코딩, main.rs:836~890),
  Tab/Right로 선택 완성
- 단일 라인 에디터: Backspace, Enter, Ctrl+C, Ctrl+D

### 1.4 영속성 레이아웃 (`.nerve/`)

```
.nerve/
├── sessions/         # RunReport JSON
├── patches/          # NvPatch JSON + index.json
│   └── index.json    # PatchRecord[]
├── session-meta/     # SessionMetadata (옵션)
└── scratch/          # 임시 데이터
```

저장 메타데이터: `SessionSummary`(verdict/rounds/applied/blocked/cost),
`PatchRecord`(file_count/changed_files), `RunReport`(rounds[], events[], usage).

### 1.5 RPC 이벤트 (`nv daemon --rpc`, main.rs:1824~1850)

현재 4종만 emit:
- `session_end`
- `review_event`
- `patch_ready`
- `apply_result`

→ **중간 진행 상황(round_start, stdout_chunk, cost_update)이 없어
   외부 UI가 라이브 시각화를 못 한다.**

### 1.6 명백히 비어 있는 영역

- 진행률 스피너 / ETA
- 라이브 agent view (lead/reviewer 실시간 상태)
- Plan mode (read-only 분석 → 계획 승인 → 실행)
- Goal tracking (조건 기반 자동 종료/재시도)
- ratatui 기반 멀티 패널 (`--tui`는 main.rs:1983 스텁만 존재)
- MCP 서버 연결
- 세션 fork / 다중 분기 비교
- 동시 워킹디렉터리 변경 감지
- 세션 검색 (현재는 시간순 10개만)

---

## 2. Claude Code 2.x / Codex CLI 신기능 매핑

### 2.1 Goals / Plan / TODO

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| `/goal` | Claude Code | 종료 조건 등록 → 만족 시 자동 stop, 불만족 시 재시도 |
| `/loop` | Claude Code | 폴링 워크플로우(배포·CI 감시)용 |
| Plan mode (`Shift+Tab`×2) | Claude Code | read-only 분석 → 계획 승인 → 실행 |

### 2.2 Agent View / 멀티 에이전트 시각화

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| `claude agents` 화면 | Claude Code | `/status --live`로 lead/reviewer 행별 상태·timer·cost |
| 상태 아이콘 ✽/✻/∙/✢ | Claude Code | adapter 이벤트를 아이콘으로 인코딩 |
| Peek 패널 (`Space`) | Claude Code | `/diff` 인라인 미리보기 |
| Background supervisor | Claude Code | nerve daemon 프로세스 상태 API |

### 2.3 Slash Command / Palette

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| Enhanced `/skills` (필터·토큰 정렬) | Claude Code v2.1.142+ | `/templates` 검색·정렬 |
| `/model` 선택기 + `d` 고정 | Claude Code | lead/reviewer 모델 picker |
| `--agent <name>` flag | Claude Code | `nv /apply --agent reviewer` |
| Transcript navigation (`{`/`}`) | Claude Code | `/diff --prev`/`--next` |

### 2.4 Hooks / Skills

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| Hook context 확장 | Claude Code | PostToolUse hook으로 lint/test 자동 체인 |
| `type: "mcp_tool"` hook | Claude Code | `/apply` 시 LSP 타입 체크 자동화 |
| `continueOnBlock: true` | Claude Code | reviewer 실패 시 fallback 자동 |

### 2.5 세션 관리

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| `--resume` + PR URL | Claude Code | `nv resume github.com/.../pull/123` |
| `--fork-session` / `/branch` | Claude Code | 실패 시 분기 백업 |
| Worktree 자동 isolation | Claude Code v2.1.143+ | reviewer 병렬 적용 격리 |
| Checkpoint (Esc×2) | Claude Code | `/rollback` 확장 (세션 스냅샷) |

### 2.6 TTY/UI 개선

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| Fullscreen mode | Claude Code | ratatui 패널 업그레이드 |
| Thinking progress inline | Claude Code | 상태 바 단계 아이콘 |
| Improved scrolling | Claude Code | `/diff` 스크롤 성능 |
| Table rendering (CJK·하이퍼링크) | Claude Code | `/status` 표 + PR 링크 |

### 2.7 Workspace 관리

| 기능 | 출처 | Nerve 차용 포인트 |
|------|------|------------------|
| `.claude/worktrees/` 자동 | Claude Code | `.nerve/worktrees/<patch-id>` |
| MCP tool search | Claude Code | reviewer가 외부 도구 호출 |
| Environment isolation | Codex CLI v0.130 | Claude/Codex 각각 `CLAUDE_CONFIG_DIR` 격리 |

### 2.8 출처

- Claude Code Changelog — https://code.claude.com/docs/en/changelog.md
- Agent View Guide — https://code.claude.com/docs/en/agent-view.md
- `/goal` Command — https://code.claude.com/docs/en/goal.md
- Scheduled Tasks & `/loop` — https://code.claude.com/docs/en/scheduled-tasks.md
- Codex CLI Features — https://developers.openai.com/codex/cli/features
- Codex CLI v0.130 Reference — https://blakecrosley.com/guides/codex

---

## 3. 우선순위별 차용 제안

### Tier 1 — 비용 낮고 가치 큼 (1~2일)

#### 1a. 라이브 상태 바 (Agent View 스타일)

- **무엇을**: 인터랙티브 모드 헤더에 상시 한 줄
  ```
  nerve:claude:apply  lead⟳  rev✓  round 2/3  ⏱42s  $0.018
  ```
- **데이터 출처**: `RunReport.usage`(cost_microusd, tokens), refinement 카운터,
  `AgentEvent::Stdout/Stderr` 빈도
- **구현 위치**:
  - `crates/nerve-cli/src/main.rs:904` (`InteractiveLineEditor`) 위에
    `StatusBar` 구조체 추가
  - `crates/nerve-core/src/lib.rs`의 orchestrator → mpsc 채널로 상태 push
  - 아이콘 매핑: idle `∙`, thinking `⟳`, done `✓`, blocked `✗`
- **영감**: Claude Code agent view ✽/✻/∙/✢ + elapsed/spend 표시

#### 1b. `/goal` 명령 — 종료 조건 기반 자동 재시도

- **무엇을**: `/goal tests pass && diff applied` 형태로 종료 조건 등록
- **동작**: orchestrator가 max_rounds 이전이라도 조건 충족 시 stop;
  미충족이면 reviewer 피드백을 새 lead 프롬프트로 자동 재투입
- **Evaluator 설계**: LLM 호출 없이 **shell exit code + regex 매칭**만
  지원 (Claude Code는 Haiku 평가자를 쓰지만 Nerve는 deterministic check가
  도메인에 더 맞음)
- **구현 위치**:
  - `crates/nerve-config/src/lib.rs`에 `GoalSpec { check_cmd, success_pattern }`
    타입 추가
  - `crates/nerve-core/src/lib.rs`의 `run_synaptic_loop` 종료 조건 hook
  - `/goal` 슬래시 명령 핸들러는 `main.rs:1242` 매치 분기에 추가

#### 1c. `/templates` 검색·정렬 강화

- **무엇을**: 현재 단순 목록 → **substring 필터 + 사용 빈도 정렬**
- **구현**: 기존 팔레트 코드(`main.rs:836`)와 동일 패턴으로 sub-palette 추가;
  `.nerve/session-meta/template-usage.json`에 사용 카운터 누적

### Tier 2 — 중간 비용 (3~5일)

#### 2d. Worktree 격리된 `/apply`

- `/apply --worktree`: `.nerve/worktrees/<patch-id>` 에서 적용 → 성공 시
  main으로 머지, 실패 시 worktree만 제거
- 충돌 없는 다중 reviewer 병렬 적용의 기반
- `nerve-patch`가 이미 git 메타데이터를 다루므로 `git worktree add` 래퍼만
  추가 (Bash 호출, exit code 검사)

#### 2e. RPC 이벤트 스트리밍 확장

- 현재 4종 → 추가:
  - `round_start { round, lead_agent, reviewer_agent }`
  - `lead_stdout_chunk { round, bytes }`
  - `reviewer_stdout_chunk { round, bytes }`
  - `goal_check { goal_id, passed, output }`
  - `cost_update { tokens, cost_microusd }`
- 외부 UI(별도 `nerve-agent-view` 프로세스)에서 JSONL 소비 가능
- 구현: `main.rs:1824` emit 함수 일반화 + adapter `AgentEvent` pass-through

#### 2f. `/plan` (Plan mode)

- lead 호출 전 read-only 분석 → 단계 목록 출력 → 사용자 승인 후 실제
  dispatch
- 기존 `--dry-run`에 prompt prefix("write a plan only, no changes") +
  승인 UI를 얹는 형태
- 큰 리팩토링에서 토큰 낭비 방지에 큰 효과

### Tier 3 — 큰 작업 (1~2주)

#### 3g. ratatui 기반 진짜 멀티 패널 TUI

- `main.rs:1983` 스텁 → 실제 구현:
  - 좌: lead stream
  - 우: reviewer stream
  - 하단: status bar + diff peek
- crossterm + ratatui 의존 추가
- 별도 crate `nerve-tui`로 분리 (이미 nerve-implementation-plan.md에 예정됨)

#### 3h. 세션 fork / branch

- `nv branch <task-id>`: 실패한 lead 출력을 베이스로 분기 → 비교
- `.nerve/sessions/<parent>/<child>.json` 트리
- 모델 비교 실험에 유용

#### 3i. MCP 서버 연결

- reviewer가 외부 도구(LSP, 정적 분석)를 호출할 수 있게 MCP client 추가
- 가장 큰 작업이라 후순위

---

## 4. v0.2.0 권장 묶음

**Tier 1 a + b + c 세 가지를 v0.2.0으로 묶을 것을 권장한다.**

- 셋 다 기존 데이터/구조 위에 얹는 작업이라 상호 의존성이 없다.
- 인터랙티브 모드를 켰을 때 **즉시 체감되는 변화** 세 가지:
  1. 상시 노출되는 상태 바
  2. `/goal`로 자동 종료/재시도
  3. 검색 가능한 템플릿
- 예상 작업량: 1~2일, 단일 PR로 묶기 적합.

### 4.1 진입 파일·라인 요약 (Tier 1 작업 시작점)

| 작업 | 진입점 |
|------|--------|
| 상태 바 구조체 | `crates/nerve-cli/src/main.rs:904` (`InteractiveLineEditor` 인접) |
| orchestrator → 상태 채널 | `crates/nerve-core/src/lib.rs` (`run_synaptic_loop`) |
| `/goal` 슬래시 핸들러 | `crates/nerve-cli/src/main.rs:1242` 매치 분기 |
| `GoalSpec` 타입 | `crates/nerve-config/src/lib.rs` |
| `/templates` 검색 팔레트 | `crates/nerve-cli/src/main.rs:836` |
| 사용 카운터 영속화 | `.nerve/session-meta/template-usage.json` |

### 4.2 v0.2.0 검증 체크리스트

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p nerve-cli -- config validate
cargo run -p nerve-cli -- interactive   # 상태 바 시각 확인
cargo run -p nerve-cli -- "<task>" /goal "exit 0"   # goal 자동 종료 확인
```

---

## 5. Tier별 위험 요소

| Tier | 위험 | 완화 |
|------|------|------|
| 1a 상태 바 | raw TTY 모드와 다른 출력 간 깜빡임 | 출력 갱신 시 cursor save/restore (`\x1b[s` / `\x1b[u`) |
| 1b /goal | 조건 평가가 무한 루프 유발 | `max_refinement_rounds`는 그대로 hard ceiling 유지 |
| 1c 템플릿 검색 | 사용 카운터 동시성 | `nerve-core/src/store.rs`의 atomic write 패턴 재사용 |
| 2d worktree | git 버전·플랫폼 차이 | `nv doctor`에 `git --version` 검사 추가 |
| 2e RPC 확장 | 외부 컨슈머 호환성 | 이벤트에 `version` 필드 + unknown type ignore 가이드 |
| 3g ratatui | 의존성 트리 증가 | feature gate (`--features tui`)로 옵션화 |

---

## 6. 관련 문서

- 아키텍처 — `nerve-architecture.md`
- 구현 계획 — `nerve-implementation-plan.md`
- 사용자 가이드 — `nerve-101.md`
- README — `README.md`

본 제안서는 위 세 문서와 별개로, **v0.2.0 UX 개선 한 사이클**에 집중한다.
