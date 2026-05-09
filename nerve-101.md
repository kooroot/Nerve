# Nerve 101

> **한 줄 요약** — `nv "<task>"` 한 줄로 lead 모델이 코드 패치를 만들고,
> reviewer 모델이 비평하고, 합의된 unified diff가 dry-run으로 출력된다.
> 파일은 `--apply`가 있을 때만 변경된다.

---

## 1. 프로젝트 개요

| 항목 | 내용 |
|------|------|
| 정체 | Rust로 작성된 **CLI 오케스트레이터** (`nv` 바이너리) |
| 핵심 가치 | 단일 모델의 자기합리화를 깨고, **lead vs reviewer**의 마찰로 패치 품질을 올린다 |
| 설계 | Cargo workspace, 6개 crate, 로드맵 기능과 리뷰 이슈 #1-#11 처리 완료 |
| 모델 호출 | CLI 서브프로세스 — `claude -p ... --output-format stream-json --verbose` 와 `codex exec --json ...` |
| 안전 모델 | dry-run 기본, SHA-256 해시 검증, staged write, 멀티파일 snapshot rollback |
| 라이선스 | MIT |
| 저장소 | https://github.com/kooroot/Nerve |

핵심 데이터 흐름:

```
Task → Profile 선택 → Strategy(consensus/pipeline/tournament)
     → lead.implement → reviewer.review
     → (REQUEST_CHANGES면) lead.refine 반복 → conflict_policy로 최종 patch 결정
     → dry-run 출력 (또는 --apply 시 파일 적용)
```

---

## 2. 환경 요구사항

| 카테고리 | 필수 | 비고 |
|----------|------|------|
| OS | macOS / Linux | Windows는 미테스트 |
| Rust | stable, edition 2024 (1.85+ 권장) | `rustup update stable` |
| `git` | 시스템 PATH | `merge_attempt` 정책에서 `git merge-file` 사용 |
| `claude` CLI | real 모드만 필요 | 2.1.128+에서 동작 확인. 사전 로그인 필수 |
| `codex` CLI | real 모드만 필요 | 0.128.0+에서 동작 확인. 사전 로그인 필수 |

mock 모드는 외부 CLI 없이 결정적 응답으로 동작 — 학습·테스트·개발 시 권장.

---

## 3. 5분 시작

```bash
# 1) 클론 & 빌드
git clone https://github.com/kooroot/Nerve.git
cd Nerve
cargo build

# 2) 설정 검증
cargo run -p nerve-cli -- config validate

# 3) mock으로 흐름 체험 (외부 CLI 불필요)
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add a health endpoint"

# 4) 시스템에 설치
cargo install --path crates/nerve-cli

# 5) 사전 점검
nv doctor                  # real 모드: claude/codex가 PATH에 있어야 통과
NERVE_ADAPTER=mock nv doctor

# 6) 실제 모델로 실행 (인증된 claude/codex CLI 필요)
nv "rename foo to bar in src/lib.rs"        # dry-run
nv --apply "rename foo to bar in src/lib.rs"
```

---

## 4. CLI 사용법 (clap 서브커맨드)

| 명령 | 동작 |
|------|------|
| `nv "<task>"` | 기본 dispatch (dry-run) |
| `nv --apply "<task>"` | 결과 patch를 파일에 적용 |
| `nv --json "<task>"` | `RunReport` JSON을 stdout으로 |
| `nv --tui "<task>"` | Lead/Reviewer/Orchestrator 3분할 터미널 요약 |
| `nv --adapter mock "<task>"` | 결정적 mock 어댑터 사용 |
| `nv history [--json]` | `.nerve/sessions/*` 목록 |
| `nv resume <task-id> [--json]` | 저장된 `RunReport` 출력 |
| `nv list [--json]` | `.nerve/patches/index.json`의 patch 목록 |
| `nv apply <patch-id>` | 저장된 patch 재적용 |
| `nv rollback <patch-id>` | 저장된 patch 되돌리기 |
| `nv doctor` | config + 어댑터 사전 점검 |
| `nv daemon [--once]` | stdin 한 줄 = 한 task, stdout 한 줄 = JSON report (에디터 통합용) |
| `nv config validate` | 현재 작업 디렉터리의 `nerve.config.json` 검증 |

