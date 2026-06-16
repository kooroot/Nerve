# Nerve Loop-Engineering Roadmap (P0 → P2)

> Branch: `feat/loop-engineering-p0` · 출처: Claude Code + Codex changelog 벤치마크 (2026-06-16)
> North star: **Verification-Gated Friction Loop** — 수락 = (reviewer accepts AND deterministic verifier green). LLM 의견만으론 절대 수락 안 함.

## Context — 왜 이 로드맵인가

Claude Code와 Codex의 changelog를 벤치마킹해 Nerve의 loop/goal 방향에 맞는 항목만 추렸다.
드러난 **구조적 공백 3가지**:

1. **검증 게이트가 opt-in** — 사용자가 `check_cmd`(goal)를 주지 않으면 `run_goal_check`가 `Skipped`를 반환하고,
   종료가 reviewer의 `Lgtm` 단독으로 붕괴 (`crates/nerve-core/src/lib.rs` stop edge). 수락 = LLM 의견.
2. **루프가 fire-and-forget 블로킹** — `run_synaptic_loop`이 하나의 `RunReport`로 `.await`,
   `CancellationToken`/watch/mpsc 없음. RPC 이벤트는 종료 후 사후 재생(post-hoc), 라이브 아님.
   → pause/resume/cancel/update-goal 전부 불가. 모든 chat 제어 동사의 단일 의존성.
3. **plan/goal이 advisory** — `PlanReport`는 읽기전용 markdown, 루프로 가는 실행 핸드오프 없음.
   'Consensus'/'Tournament'는 진짜 multi-voter consensus 아님(단일 lead+reviewer / 2-후보).

각 항목은 *기능 따라가기*가 아니라 Nerve의 "lead/reviewer 적대적 마찰 + 비-LLM 검증" 테제를 **더 날카롭게** 만드는 것만 골랐다.

---

## 실행 엔진 — 스텝당 friction 루프 (= Nerve의 루프를 Nerve 빌드에 적용)

각 스텝 `Sx`마다:

```
1. DESIGN     서브에이전트(Plan, read-only): 파일/심볼·인터페이스·데이터변경·테스트플랜·리스크·롤백 스펙
2. IMPLEMENT  서브에이전트: 구현 + 단위/통합 테스트, cargo self-check
3. VERIFY     메인 에이전트 = 비-LLM 게이트: cargo build/test + clippy -D warnings + diff 정독
4. DOUBLE     codex: `codex exec review`(stdin 프롬프트) 독립 리뷰 — 정확성·안전성·테제 정렬
5. IMPROVE    메인/서브: 메인+codex 지적 반영 → VERIFY 복귀 (cargo green AND codex no-blocking 까지)
6. LAND       검증된 diff → 승인 시 커밋 (스텝당 단일 커밋)
```

매핑: **lead = IMPLEMENT · reviewer = codex · 비-LLM verifier gate = cargo · conflict policy = 머지 판단(메인)**.
수락 조건은 `(cargo 그린 AND codex 무차단)` — LGTM 의견만으론 넘기지 않는다.

---

## 로드맵 — 4 웨이브 / 15 스텝 (의존성 순)

같은 파일(`nerve-core/src/lib.rs`, `nerve-types/src/lib.rs`)을 깊게 건드리는 항목이 많아 **스텝은 순차** 진행.

### 🌊 Wave 0 — 머신러리 검증 + 저위험 윈
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| **S1** | accept-with-nits (등급제 verdict) | S | ✅ **DONE** (codex-verified) |
| **S2** | 탄력적 어댑터 spawn 재시도 | S | ✅ **DONE** (codex-verified) |
| S3 | fail-loud 컨텍스트 로딩 | S | ⬜ |

### 🌊 Wave 1 — 검증-게이트 코어 (north star)
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| **S4** | 상시 빌트인 Verifier 게이트 (test/build/lint/patch-applies) | M | ⬜ |
| S5 | OS 실행 샌드박스 (Seatbelt / bwrap+seccomp+Landlock) — S4 안전 의존성 | M~L | ⬜ |
| S6 | 스키마 강제 verdict 객체 (free-text LGTM 파싱 폐기) | M | ⬜ |
| S7 | distance-to-goal 진행 신호 (CheckResult에 score) | M | ⬜ |

### 🌊 Wave 2 — 조종 가능·지속 루프 (daemon v2)
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| S8 | 라운드 증분 체크포인트 (record_round → .nerve store) | M | ⬜ |
| **S9** | 논블로킹 라운드-이음새 데몬 v2 + 라이브 JSONL 스트림 | L | ⬜ |
| S10 | crossfire advisory → redirect/단락 | M | ⬜ |
| S11 | 승인-에스컬레이션 지속성 (sticky per run) | S | ⬜ |
| S12 | auto-mode 분류기 게이트 (implement↔apply) | M | ⬜ |

