# Nerve 시스템 상세 구현 계획

## Context (배경)

`nerve-architecture.md`는 Nerve의 비전(Synaptic Loop, Collaborative Friction)과
주요 컴포넌트(Nerve-Core, Synapse, Lead/Reviewer Agent, Fusion Module)를
상위 수준에서 정의한다. 그러나 다음과 같은 실행 단위 결정은 비어 있다.

- 두 모델을 **어떻게** 동시에 spawn하고 stream을 동기화할 것인가
- Synapse 버퍼의 **데이터 형상**과 영속성
- Lead ↔ Reviewer 간 **메시지 프로토콜**
- `nv-patch`의 **롤백 단위**와 적용 순서
- Profile 매칭과 strategy 분기의 **결정 트리**

본 계획은 이 빈칸들을 채우고, **MVP → Phase 2 → Phase 3** 단계별로 검증
가능한 마일스톤을 정의한다. 호출 방식은 **CLI 서브프로세스** (claude/codex
바이너리를 `tokio::process::Command`로 spawn) 방식으로 결정.

목표: 첫 빌드 완료 시 `nv "add a /health endpoint"` 한 줄로 두 모델이
병렬 실행되고, 합의된 patch 한 개가 작업 디렉터리에 적용되는 것.

---

## Workspace 구조 (Cargo Workspace)

루트에서 단일 바이너리가 아닌 **workspace** 로 시작한다 — 향후 `nerve-tui`
와 `nerve-daemon` 분리를 무리 없이 흡수하기 위함.

```
Nerve/
├── Cargo.toml                # [workspace], members = [...]
├── nerve.config.json         # 사용자 워크스페이스에 복사되는 default
├── crates/
│   ├── nerve-core/           # 도메인 로직: orchestrator, synapse, fusion
│   ├── nerve-adapter/        # ModelAdapter trait + Claude/Codex impl
│   ├── nerve-patch/          # nv-patch 데이터 모델, apply/rollback
│   ├── nerve-config/         # nerve.config.json 로딩, profile 매칭
│   └── nerve-cli/            # `nv` 바이너리 (clap), 메인 진입점
└── tests/
    └── e2e/                  # 통합 테스트 (mocked adapter)
```

- crate 간 의존: `cli → core → {adapter, patch, config, synapse}`
- 모든 crate `edition = "2024"`, MSRV는 stable 최신.
- 공통 의존: `tokio` (full feature), `serde`, `serde_json`, `anyhow`,
  `thiserror`, `tracing`, `async-trait`.

---

## Phase 1 — MVP (목표: 동작하는 한 줄 명령)

**완료 정의**: `nv "<task>"` 실행 → 두 CLI를 병렬 spawn → Reviewer 피드백
1회 → Lead가 수정 → 합의된 unified diff를 stdout 출력 (apply는 `--apply`
플래그가 있을 때만).

### 1.1 `nerve-config` — 설정 로딩
- `Config::load()`: `./nerve.config.json` → `~/.config/nerve/config.json` →
  내장 default 순으로 fallback.
- `Profile::match_for(task: &Task) -> &Profile`: `match_rules`의 glob/keyword를
  순회. 매치 없으면 `roles` 기본값 사용.
- `globset` crate로 glob 평가, keyword는 단순 substring 매치.
- 검증: `serde` `deny_unknown_fields`, 누락 시 명확한 에러 메시지.

### 1.2 `nerve-adapter` — 모델 추상화
```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn id(&self) -> &'static str;             // "claude-code" | "codex"
    async fn dispatch(
        &self,
        task: &Task,
        cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput>;
}
```

- `AgentEvent`: `Stdout(String) | Stderr(String) | ToolCall(...) | Done(AgentOutput)`.
- `ClaudeCodeAdapter`: `claude -p "<prompt>" --output-format stream-json --verbose`
  spawn, JSON 라인을 `AgentEvent`로 파싱.
- `CodexAdapter`: `codex exec --json "<prompt>"` 동일 패턴.
- 2026-05-06 실제 shape 확인: Claude Code 2.1.128은 `stream-json`에
  `--verbose`가 필요하며 assistant text는 `message.content[].text`,
  Codex CLI 0.128.0은 `item.completed.item.text`에 출력한다.