**환경변수**:
- `NERVE_ADAPTER=real|mock` (기본 `real`)
- `NERVE_LOG=warn|info|debug|trace` (기본 `warn`, `tracing-subscriber` EnvFilter)

---

## 5. 설정 파일 (`nerve.config.json`)

로딩 순서:
1. `./nerve.config.json`
2. `~/.config/nerve/config.json`
3. 내장 default (Cargo 바이너리에 임베드)

핵심 필드:

```json
{
  "orchestration": {
    "default_strategy": "consensus",         // consensus | pipeline | tournament
    "max_refinement_rounds": 2,              // 0..=5
    "conflict_policy": "lead_priority",      // 아래 표 참조
    "max_total_tokens": 200000,              // 선택, 사용량 보고하는 어댑터에서 적용
    "max_estimated_cost_microusd": 5000000   // 선택, 마이크로 USD 단위
  },
  "roles": { "architect": "claude-code", "reviewer": "codex" },
  "profiles": [
    {
      "id": "blockchain_dev",
      "match_rules": ["*.rs", "*.sol", "contract"],
      "lead": "claude-code",
      "reviewer": "codex",
      "review_strictness": "high"            // low | normal | high
    }
  ]
}
```

| `conflict_policy` | 의미 | 현재 상태 |
|-------------------|------|----------|
| `lead_priority` | lead patch 우선 | 동작 |
| `reviewer_priority` | reviewer suggested patch 우선 | 동작 |
| `merge_attempt` | `git merge-file`로 합성 시도. conflict marker가 생겨도 결과를 보존 | 동작 |
| `abort_on_conflict` | reviewer가 `LGTM`이 아니면 apply 차단 | 동작 |
| `reviewer_block` | reviewer `BLOCK`일 때 apply 차단 | 동작 |
| `manual` | 항상 auto-apply 차단, patch를 수동 처리 대상으로 남김 | 동작 |

`match_rules`는 두 가지 형식 지원:
- 배열 shorthand: `["*.rs", "fix"]` — keyword는 prompt, glob은 prompt 안의 path token과 변경된 Git path에서 수집한 `task.context_paths`에 매칭
- 논리 객체: `{ "all": ["*.rs"], "any": ["audit", "security"] }`

`max_refinement_rounds`는 reviewer 호출 수가 아니라 lead refinement 시도 횟수다.
예를 들어 `2`면 초기 review 1회와 refinement 이후 review 최대 2회까지 발생할 수 있다.

---

## 6. 워크스페이스 구조

```
crates/
  nerve-cli/       `nv` 바이너리, clap, 터미널 출력
  nerve-core/      Synapse 상태, refinement loop, conflict 정책, scratch watcher
  nerve-adapter/   ModelAdapter trait, MockAdapter, SubprocessAdapter
  nerve-config/    nerve.config.json 로딩, profile 매칭
  nerve-patch/     NvPatch, SHA-256 검증, apply/rollback, unified-diff 파서
  nerve-types/     Task, AgentEvent, AgentOutput, ReviewerFeedback, RoundRecord, Verdict
crates/nerve-cli/tests/            e2e 테스트 (mock + 실제 어댑터 fixture)
.nerve/                            런타임 상태 (git ignored)
  ├── sessions/{task-id}.json      RunReport 영속화
  ├── patches/{patch-id}.json      개별 patch 본문
  ├── patches/index.json           patch 인덱스 + applied 플래그
  ├── patches/index.lock           patch index 갱신 직렬화용 lock
  └── scratch/                     lead가 작성하는 임시 파일 (reviewer가 polling)
```

크레이트 의존: `cli → core → {adapter, config, patch, types}` (순환 없음).

---

## 7. 운영 팁 & 주의사항