### 🌊 Wave 3 — plan/goal 핸드오프 + fleet
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| S13 | 실행형 plan → loop 핸드오프 (Steps → Task/PatrolTask) | L | ⬜ |
| S14 | Agent-Teams 조율 원장 (공유 task 원장 + mailbox + 파일락 claim) | L | ⬜ |
| S15 | Conductor 라이브 상태 + 일괄 cancel (S9 의존) | L | ⬜ |

---

## ⛔ Anti-patterns — 베끼지 말 것

1. `--yolo`/danger-full-access를 **기본**으로 — 검증 게이트 우회는 요란하게 명명된 명시적 위험 플래그여야지 기본이 아님. dry-run 우선 유지.
2. **LLM 의견뿐인 `/review`를 수락 게이트로** — 그게 Nerve가 메우려는 공백. severity 랭킹은 reviewer 채널 보강에만.
3. "생성 중 인터럽트 = kill"을 verifier/rollback 경로에 — 조종은 라운드 이음새에서만(큐잉). 돌아가는 check_cmd·in-flight NvPatch는 원자적 완료.
4. 수백 에이전트 재귀 중첩 — process-per-loop + 외부 supervisor(Codex `max_depth=1`) 유지. fleet은 Conductor 아래 수평 확장.
5. 'Consensus'를 진짜 합의처럼 / best-of-N을 정족수처럼 마케팅 — 추가하려면 정직하게 새 전략+판정 모델로.

---

## 진행 로그

### S1 — accept-with-nits (✅ DONE, 2026-06-16)

reviewer가 저-severity nit만 남았을 때(`ACCEPT_WITH_NITS` 토큰) 결정론적 체크가 green이면
**refine 라운드를 낭비하지 않고** 종료할 수 있는 등급제 verdict. `review_strictness`로 튜닝(Low/Normal 허용, High는 라운드 강제).

- `Verdict::AcceptWithNits` + `Verdict::accepts_under(nits_permitted)`, `ReviewStrictness::permits_nits()`, `RPC_SCHEMA_VERSION 1.0.0→1.1.0`.
- **핵심 안전 설계 (codex 3라운드 구동)**: 수락/적용은 **정책-독립** `nits_unverified` 게이트로 — 모든 conflict 정책·전략에서
  "실제 `Pass` 체크(Skipped 아님) + nits 허용 strictness"일 때만 accept/apply. LLM 의견만으론 절대 수락/적용 불가. Lgtm/RequestChanges/Block 불변.
- codex 이중 검증이 cargo-green + 메인 리뷰가 놓친 테제 위반 3건을 연쇄로 적발: stop-edge Skipped 우회 → AbortOnConflict apply 게이트 → 나머지 정책+persistence. R4에서 "No BLOCKING".
- 검증: `cargo test --workspace` 303 green, `clippy -D warnings` clean, codex clean. 회귀 테스트 ~18개.
- ⚠️ 알려진 제약: **S4(상시 Verifier) 전까진 사실상 무동작** — goal 없으면 Skipped라 nit 수락 안 되고 refine로 강등. 의도된 안전 기본값이며 S4가 실제 Pass를 공급하면 활성화.

### S2 — 탄력적 어댑터 spawn 재시도 (✅ DONE, 2026-06-16)

`SubprocessAdapter`가 `spawn(2)` 일시 실패(EAGAIN/ENOMEM/ETXTBSY/EINTR)에 한해 지수 백오프로 재시도.
ENOENT/EACCES(바이너리 부재·권한)는 **fail-fast** — 깨진 어댑터를 재시도로 가리지 않음(테제: 검증 게이트 약화 금지).

- `is_transient_spawn_error` (errno→`ErrorKind` 분류), `spawn_with_retry` (제네릭·단위테스트 가능), `spawn_retry_backoff` (오버플로 안전).
- 설정 노브 `orchestration.adapter_spawn_retries: Option<u32>` → `AdapterLimits`로 `default_adapters_with_limits`에 배선. 기본 2회, `MAX_SPAWN_RETRIES=10` 클램프.
- **codex 이중 검증이 cargo-green + 메인 리뷰가 놓친 BLOCKING 2건 적발**:
  (1) `ETXTBSY`는 `ResourceBusy`가 아니라 `ErrorKind::ExecutableFileBusy`로 매핑됨(codex가 `from_raw_os_error`로 실증) → 분류 누락 수정.
  (2) 무한 재시도 + `1u32 << attempt` 오버플로(attempt≥32 panic) + 백오프가 타임아웃 밖 → 재시도/백오프 상한(shift≤16, per-attempt≤2s) 추가.
- 검증: `cargo test --workspace` green (어댑터 40 tests, +오버플로/클램프/errno 회귀 5개), `clippy -D warnings` clean.