- 각 adapter는 자신만의 prompt template을 보유 (Lead용 / Reviewer용 분리).
- 종료 코드 != 0 이면 stderr를 포함한 `AdapterError` 반환.

### 1.3 `nerve-core` — Synapse + Orchestrator

**Synapse (in-memory only, MVP)**:
```rust
pub struct Synapse {
    inner: Arc<RwLock<SynapseState>>,
}
struct SynapseState {
    task: Task,
    lead_output: Option<AgentOutput>,
    reviewer_feedback: Option<ReviewerFeedback>,
    rounds: Vec<RoundRecord>,   // refinement history
}
```
- `tokio::sync::broadcast`로 모든 이벤트를 fan-out (CLI streaming용).
- `RwLock`은 `tokio::sync::RwLock` (lock-holding await 안전).

**Orchestrator** (`run_synaptic_loop`):
1. `Profile::match_for(task)` → lead/reviewer adapter 선택.
2. `tokio::join!(lead.dispatch(...), reviewer.dispatch(...))` — Reviewer는
   초기에 "lead가 끝날 때까지 대기 후 비평" 모드로 시작 (MVP는 순수
   병렬 대신 **순차+1라운드 비평**으로 단순화).
3. Reviewer 피드백을 lead에게 second prompt로 재전송.
4. `max_refinement_rounds`만큼 lead refinement를 허용하고, reviewer가 `LGTM`
   반환하거나 허용된 refinement를 모두 소진하면 loop 종료.
5. `conflict_policy`(`lead_priority` 등)에 따라 최종 patch 선정.

### 1.4 `nerve-patch` — diff 데이터 모델
- `NvPatch { id: Ulid, base_commit: Option<String>, files: Vec<FilePatch> }`
- `FilePatch`는 unified diff 문자열을 보유 (MVP는 `similar` crate로 생성/적용).
- `apply(cwd: &Path)` / `rollback(cwd: &Path)` — patch 별 dry-run 우선.
- `--apply` 없이는 stdout에 colored diff만 출력.

### 1.5 `nerve-cli` — `nv` 바이너리
- `clap` derive, 서브커맨드:
  - `nv "<prompt>"` → 기본 dispatch
  - `nv apply <patch-id>` (Phase 2)
  - `nv rollback <patch-id>` (Phase 2)
  - `nv config validate`
- 환경변수 `NERVE_LOG=debug` → `tracing_subscriber` EnvFilter.

### 1.6 Phase 1 검증
- **단위 테스트**: config loading, profile 매칭, patch apply round-trip.
- **통합 테스트**: `MockAdapter`(고정 응답)로 orchestrator 1라운드 시나리오.
- **수동 E2E**: `claude --version`, `codex --version` 확인 후
  `cargo run -- "rename foo to bar in src/lib.rs"` → diff 출력 확인.

---

## Phase 2 — Cross-Firing & 영속성

MVP가 "순차+1라운드"에 머무른 부분을 **진짜 병렬 + 실시간 cross-firing**으로
끌어올린다.

### 2.1 실시간 Reviewer (file watcher)
- `notify` crate로 lead가 만들어내는 임시 파일(`.nerve/scratch/`)을 감시.
- 변경 감지 시 reviewer에게 incremental prompt 전송 — "지금까지의 변경분만
  보고 보안/성능 이슈 보고하라".
- 2026-05-06 현재: 외부 watcher dependency 없이 `.nerve/scratch`를 polling해
  lead 실행 중 파일 변경을 감지하고 reviewer `crossfire` hook을 호출한다.
  결과는 `RunReport.crossfire_feedback`에 기록된다.

### 2.2 Synapse 영속화
- `sled` 또는 `redb`로 round history를 디스크에 저장.
- 키: `task_id/round_n/{lead|reviewer}`, 값: `AgentOutput` JSON.
- `nv history`, `nv resume <task-id>` 커맨드 추가.
- 2026-05-06 현재: Phase 2의 첫 영속화 경로는 별도 DB 없이
  `.nerve/sessions/{task_id}.json`에 `RunReport` 전체를 저장하는 파일 기반
  구현으로 시작했다. `nv history`와 `nv resume <task-id>`가 이 저장소를
  읽는다.