- **항상 dry-run 먼저** — `nv "<task>"`는 절대 파일을 건드리지 않는다. `--apply`를 명시적으로 붙여야 적용.
- **mock 모드로 워크플로우 검증** — `NERVE_ADAPTER=mock`은 결정적이라 CI/디버깅에 적합.
- **세션 재현** — 모든 run은 `.nerve/sessions/{id}.json`에 저장됨. `nv resume <id>`로 동일 분석 재출력.
- **patch 인덱스로 후처리** — `nv list`로 과거 patch 목록을 보고 `nv apply <patch-id>` / `nv rollback <patch-id>`로 별도 시점에 적용·롤백 가능. session report의 `applied` 상태도 함께 갱신된다.
- **doctor는 실행 가능 파일을 확인** — real 모드에서는 `PATH`의 `claude`/`codex`가 파일이면서 Unix execute bit가 있는지 확인한다.
- **`max_refinement_rounds`는 refinement 횟수** — reviewer 호출 횟수와 1:1이 아니다. 비용 예측 시 초기 review + refinement 후 review를 함께 계산한다.
- **glob profile은 context path에 의존** — CLI가 prompt 안 path token과 `git diff --name-only HEAD` 결과를 `Task.context_paths`로 채워 glob rule을 활성화한다.

---

## 8. 리뷰 이슈 처리 현황

현재 GitHub open issue는 없다. 리뷰에서 발견된 #1-#11은 `a1becc5`에서 처리됐다.

| 영역 | 보완된 내용 |
|------|-------------|
| adapter verdict parsing | reviewer 응답 첫 verdict token만 파싱해 `LGTM: no blockers` 오분류 방지 |
| adapter JSONL diff extraction | assistant-authored text만 diff parser에 넣고 tool result/input 문자열은 제외 |
| adapter issue summary | `Issue.message`에서 leading verdict line을 제거하고 실제 지적을 구조화 |
| UTF-8 truncation | crossfire/TUI truncation이 char boundary를 보존 |
| `merge_attempt` | `git merge-file` conflict exit code를 fatal로 보지 않고 conflict marker 출력 보존 |
| conflict policy | `abort_on_conflict`, `reviewer_block`, `manual` semantics를 명시적으로 구현 |
| profile glob matching | CLI가 context path를 수집해 glob rule이 production path에서도 동작 |
| doctor | Unix execute bit까지 검사 |
| store consistency | `nv apply`/`rollback`이 patch index와 session report applied 상태를 함께 갱신 |
| store concurrency | JSON write는 unique temp file을 쓰고 patch index 갱신은 lock으로 직렬화 |

---

## 9. 개발 검증 (CI 등가)

```bash
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p nerve-cli -- config validate
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "smoke task"
```

테스트가 커버하는 동작:
- 기본 config 로딩, keyword/glob/logical profile 매칭
- mock lead/reviewer가 LGTM까지 refine
- patch apply/rollback 라운드트립, hash mismatch 거부
- create/modify/delete/rename + rename-with-content unified diff
- 멀티파일 mid-apply 실패 시 atomic rollback
- 경로 traversal/symlink 탈출 거부
- session 영속화, patch 인덱스, history/resume/list/apply/rollback
- `consensus`/`pipeline`/`tournament` 전략 분기
- `merge_attempt` patch 합성과 conflict marker 보존, scratch crossfire watcher
- `--json` 보고, `--tui` 3분할, `daemon` line-mode

---

## 10. 어디서부터 코드를 읽을지

| 궁금한 것 | 시작 파일 |
|----------|----------|
| `nv` 명령이 어떻게 dispatch 되는가 | `crates/nerve-cli/src/main.rs` |
| 전체 refinement 루프 | `crates/nerve-core/src/lib.rs` (`run_synaptic_loop`) |
| `claude` / `codex` 호출 | `crates/nerve-adapter/src/lib.rs` (`SubprocessAdapter`) |
| profile 매칭 규칙 | `crates/nerve-config/src/lib.rs` (`Profile::matches`) |
| patch 적용 안전성 | `crates/nerve-patch/src/lib.rs` (`NvPatch::apply`) |
| 세션 영속화 | `crates/nerve-core/src/store.rs` |
| 공유 타입 | `crates/nerve-types/src/lib.rs` |

---

## 11. 함께 보면 좋은 문서

- [README.md](./README.md) — 제품 소개와 quick start
- [nerve-architecture.md](./nerve-architecture.md) — 시스템 비전과 컴포넌트 정의
- [nerve-implementation-plan.md](./nerve-implementation-plan.md) — Phase 1/2/3 단계별 구현 계획