### 2.3 `nv-patch` 인덱스
- `.nerve/patches/{ulid}.patch` + `.nerve/patches/index.json`.
- `nv apply <id>` / `nv rollback <id>` / `nv list` 활성화.
- atomic 보장: temp file → rename, 실패 시 자동 rollback.
- 2026-05-06 현재: patch 본문은 `.nerve/patches/{id}.json`, 인덱스는
  `.nerve/patches/index.json`에 저장한다. `nv list`, `nv apply <id>`,
  `nv rollback <id>`가 활성화됐고, JSON 저장은 temp file 후 rename한다.
- 2026-05-06 현재: `NvPatch`의 파일 쓰기 역시 sibling temp file에 먼저
  기록한 뒤 rename으로 commit한다. rename 실패 시 temp file을 정리한다.

### 2.4 Conflict Policy 확장
- `lead_priority` (default), `reviewer_priority`, `merge_attempt`,
  `abort_on_conflict` 4가지를 `enum ConflictPolicy`로 구현.
- `merge_attempt`은 `git merge-file` 호출 (system `git` 의존).
- 2026-05-06 현재: `merge_attempt`는 lead patch와 reviewer suggested patch를
  합성한다. 서로 다른 파일은 결합하고, 같은 파일의 create/modify 충돌은
  `git merge-file -p`로 병합한다. delete/rename 등 지원하지 않는 조합이나
  merge conflict는 오류로 중단한다.

### 2.5 Phase 2 검증
- 통합 테스트에 `tempdir` + 실제 파일 변경 시나리오 추가.
- `proptest`로 patch apply/rollback round-trip property 검증.

---

## Phase 3 — UX (cmux 3분할 + Profile 풀 동작)

### 3.1 cmux 자동 레이아웃
- `nv` 실행 시 `CMUX_SESSION` 환경변수 감지하여:
  - 좌측 패널: lead의 `tracing` 이벤트 stream
  - 우측 상단: reviewer 이벤트 stream
  - 우측 하단: orchestrator 상태 (round, conflict policy, elapsed)
- cmux이 없으면 단일 stream으로 fallback.
- 구현: `crates/nerve-tui/` 별도 crate, `ratatui` 사용 (cmux 미사용 시).
- 2026-05-06 현재: 별도 TUI crate 전 단계로 `nv --tui "<task>"` fallback을
  구현했다. 실행 결과를 Lead / Reviewer / Orchestrator 3분할 terminal
  summary로 렌더링한다.

### 3.2 Profile 매칭 고도화
- `match_rules`에 **AND/OR 조합** 지원:
  ```json
  { "all": ["*.rs", "contract"], "any": ["fix", "audit"] }
  ```
- `review_strictness: "high" | "normal" | "low"`를 reviewer prompt에 주입.
- Profile 별 `max_refinement_rounds` override 허용.
- 2026-05-06 현재: 기존 배열 shorthand와 함께 `{ "all": [...], "any": [...] }`
  logical rule object를 지원한다.

### 3.3 Strategy 플러그인
- `default_strategy`를 trait `Strategy`로 추상화:
  - `consensus` (default): refinement loop until LGTM
  - `tournament`: 두 모델이 각자 patch 생성 → 제3 reviewer가 선택
  - `pipeline`: lead → reviewer → lead 단방향 1패스
- `Strategy`는 `ModelAdapter`처럼 trait 객체로 plug.
- 2026-05-06 현재: trait plugin화 전 단계로 core strategy dispatch를
  구현했다. `consensus`는 기존 refinement loop, `pipeline`은 1회 구현+1회
  리뷰 후 종료, `tournament`는 lead/reviewer 양쪽 candidate 생성 후
  cross-review로 수락 candidate를 고른다.

### 3.4 Phase 3 검증
- ratatui snapshot 테스트 (`insta`).
- cmux session 내에서 수동 E2E.
- 2026-05-06 현재: editor/shell integration용으로 `nv daemon`을 구현했다.
  stdin 한 줄당 prompt 하나를 처리하고 stdout 한 줄당 JSON `RunReport` 하나를
  출력한다. `--once`는 단일 prompt 처리 후 종료한다.

---

## 핵심 데이터 모델 요약

```rust
pub struct Task { id: Ulid, prompt: String, cwd: PathBuf, started_at: DateTime<Utc> }

pub struct AgentOutput {
    agent_id: &'static str,
    raw_text: String,
    proposed_patch: Option<NvPatch>,
    tool_calls: Vec<ToolCall>,
    cost: Option<UsageStats>,
}

pub struct ReviewerFeedback {
    verdict: Verdict,                 // LGTM | RequestChanges | Block
    issues: Vec<Issue>,
    suggested_patch: Option<NvPatch>,
}

pub enum AgentEvent { Stdout(String), Stderr(String), Tool(ToolCall), Done(AgentOutput) }

pub struct RoundRecord { round: u8, lead: AgentOutput, reviewer: ReviewerFeedback }
```

---

## 변경/생성될 핵심 파일 (Phase 1 기준)

- `Cargo.toml` (workspace 루트)
- `crates/nerve-config/src/{lib.rs, profile.rs, schema.rs}`
- `crates/nerve-adapter/src/{lib.rs, claude.rs, codex.rs, mock.rs}`
- `crates/nerve-core/src/{lib.rs, synapse.rs, orchestrator.rs}`
- `crates/nerve-patch/src/{lib.rs, diff.rs, apply.rs}`
- `crates/nerve-cli/src/{main.rs, args.rs}`
- `nerve.config.json` (default template, 아키텍처 문서의 예시 그대로)
- `tests/e2e/mvp.rs` (MockAdapter 기반 1라운드 시나리오)

---

## 위험 요소 & 미결 사항

1. **CLI 출력 포맷 변동성**: `claude -p --output-format stream-json --verbose` /
   `codex exec --json` 의 스키마가 두 도구 모두 비공식적으로 변할 수
   있다. → adapter 단에서 schema versioning 가드 필요. Phase 1에서는
   "필드 누락 시 raw text fallback".
2. **인증**: 두 CLI 모두 사용자 머신에 미리 로그인되어 있다고 가정.
   `nv doctor` 커맨드로 사전 체크한다.
   2026-05-06 현재 `nv doctor`는 config를 검증하고 real adapter 모드에서
   `claude`, `codex` 바이너리가 `PATH`에 있는지 확인한다. mock adapter
   모드는 외부 바이너리 없이 통과한다.
3. **동시 파일 쓰기 충돌**: 두 모델이 동시에 같은 파일을 수정하려 할
   때 — MVP는 lead만 실제 파일 시스템에 쓰고, reviewer는 항상
   `.nerve/scratch/reviewer/` 로 격리. 2026-05-06 현재 cross-firing은
   `.nerve/scratch` watcher와 reviewer hook으로 처리한다.
4. **비용 통제**: refinement loop이 무한 반복되지 않도록
   `max_refinement_rounds` 외에 **token budget** 상한을 둔다.
   2026-05-06 현재 `orchestration.max_total_tokens`와
   `orchestration.max_estimated_cost_microusd`를 지원하며, adapter가
   `UsageStats`를 보고하면 초과 시 loop을 중단하고 apply를 막는다.
5. **Codex CLI 행동**: `codex exec`이 stateful session을 가질지 stateless
   one-shot인지에 따라 reviewer prompt 전략이 달라짐 → Phase 1 spike
   필요 (codex CLI 동작 1시간 조사).

---

## 검증 (End-to-End)

Phase 1 완료 시 다음 4단계로 검증한다.

```bash
# 1. 단위 + 통합 테스트
cargo test --workspace

# 2. clippy + fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# 3. CLI smoke test (mock adapter)
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add log line to main.rs"

# 4. 실제 모델 E2E (claude + codex 로그인 상태에서)
cargo run -p nerve-cli -- "rename function bar to baz in tests/fixture.rs"
# → stdout에 unified diff 출력, --apply 부재 시 파일 변경 없음
```

각 Phase 종료마다 위 4단계 + Phase 고유 시나리오를 모두 통과해야 다음
Phase 진입.

---

## 추천 작업 순서 (Phase 1 내부)

1. Workspace 스캐폴딩 + `nerve-config` (Day 1)
2. `nerve-patch` (diff 생성/적용, mock 데이터로 단위 테스트) (Day 1–2)
3. `nerve-adapter` trait + `MockAdapter` (Day 2)
4. `nerve-core` orchestrator (MockAdapter로 1라운드 동작) (Day 3)
5. `nerve-cli` 진입점 (Day 3)
6. **Spike**: `claude -p` / `codex exec` 실제 출력 관찰 후 진짜 adapter 구현
   (Day 4–5)
7. E2E 시나리오 통과 후 Phase 1 마무리.
