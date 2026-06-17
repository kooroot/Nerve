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
| **S3** | fail-loud 컨텍스트 로딩 | S | ✅ **DONE** |

### 🌊 Wave 1 — 검증-게이트 코어 (north star)
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| **S4** | 상시 빌트인 Verifier 게이트 (test/build/lint/patch-applies) | M | ✅ **DONE** (codex-verified) |
| S5 | OS 실행 샌드박스 (macOS Seatbelt / Linux bwrap; raw seccomp/Landlock은 향후 강화) — S4 안전 의존성 | M~L | ✅ |
| S6 | 스키마 강제 verdict 객체 (free-text LGTM 파싱 폐기) | M | ✅ |
| S7 | distance-to-goal 진행 신호 (CheckResult에 score) | M | ✅ |

### 🌊 Wave 2 — 조종 가능·지속 루프 (daemon v2)
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| S8 | 라운드 증분 체크포인트 (record_round → .nerve store) | M | ✅ |
| **S9** | 논블로킹 라운드-이음새 데몬 v2 + 라이브 JSONL 스트림 | L | ✅ |
| S10 | crossfire advisory → redirect/단락 | M | ✅ |
| S11 | 승인-에스컬레이션 지속성 (sticky per run) | S | ✅ |
| S12 | auto-mode 분류기 게이트 (implement↔apply) | M | ✅ |

### 🌊 Wave 3 — plan/goal 핸드오프 + fleet
| Step | 항목 | effort | 상태 |
|---|---|---|---|
| S13 | 실행형 plan → loop 핸드오프 (Steps → Task/PatrolTask) | L | ✅ |
| S14 | Agent-Teams 조율 원장 (공유 task 원장 + mailbox + 파일락 claim) | L | ✅ |
| S15 | Conductor 라이브 상태 + 일괄 cancel (S9 의존) | L | ✅ |

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

### S3 — fail-loud 컨텍스트 로딩 (✅ DONE, 2026-06-16)

`collect_context_paths`가 프롬프트에서 **명시적 파일 참조**(경로 구분자 `/` 포함)를 실제 존재 여부로 검사 →
없으면 노란색 경고를 stderr로 큰소리(`⚠ referenced path not found: … — context may be incomplete`).
조용히 컨텍스트를 흘려서 루프가 잘못된 타깃에 "성공"하는 것을 차단(테제: 검증 게이트는 입력부터 신뢰 가능해야).

- `scan_context_paths(prompt, cwd) -> ContextScan { paths, missing_explicit }` (순수·테스트 가능), `context_path_exists`(절대/상대 해석).
- **오탐 방지**: `/` 없는 점-토큰(`v1.0.0`, 단독 `config.json`)은 경고 안 함 — 명시적 경로 참조만. git-derived 경로는 best-effort(경고 안 함).
- 기존 동작 보존: missing 경로도 `paths`에는 그대로 포함(프로필 glob 매칭 불변), 경고만 추가.
- 검증: `cargo test -p nerve-cli` green (+회귀 4개: missing/existing/오탐/절대경로), `clippy -D warnings` clean.

### S4 — 상시 빌트인 Verifier 게이트 (✅ DONE, 2026-06-16)

`/goal`이 없을 때 acceptance가 reviewer 단독 의견으로 붕괴(S1이 지적한 공백)하던 것을 닫는다.
빌트인 verifier가 프로젝트의 관용적 테스트/빌드 커맨드(Cargo/Go/npm) 또는 운영자 지정 argv를 `GoalSpec`으로
합성해, **기존 샌드박스 `GoalEvaluator`**(env_clear 화이트리스트·timeout·output cap·optional ulimit)를 그대로 재사용해
결정론적 `Pass`/`Fail`을 공급한다. 단일 funnel `run_synaptic_loop`의 맨 위에서 적용 → consensus/tournament 공통 상속.

- `nerve-core/src/verifier.rs`(신규): `detect_builtin_verifier`(Cargo.toml→`cargo test --quiet`, go.mod→`go test ./...`,
  package.json+test script→`npm test`; **no-test에 exit 0인 생태계만** 자동탐지해 spurious Fail 방지),
  `resolve_builtin_verifier(orch, cwd, exec_trusted)`, `project_verifier_consent_from_env()`.
- 설정: `orchestration.builtin_verifier { mode: off|auto|command, command, timeout_secs }`.
- **codex 이중 검증이 BLOCKING 2건을 연쇄 적발 (cargo-green + 메인 리뷰 통과 후)**:
  (1) **기본 `auto`가 동의 없이 repo 코드 실행** — `cargo test`는 build script를, `npm test`는 임의 스크립트를 돈다.
  env/timeout/ulimit 가드는 *자원* 제한이지 FS/네트워크 격리가 아님(OS 샌드박스는 S5, 미구현). → 기본값을 **`off`**로 바꿔
  명시적 opt-in이 아니면 절대 코드 실행 안 함(anti-pattern #1 준수). gate 없으면 CLI가 큰소리 경고(절대 침묵 아님).
  (2) **config provenance** — repo-local `./nerve.config.json`이 `auto`/`command`를 켜면 *프로젝트 작성자*가
  운영자 동의 없이 코드 실행을 opt-in 시킬 수 있음. → `ConfigSource{Project,User,Default}`로 출처를 추적,
  `load_from`이 스탬프. `Project` 출처의 실행 모드는 **out-of-band 운영자 동의**(`NERVE_TRUST_PROJECT_VERIFIER`, repo가 위조 불가)
  없이는 거부. User/Default(운영자 제어)만 신뢰. codex R-final: **LGTM, no new regression**.
- 안전 설계: `check_cmd[0]` PATH-safe(`/`,`\`,`..` 금지, no shell). 명시적 `/goal`은 항상 우선(빌트인 미적용).
  기존 core 테스트는 `task.cwd`에 마커 없으면 `Skipped` 유지 → 레거시 동작 불변.
- 검증: `cargo test --workspace` green (config 42, core 128, cli 49+16+4, adapter 40), `clippy -D warnings` clean.
  S4 회귀: 빌트인 resolve(off/auto/command/untrusted-project), provenance 스탬프/trust, CLI announce 3-way 스모크(off→warn / auto→notice / project-no-consent→refuse+warn / project+consent→run).
- ⚠️ S5(OS 실행 샌드박스)가 S4의 안전 의존성 — 그때까지 코드 실행은 운영자 명시 opt-in으로만.

### S6 — 스키마 강제 reviewer verdict (✅ DONE, 2026-06-16)

reviewer가 free-text 첫 줄에 더해 머신 리더블 `nerve-verdict` JSON 블록(verdict/summary/issues)을 내도록 프롬프트.
블록은 verdict를 **풍부하게**(summary·구조적 issue) 만들 뿐, 비수락(reject) 리뷰를 수락으로 **승격할 수 없다** —
lead가 위조하거나 reviewer가 인용한 forged `lgtm` 블록이 self-approve하는 경로를 구조적으로 차단.
North star: reviewer의 REJECTION이 ACCEPTANCE로 파싱되는 일은 절대 없어야 한다(false-reject는 라운드 1회 비용, false-accept는 보안 버그).

- `feedback_from_text`: `normalize_newlines`(CRLF/CR→LF, 캐리지리턴 밀반입 차단) → `parse_verdict_token`(free-text floor) +
  `all_json_objects`(전체 출력의 모든 JSON 객체, 중첩 포함)에서 most-severe verdict 객체. `honor_block_verdict`로 충돌 해소.
- `parse_verdict_token`: **reject-biased·case-sensitive**. `rejection_signal`이 전체 출력에서 standalone 대문자
  `BLOCK`/`REQUEST_CHANGES`(word-bounded)를 어디서든 잡아 수락을 veto; 그 외엔 첫 줄의 clean leading 대문자
  accept 토큰만 수락. accept 토큰은 바로 뒤 문자가 **종결자**(공백 또는 `.,:;!`)이거나 EOS일 때만 인정 —
  `is_accept_terminator` 화이트리스트가 reject-biased라 `'`(축약형)·`-`·`/`·alnum·`_`·`?`는 prose 취급. 소문자 "lgtm"도 prose.
- **fence 파싱 폐기 (R12)**: 파서는 `nerve-verdict` fence 구조를 **더 이상 해석하지 않는다**. lead가 통제하는 인용 콘텐츠에
  forged fence opener/closer를 심을 수 있어, 어떤 fence-pairing도 desync되어 인용 블록이 reviewer의 진짜 블록을 "소비"하고
  rejection을 드롭시킬 수 있음(#5/#6/#7/#12). fence scoping은 false-reject를 **줄일 뿐** 안전에 필요 없음 →
  `all_verdict_fences`·`fence_marker`·`is_verdict_opener_info` 삭제.
- `all_json_objects` + `structured_verdict_from_object` + `block_severity_rank`: **전체 출력**에서 모든 top-level JSON 객체를
  스캔(`{` 찾고 serde StreamDeserializer로 1개 Value 파싱, byte_offset만큼 전진; 실패 시 `{` 1바이트 skip; Object만 보존)해
  주변 prose·fence 마커 전부 무시. 각 객체를 파싱해 **가장 severe한** verdict 채택(Lgtm0<AcceptWithNits1<RequestChanges2<Block3,
  동률 last). **핵심 안전 논거**: rejection이 monotonic이라 모든 곳의 객체를 스캔해도 rejection을 **더** 찾을 뿐 덜 찾지 않음 —
  lead 인용/forged 객체는 severity를 **올릴 뿐**(허용되는 false-reject) reviewer의 진짜 rejection을 선점·드롭·하향 불가
  (순서·중첩·footer/preamble prose·forged fence·fence 길이 무관). #4(객체 간 most-severe)·#5/#6/#7/#12(fence 비의존)·
  #10(JSON 주변 prose 무시)·#11(객체 여럿 most-severe)을 한 invariant로 통합.
  `structured_verdict_from_object`: `verdict`는 `parse_verdict_value`로 **case-insensitive**(`"Block"`/`"BLOCK"` 드롭 안 됨, #9);
  `blocking`을 verdict 필드와 **독립·선행** 감지해 accepting/missing/garbage라도 `Block` 정규화(#2); issue element별 파싱.
  parseable verdict도 blocking issue도 없으면 None.
- **중복 키 fail-closed (R13)**: serde_json::Value는 중복 객체 키를 last-wins로 합쳐, 위조/중복된 `issues`(blocking 후 `[]`)·
  `verdict`(block 후 lgtm)·중첩 `severity`(blocking 후 info) 키가 "blocking이면 Block" invariant가 보기 **전에** rejection
  신호를 지울 수 있음(#13). `all_json_objects`가 각 객체를 `ScannedObject { value, has_duplicate_keys }`로 돌려주고,
  `json_has_duplicate_keys`가 그 객체의 **원본 바이트 스팬**(`&slice[..consumed]`)을 serde 비지터(`NoDuplicateKeys`)로 재파싱해
  **모든 깊이**에서 첫 중복 키에 에러 — `feedback_from_text`가 중복 키 객체를 **강제 Block**(synthetic blocking issue). monotonic이라
  severity를 **올릴 뿐**(lead가 인용한 중복 키 객체는 허용되는 false-reject) reviewer verdict를 못 내림. 서로 다른 형제 객체의
  반복 키는 중복 아님.
- `honor_block_verdict` + `max_verdict` floor: rejection 블록은 **항상** 존중; acceptance 블록(모든 블록이 accept일 때만 선택됨)은
  free-text 첫 줄이 explicit accept일 때만. terminality 경로 없음 — lead가 reviewer 첫 줄을 통제 못하므로 explicit accept 요구가
  self-approval 원천 차단. 추가로 free-text 첫 줄이 severity **floor**: honored 블록은 `max_verdict`로 floor 위로만 올림(toward
  rejection), 절대 못 내림 — forged `lgtm` 블록이 `ACCEPT_WITH_NITS`를 `LGTM`으로 승격(게이트 우회) 불가.
- **codex 이중 검증이 cargo-green + 메인 리뷰가 놓친 north-star 위반을 14라운드 연속 적발 (각 라운드 genuine reject→accept)**:
  (R1) leading 토큰 없는 prose reject + 말미 forged lgtm 블록 → terminality가 Lgtm으로 승격. → terminality 경로 제거,
  acceptance에 explicit free-text accept 요구.
  (R2) lgtm + blocking issue 모순 + issue 배열 fail-open(불량 1개가 전체 삭제). → 원천 `Block` 정규화 + element별 파싱 + raw severity 스캔.
  (R3) 수사적 `LGTM? No: BLOCK` / `LGTM. BLOCK: do not ship`(leading LGTM·mid-line BLOCK) / closing-fence 줄의 reject 누락.
  → case-sensitive 매칭 + `rejection_signal`이 전체 출력 스캔해 수락 veto.
  (R4) 앞선 genuine `{"verdict":"block"}` 블록이 뒤따르는 forged `lgtm` 블록에 last-wins로 폐기('LGTM at first glance, but…'
  자연스런 첫 줄 + lead 인용 forgery). → `last_verdict_fence`→`all_verdict_fences`, most-severe 채택으로 rejection monotonic화.
  (R5) lead가 인용한 **미종결** forged `lgtm` 블록의 "close"가 사실 reviewer의 다음 `nerve-verdict` opener라, 진짜 rejection
  블록이 plain text로 skip됨. → fence **resync**(nested opener=현 블록 종료+새 블록 시작, 양쪽 body 보존). codex가 제안한
  "현 후보 abandon"은 mirror(block-first) 방향에서 unsafe라 채택 안 함 — keep-both가 순서 무관 안전.
  (R6) reviewer의 진짜 rejection 블록이 **4-backtick**(정상 CommonMark) fence면 opener 인식 실패로 드롭→free-text LGTM 채택.
  → `fence_marker`로 backtick/tilde 3+ 일반화 + matching-length close + trailing info word 허용(같은 class 선제 차단).
  (R7) reviewer의 rejection 블록이 **EOF에서 close fence 누락**이면 "unclosed→discard" 규칙이 드롭→free-text LGTM 채택.
  → EOF 미종결 블록 보존·파싱(monotonic-rejection + honor-requires-free-accept invariant 덕에 strictly safer).
  (R8) `leading_accept_token`이 **아포스트로피 축약형** 첫 줄(`LGTM's not sufficient; this still ships attacker shell input`)을
  수락으로 파싱 — `'`가 catch-all accept 분기로 빠짐. → `is_accept_terminator` reject-biased 화이트리스트(공백·`.,:;!`만 종결)로
  교체해 `'`뿐 아니라 `-`·`/`·alnum·`_`·`?` 전 구두점 class를 한 번에 차단(단발 패치 아닌 invariant).
  (R9) **2건**: (a) **title-case verdict 값** rejection 블록(`{"verdict":"Block", issues:[{severity:"blocking"}]}`)이 clean
  leading `LGTM` 아래에서 드롭 — serde가 verdict를 case-sensitive(`block`만)로 파싱하고 blocking 스캔 **전에** `?` early-return.
  → `parse_verdict_value` case-insensitive 파싱 + blocking 감지를 verdict 필드와 **독립적·선행**(accepting/missing/garbage verdict라도
  blocking 있으면 Block). (b) lead 인용 forged `{"verdict":"lgtm"}` 블록이 free-text `ACCEPT_WITH_NITS`를 full `LGTM`으로 **승격**
  (High strictness에서 AcceptWithNits는 gated·Lgtm은 무조건 accept라 게이트 우회). → `max_verdict` severity **floor**: honored 블록은
  severity를 **올릴 뿐**(toward rejection) free-text floor 아래로 못 내림 — monotonic-rejection invariant를 acceptance-tier에도 확장.
  (R10) EOF 미종결 rejection 블록 **뒤에 footer 텍스트**(`...}\n\nThanks for the patch.`)가 붙으면 whole-body
  `serde_json::from_str`가 trailing text로 실패→블록 드롭→clean leading LGTM 채택. → `first_json_value`로 body에서 **첫 JSON 객체만**
  추출(첫 `{`부터 stream 1개)해 주변 prose 무시 — close fence 없는 EOF 블록도 verdict·blocking 신호 보존.
  (R11) R10의 "첫 객체" 추출이 역행 — fence body preamble에 lead가 인용한 `{"verdict":"lgtm"}` 객체가 reviewer의 진짜
  `{"verdict":"block"}` 객체(아래)를 **선점**. → `all_json_objects`로 body 내 **모든** 객체 추출 후 **most-severe** 채택 —
  most-severe-wins를 block 간뿐 아니라 **block 내부**에도 적용(lead 객체는 severity를 올릴 뿐=false-reject, 못 내림).
  (R12) lead가 인용한 **완전한 중첩 ```nerve-verdict``` fence**(opener+closer)가 reviewer 블록 안에 들어가면 fence pairing이
  desync — 인용 closer가 닫아버려 reviewer의 진짜 `{"verdict":"block"}` 객체가 어떤 fence body에도 안 들어가 드롭→free-text LGTM.
  → **fence 파싱 자체를 제거**하고 전체 출력에서 JSON 객체를 스캔(most-severe). fence desync 클래스(#5/#6/#7/#12) 영구 종결.
  (R13) serde_json::Value의 **중복 키 last-wins 합치기**로 `{"verdict":"lgtm","issues":[{"severity":"blocking"}],"issues":[]}`가
  `{"verdict":"lgtm","issues":[]}`로 붕괴 — blocking issue가 "blocking이면 Block" invariant 전에 사라져 clean leading LGTM 채택.
  → 각 객체의 원본 바이트를 `json_has_duplicate_keys`(serde `NoDuplicateKeys` 비지터, 모든 깊이 첫 중복 키에 에러)로 재검사,
  중복 키 객체는 **강제 Block**(monotonic이라 severity만 올림, reviewer rejection 못 내림).
  (R14) 진짜 `{"verdict":"block",...}` 객체가 **외부 객체/배열 안에 중첩**(`{"wrapper":[{...}]}`)되면, `all_json_objects`가
  외부 객체만 파싱하고 통째로 건너뛰어 중첩된 rejection을 한 번도 검사 안 함 → clean leading LGTM 채택. → `collect_objects`로
  **재귀**해 모든 깊이의 객체를 스캔(most-severe). 중첩이라 더 많은 verdict 객체를 찾을 뿐(monotonic), 중첩된 lead 객체는
  severity만 올림(못 내림)·중첩 accept 객체도 free-text explicit accept 없으면 self-approve 불가. serde_json 재귀 한도(128)가
  파싱 깊이를 제한하므로 stack overflow 없음.
- 검증: `cargo test --workspace` green (adapter 93 + 회귀 다수[모든 codex repro 포함], core 128, cli 49+16+4, config 42),
  `clippy -D warnings` clean.
  잔여 경계: reviewer가 스스로 첫 줄에 accept 토큰을 내고도 토큰 없는 prose로 모순 reject하는 self-contradiction은
  파서가 아닌 reviewer-protocol 위반이며, nerve-core 결정론적 verifier(acceptance = verdict-accept AND check Pass)가 backstop.

### S7 — distance-to-goal 진행 신호 (✅ DONE, 2026-06-16)

실패한 결정론적 체크가 이제 **pass-ratio**(distance-to-goal)를 실어, 루프가 binary pass/fail 대신
수치 진행 신호를 갖는다. **순수 additive 텔레메트리 + stall 힌트** — 수락은 여전히 진짜 green
체크(`CheckResult::Pass`, partial ratio 절대 아님)와 verdict accept를 **둘 다** 요구한다(게이트 불변).
North star(S7): progress 신호는 수락 게이트를 **약화/변경하지 못한다**. lead가 위조한 progress가
false-accept를 유발하면 보안 버그(false-reject/추가 abort는 허용).

- `CheckResult::Fail { reason, progress: Option<u16> }` — **PERMILLE**(0..=1000). `f64`가 아니라 permille이라
  `CheckResult: Eq`(→`RoundRecord`·`RpcEnvelope` derive) 보존. `serde(default)`+`skip_serializing_if=None`이라
  구버전 소비자 와이어 호환 → `RPC_SCHEMA_VERSION` 1.1.0→**1.2.0**(minor; doctor는 major만 비교, config 기본값 1.1.0 유지).
  `CheckResult::progress() -> Option<f64>`(Pass=1.0, Skipped=None, Fail=p/1000).
- `goal.rs`: 실패 체크 출력에서 비율 파싱 — libtest(`N passed; M failed`)·pytest(`M failed, N passed`) 요약을
  스트림별 **마지막 요약 줄** 우선(`next_back`). 양쪽 스트림 모두 인식되면 **가장 비관적(min) 비율** 채택 —
  lead가 한 스트림에 위조한 `1000 passed`가 다른 스트림의 실제 실패 요약을 가리지 못함(min은 stall 압박을 늘릴 뿐
  완화 불가 → reject-only 신호와 일관, codex S7 r2 nit 반영). prose 내성: 숫자가 `passed`/`failed` 바로 앞일 때만
  카운트(토큰 분할+windows). 미인식→None. infra 실패(spawn/timeout/await/output-cap)는 progress=None.
  nonzero-exit 분기에서 `stdout_text` 캡처(cap 체크가 Err를 이미 early-return하므로 Ok).
- **no-progress 가드 일반화**: 동일 patch hash(ma-1)뿐 아니라, **다른** patch를 내도 지금까지의 best pass-ratio를
  못 넘으면 stall로 카운트(green에 가까워지지 않고 churn하는 lead 포착). `round_is_stalled`는 progress 차원이 **순수 가산** —
  측정 가능한 비율이 없으면 기존 identical-hash 동작으로 환원, hash 누락은 절대 stall 아님. `best_progress`는 현재 라운드
  반영 **전**의 best와 비교(개선 판정 정확). 이 경로는 **abort(블록)만** 유발 → non-accept를 accept로 뒤집을 수 없음.
  게이트(`terminal`/`goal_satisfied`/`blocked`/`nits_unverified`)는 progress와 무관(no_progress_exceeded 경유로만 블록 가산).
- 검증: `cargo test --workspace` green (core 138: parse_progress libtest/pytest/all-failed/unrecognized/last-line/worst-across-streams +
  progress() accessor + goal-evaluator 통합 + round_is_stalled/check_result_progress 회귀; config 42, cli 49+16+4, adapter 92+1, types 25, tui 13),
  `clippy -D warnings` clean.

### S5 — OS 실행 샌드박스 (✅ DONE, 2026-06-17)

결정론적 체크(`/goal` + 빌트인 verifier)가 실행하는 **프로젝트 제어 코드**(Cargo build script,
`package.json` script, 테스트 코드)를 OS 수준에서 격리한다. 기존 자원 가드(env 화이트리스트·timeout·
output cap·ulimit) 위에 **파일시스템 쓰기 + 네트워크 confinement**를 더한다. Wave 1(검증-게이트 코어) 완료.

North star(S5, 정밀): 샌드박스는 성공을 **날조하지 않는다**. 게이트 판정을 더 보수적으로 만들거나(Refuse→Fail,
코드 미실행), 실행 명령의 **실제 exit status를 그대로 둔다**(Wrap 후에도 Pass는 자식의 실제 exit에만 의존).
백엔드를 세울 수 없는데 운영자가 `required`를 요청했으면 **fail-closed** — 코드는 무방비로 실행되지 않고 `Fail`을
반환한다(anti-pattern #1: 위험 실행은 요란한 opt-in, 침묵 기본 금지).
단, confined/unconfined **exit-code 동일성은 보장하지 않는다**(모든 샌드박스에 내재 — 거부된 syscall은 코드가
분기할 수 있는 에러를 반환; 예 `write-outside-cwd && exit 1 || exit 0`은 unconfined 1·confined 0). 이는
허용 가능한 잔여물: exit-code 권한은 **운영자 신뢰 체크(`check_cmd`/빌트인 verifier)**의 것이고 **lead는 이를
제어하지 못한다**(lead는 패치=체크가 실행하는 코드만 제어). 현실적 운영자 체크(예 `cargo test` 같은 집계 러너,
운영자 단언 하나라도 실패하면 non-zero)에 대해 lead는 거부 분기로 진짜 실패를 통과로 못 뒤집고, exit 로직을
완전히 작성할 수 있는 경우엔 unconfined로도 이미 통과 가능 — 샌드박스는 악성 **부작용**(루트 밖 쓰기·네트워크)만
안전 방향으로 제거. 따라서 `Required`는 confinement에 의도적으로 invert하도록 작성된 체크에 대해 `Off`의
**엄격한 보수적 상위집합이 아니다**(체크 작성자의 책임, 게이트의 문제 아님).

- `nerve-config`: `SandboxMode { Off|Auto|Required }`(기본 **Off** → 기존 동작 byte-identical 불변),
  `SandboxConfig { mode, allow_network }`(`allow_network` 기본 **false**: verifier는 hermetic해야 하고
  네트워크 차단이 프로세스 **직접 outbound 소켓**(흔한 exfil 경로)을 막음 — 단 daemon-mediated 채널[DNS via
  mDNSResponder]은 쓰기 측과 동일 잔여물로 남음, 하단 한계 참조). `deny_unknown_fields`로 오타 키가 confinement를 조용히 끄는 것 방지.
  `Orchestration.sandbox`로 배선.
- `nerve-core/sandbox.rs`(신규): `decide(config, cwd, check_cmd, extra_writable) -> SandboxDecision
  { Unconfined{warning} | Wrap{program,args} | Refuse{reason} }`. Off → Unconfined. 백엔드 부재 시
  Required→Refuse(fail-closed), Auto→경고+무방비(best-effort). writable root는 **canonicalize**
  (심볼릭링크 해소: macOS `/var/folders`→`/private/var/folders`, `/tmp`→`/private/tmp`)해 커널의 resolved
  뷰와 일치 — 불일치 시 *거부* 방향(fail-safe), over-broad allow 절대 아님.
- **macOS Seatbelt**(실커널 검증): SBPL 프로파일 = `(version 1)(allow default)(deny file-write*)
  (allow file-write* (subpath cwd)(subpath temp))[(deny network*)]`. last-match-wins라 deny 후 root 재허용이
  정상 동작. 경로는 SBPL string literal escaping(`"`/`\`)로 **profile injection** 차단. wrapper program은
  절대경로 `/usr/bin/sandbox-exec`(cleared PATH와 무관하게 해소), inner 명령은 sandbox-exec가 child PATH로
  resolve — unwrapped 동작과 동일.
- **Linux bwrap**(arg-gen 단위테스트, **macOS 호스트에선 런타임 미검증** — 정직히 명시): `--ro-bind / /`
  (호스트 ro) + writable root별 `--bind`(rw) + `--proc`/`--dev` + `--chdir cwd` + `--die-with-parent` +
  (네트워크 거부 시)`--unshare-net` + `-- 명령`. raw **seccomp/Landlock은 미구현** — bwrap의 네임스페이스
  +bind+unshare-net이 FS·네트워크 격리를 담당하며, syscall 필터링은 향후 강화 과제(과대광고 금지, anti-pattern #5).
  **wrapper 바이너리는 신뢰된 절대경로(`/usr/bin/bwrap` 등)에서만 해소 — `$PATH` 절대 미사용**: lead가 repo에
  `./bwrap`를 심고 `$PATH`에 `.`/repo-writable dir를 앞세우면 무방비 실행+exit 0로 fail-closed를 우회할 수 있어
  (codex S5 r1 BLOCK), root-owned 시스템 경로만 신뢰. 비신뢰 위치면 `Required`는 refuse(fail-closed).
- `goal.rs`: `spawn_and_wait` 맨 위에서 sandbox 해소 → wrap/refuse/unconfined. env_clear·whitelist·
  ulimit pre_exec·timeout·output cap **전부 보존**. `GoalEvaluator::with_options`(신규, sandbox 인자;
  `new`/`with_ulimit`은 Off로 위임). 프로덕션 단일 배선점 `build_goal_evaluator`만 `orchestration.sandbox` 전달.
- **argv 투명성**(sandbox 래핑이 어느 명령을 실행하는지 절대 바꾸지 않음): seatbelt는 profile 뒤 **wrapper 소유 `--`**
  삽입(`sandbox-exec -p P -- cmd …`)으로 `check_cmd[0]="--"`가 sandbox-exec 옵션으로 소비돼 뒤 토큰이 실행되는 걸 차단
  (bwrap는 이미 `--` 보유). 추가로 `GoalSpec::validate`·`BuiltinVerifierConfig::validate`가 **`-` 선행 program 거부**
  (program 이름은 `-`로 시작 안 함; 래퍼 옵션 오인 방지, defense-in-depth). codex S5 r2가 이 누락으로
  `check_cmd=["--","true"]`→`true` 실행→Pass(미래핑이면 spawn 실패=Fail)인 **수락-방향 위반(false Pass)** BLOCK 적발 → 양쪽 수정.
- 알려진 제약: macOS 프로파일은 permissive-baseline(reads/exec 허용)이라 강한 jail 아닌 *best-effort 쓰기+네트워크
  confinement*. confined 프로세스의 **직접(DIRECT) 루트 밖 쓰기/네트워크는 차단**(naive escape 격퇴)하지만
  **daemon-mediated 부작용은 못 막음** — `(allow default)`에서 Mach IPC로 샌드박스 밖 시스템 데몬에 요청 가능,
  예: `defaults write`→`cfprefsd`가 `~/Library/Preferences`에 plist를 대신 영속화(직접 쓰기는 거부되는데도; codex S5 r5).
  네트워크 동류: `(deny network*)`는 프로세스 자체 소켓을 막지만 DNS는 mDNSResponder를 IPC로 거치므로 crafted hostname으로 exfil 가능.
  쓰기/네트워크 가능 데몬 전수 차단은 whack-a-mole이고 deny-by-default IPC는 실제 빌드툴을 깨므로 **닫지 않고 문서화** —
  `Required`는 우발/naive escape 문턱을 높일 뿐 작정한 공격자에 대한 보안 경계가 아님(하드 격리는 container/VM).
  writable에 시스템 temp 전체 포함(빌드툴 intermediate; 저위험 ephemeral, 향후 per-check private temp로 좁힐 여지),
  reads는 broad. (Linux bwrap는 mount ns+RO 호스트로 이 지점에서 더 강하나, 바인드된 소켓으로 닿는 호스트 데몬은 동류 리스크.)
- 검증: 실커널 증명(`seatbelt_profile_denies_direct_out_of_root_write`: granted root 안 쓰기 허용 /
  sibling temp 밖 **직접 쓰기 거부 + 파일 미생성** — daemon-mediated는 범위 밖, 한계로 문서화),
  e2e 와이어링(Required로 cwd 쓰기 Pass, 래핑된 실패 체크 Fail 보존),
  순수 단위테스트(profile escaping·deny 선행 순서·network opt-in, bwrap ro/rw-bind·unshare-net·chdir·`--`,
  wrapper 신뢰-절대경로 불변, argv 투명성 실커널 증명[`-- true`가 `true` 미실행], leading-`-` 거부,
  fail-closed Refuse/Auto-warn). `cargo test --workspace` green
  (core 152: sandbox 14 신규; config 47: sandbox 4 + leading-dash 거부 신규), `clippy -D warnings` clean.
  codex 이중검증이 BLOCK 3건을 연쇄 적발: r1 Linux bwrap **`$PATH` hijack로 Required fail-open** → 신뢰-절대경로 해소;
  r2 macOS **`check_cmd[0]="--"`로 sandbox-exec가 false Pass 유발(수락-방향 위반)** → wrapper `--` + leading-`-` 거부;
  r4 **원래의 "Fail→Pass 절대 불가" 주장이 과대**임을 지적(confinement은 관측 가능 → 체크가 거부에 분기, 예
  `write-outside-cwd && exit 1 || exit 0`) → 주장을 달성 가능·테스트로 잠긴 보장(**성공 미날조**)으로 정밀화하고
  내재적 exit-code-parity 한계와 그 위협 경계(lead는 check_cmd 미제어; 집계 러너에선 무기화 불가)를 코드·커밋·로드맵에 명시.
  코드 변경 아닌 **정직한 주장 범위 축소**가 올바른 수정(내재 속성은 코드로 못 고침) — r3는 직전 clean HEAD에서 LGTM.
  r5 **macOS 쓰기-confinement 주장 과대** 지적(daemon-mediated 쓰기 `defaults`→`cfprefsd`가 `(deny file-write*)` 우회)
  → 주장을 **직접(DIRECT) 쓰기**로 축소, 테스트명을 `…denies_direct_out_of_root_write`로 변경, daemon-mediated 잔여물을
  permissive-baseline의 알려진 한계로 명시(전수 차단=whack-a-mole, deny-by-default IPC=빌드 파손 → 닫지 않고 문서화).
  r5는 fabricated-success/Required→Unconfined 경로는 **없음**을 확인(r4 범위 축소 수용).
  r5 이후 같은 결함류(daemon-mediated)를 선제 일관 적용: **네트워크 주장도 직접 소켓으로 축소**(DNS via mDNSResponder
  잔여물 명시) — whack-a-mole을 끊으려 모든 절대적 보안 주장을 best-effort/직접-한정으로 정직하게 통일.
  r6 **ACCEPT_WITH_NITS**(차단 없음): fail-open/fabricated-success 경로 없음 재확인, 범위 축소된 주장 sound 판정.
  유일 nit — 요약표가 `bwrap+seccomp+Landlock`로 top-line 과대(상세는 미구현 명시) → 표를 `Seatbelt / bwrap; raw
  seccomp/Landlock 향후`로 정정(과대광고 제거, 코드 무변경). 수락 게이트(연속 2회 무차단)는 정정 후 HEAD에서 재확인.

### S8 — 라운드 증분 체크포인트 (✅ DONE, 2026-06-17)

기존엔 `Synapse`가 라운드를 **메모리에만** 누적하고, `run_synaptic_loop`/`run_tournament_strategy`가
끝에서 단 한 번 `RunReport`를 만들고 CLI가 그 후 `save_report`를 1회 호출 → **루프 중간 크래시는
모든 라운드 진행을 유실**하고, 진행 중 실행의 on-disk 뷰가 없어 S9(논블로킹 데몬 + 라이브 스트림)의
기질이 부재했다. S8은 완료된 라운드마다 `.nerve/checkpoints/{id}.json`을 원자적으로 써서 그 둘을 해소.

North star(S8, 상속): 체크포인트 쓰기(또는 그 **실패**)는 어떤 패치가 수락/적용되는지, `blocked`/
`goal_satisfied`를 **절대 바꾸지 못한다**(S7과 동형의 *순수 가산 텔레메트리*). 체크포인트는 구조적으로
진행-중 산출물이라 크래시 복구된 체크포인트가 **완료/수락된 실행으로 오인될 수 없다**.

- `store.rs`: `RunStatus { Running | Finished }` + `RunCheckpoint { task, selection, status, rounds,
  updated_at }`(`deny_unknown_fields`). **수락 필드를 의도적으로 전부 생략** — `applied`/`blocked`/
  `goal_satisfied`/`final_patch` 부재이므로 체크포인트가 수락을 *주장하는 것이 타입 수준에서 불가능*
  (north star #2). 쓰기 경로는 항상 `Running`만 생산(`Finished`는 S9용 예약 — 미생산이라 doc에 명시).
  API: `save_checkpoint`(원자적 write_json), `load_checkpoint`, `list_checkpoints`(복구/관측 표면, 부재 dir→빈 Vec),
  `clear_checkpoint`(멱등 — 부재면 Ok). `ensure_dirs`에 `checkpoints_dir` 추가. **`save_report`가 finalize 시
  체크포인트를 clear** → "프로세스 종료 후 체크포인트 존재 == 미finalize(중단된) 실행"이라는 깔끔한 복구 신호.
- `lib.rs`: `Synapse`에 `checkpoint: Option<CheckpointSink { store, task, selection }>` 필드 + `with_checkpoint`
  생성자(기존 `new`는 `None`이라 모든 기존 호출/테스트 동작 byte-단위 보존). `record_round`는 **락 안에서**
  in-memory 갱신 + `rounds` 스냅샷을 뜨고 **가드 drop 후** 디스크 I/O(async RwLock을 blocking write에 걸치지 않음).
  쓰기 실패는 `tracing::warn`만 하고 루프 계속(중단 금지 — 가산 텔레메트리). 두 프로덕션 진입점(consensus·tournament)이
  `Synapse::with_checkpoint(task, NerveStore::new(&task.cwd), selection)`로 구성(항상-on; CLI store와 같은 `.nerve`를 가리켜
  finalize clear가 성립; warn-on-fail이라 비쓰기 cwd에서도 안전). `pub use store::{RunCheckpoint, RunStatus}`.
- 스코프 경계(S9 침범 금지): S8 = 체크포인트 **파일만**. 라이브 JSONL 스트림·논블로킹 데몬·pause/resume/cancel은 S9.
- 검증: `cargo test --workspace` green (core 159: store 4 [round-trip / save_report-clears / clear 멱등 / list] +
  Synapse 3 [라운드별 증분 기록 / no-sink 무기록 / **중단 실행이 복구 가능한 체크포인트를 남김** end-to-end] 신규;
  config 47, cli 49+16+4, adapter 92+1, types 25, tui 13), `clippy -D warnings` clean. 프로덕션 호출처 2곳 모두
  `task.cwd == store.cwd` 정렬 확인(오펀 없음), `.nerve/`는 gitignore.

### S9 — 논블로킹 라운드-이음새 데몬 v2 + 라이브 JSONL 스트림 (✅ DONE, 2026-06-17)

데몬 v1의 `handle_rpc_command`의 `prompt`는 `run_report`를 **완료까지** 돌린 뒤 끝난 `report.events`/
`report.rounds`를 사후(post-hoc) envelope로 **재생(replay)** 했다 — 그래서 모든 라운드의
`round.started`/`round.ended`가 실행이 **다 끝난 후에야** 한꺼번에 나오고(라운드별 타이밍 없음), read 루프는
실행 내내 `.await`에 **블로킹**돼 진행 중 status/관측이 불가능했다. S9는 둘 다 닫는다: 관측 채널로 라운드
이음새를 **라이브** 송출 + 실행을 spawn해 read 루프를 반응형으로 유지 + S8 체크포인트를 읽는 `status` 명령.

North star(S9, 상속): 라이브 스트림은 **읽기전용 텔레메트리** — envelope를 송출(또는 송출 실패)해도 어떤
패치가 결정론적 게이트에 수락되는지, `blocked`/`goal_satisfied`/apply를 **절대 바꾸지 못한다**(S7/S8과 동형의
*순수 가산 텔레메트리*). 논블로킹 spawn은 auto-apply나 수락 날조 경로를 도입하지 않는다 — 실행은 여전히
**변경되지 않은** `run_synaptic_loop` 게이트를 통과하고, 데몬은 OBSERVE+persist만 한다.

- **nerve-core**: `Synapse`에 두 번째 선택적 sink `round_observer: Option<mpsc::UnboundedSender<RoundRecord>>`
  추가(S8의 `CheckpointSink` 패턴 미러). `record_round`는 (락 안에서 in-memory 갱신 + rounds 스냅샷, 가드 drop 후
  S8 체크포인트 쓰기에 이어) `if let Some(tx) = &self.round_observer { let _ = tx.send(round); }` — **best-effort**:
  unbounded라 느린 소비자에 절대 블록/await 안 하고, send 에러(수신자 drop)는 무시(S8 warn-and-continue와 동형).
  생성자 `with_checkpoint_and_observer(task, store, selection, observer)` 추가, `with_checkpoint`는 `None`으로
  위임(S8 호출/테스트 byte-동일). 공개 진입점 `run_synaptic_loop_streaming(task, config, adapters, options,
  round_observer)` 추가 — 본문 중복을 피하려 기존 `run_synaptic_loop` 본문을 내부 `run_synaptic_loop_inner(...,
  observer: Option<...>)`로 추출, 공개 `run_synaptic_loop`은 `None`으로, streaming은 `Some(tx)`로 호출.
  tournament 분기도 observer를 `run_tournament_strategy` → `with_checkpoint_and_observer`로 배선.
  `pub use store::{RunCheckpoint, RunStatus}`는 S8에서 이미 노출.
- **nerve-cli 데몬 v2**: **run 레지스트리** `type RunRegistry = Arc<std::sync::Mutex<HashMap<String,
  JoinHandle<()>>>>`(run-id=task id 키) — spawn된 핸들을 추적해 drop되지 않게 하고 종료 시 await. `std::sync::Mutex`라
  짧은 동기 임계구역만(`.await`를 가로질러 잡지 않음). `prompt` 분기는 이제 `spawn_streaming_run`을 호출하고 **즉시
  반환**(논블로킹): cwd/config/task 구성 → `session.started` 라이브 송출 → `mpsc::unbounded_channel::<RoundRecord>()`
  생성 → STREAMER 태스크 spawn(`round_rx` 드레인 → 라이브 `round_seam_envelopes`) → RUN 태스크 spawn(adapters/options
  구성 → `run_synaptic_loop_streaming` → `save_report`[S8 체크포인트 clear] → `emit_terminal_envelopes`; Err면 error
  envelope; `let _ = streamer.await`) → 핸들을 레지스트리에 삽입(먼저 `reg.retain(|_,h| !h.is_finished())`로 완료된
  것 청소 → 무한 성장 방지). 새 `status` 분기는 `NerveStore::new(&cwd).list_checkpoints()`를 읽어 in-flight 실행마다
  `checkpoint_status_envelope`(`session.status` {session_id,prompt,status,rounds,updated_at}) + `{"type":"status_end",
  "in_flight":N}` 송출 — S8 재사용이라 데몬 재시작을 가로질러도 동작. **종료**: `Arc::try_unwrap(bus)` 전에 레지스트리를
  드레인하고 모든 run 핸들을 `await`(in-flight 실행 완료 + spawn된 bus 클론 drop → `try_unwrap` 성립; `--once`도 올바름:
  명령 1개 읽고 break → spawn된 실행 await → 종료). 평문(non-RPC) `run_daemon`은 **의도적으로 미변경**(여전히 블로킹).
- **nerve-types**: `rpc_kinds`에 **가산 const** `SESSION_STATUS = "session.status"` 1개만 추가 — 스키마 **bump 아님**
  (`RPC_SCHEMA_VERSION` 1.2.0 유지; payload/envelope SHAPE 변경이 아니라 const 추가는 "구버전 소비자는 모르는 kind 무시"
  규칙에 minor-호환). 라운드 이음새는 기존 `ROUND_STARTED`/`ROUND_ENDED`를 재사용 — `RoundRecord`(이미 Serialize)를
  관측 채널로 흘려 와이어 변경(AgentEvent 신규 variant) 회피.
- 스코프 경계(과욕 금지): S9 = 논블로킹 spawn + **라이브** 라운드 이음새 스트림 + `status`만. **per-run cancel /
  bulk cancel / Conductor 라이브 상태 UI = S15**(레지스트리는 핸들 추적·종료 await용으로 포함하되, 라운드-이음새
  cancel-token은 S15로 의도적 연기). **crossfire redirect = S10**. 같은 cwd 동시 `prompt` 2건의 apply 충돌 회피는
  S14 원장의 관심사 — 결정론적 게이트 + worktree-apply가 실행별 정합성을 이미 보장.
- 검증: `cargo test --workspace` green (core 161: streaming 2 [`streaming_loop_emits_each_round_live` —
  unbounded 채널 드레인 → 라운드 0,1 2개 / `record_round_observer_send_error_is_ignored` — 수신자 drop 후 패닉
  없이 state·체크포인트 여전히 갱신] 신규; cli 51: 2 [`round_seam_envelopes_carry_session_round_and_verdict` /
  `checkpoint_status_envelope_reports_progress_not_acceptance` — applied/blocked/goal_satisfied 키 부재 단언] 신규;
  config 47, adapter 92+1, types 25, tui 13), `clippy --workspace --all-targets -- -D warnings` exit 0.
  프로덕션 run_synaptic_loop 호출처 정합 + 종료 시 bus 클론 drop 순서(핸들 await 후 `try_unwrap`) 확인.

### S10 — crossfire advisory → redirect / 단락(short-circuit) (✅ DONE, 2026-06-17)

기존엔 lead 생성 중 `.nerve/scratch`를 감시해 reviewer의 **라이브 over-the-shoulder**
crossfire 피드백을 받지만(`collect_output_with_crossfire`) **기록만**(`record_crossfire_feedback`)
하고 lead를 조종하거나 라운드를 끊지 않는 순수 advisory였다. S10은 그 신호를 **행동 가능**하게
만든다: (1) **redirect** — crossfire 힌트가 다음 refine 프롬프트를 조종, (2) **단락(Halt)** —
결정적 라이브 `Block` crossfire가 루프를 단락하고 run을 block.

**핵심 설계 제약(코드 정독으로 확정)**: subprocess 어댑터의 `Command`는 `kill_on_drop` 미설정
(nerve-adapter/src/lib.rs:388) → 진행 중 lead 생성 future를 drop하면 모델 CLI 자식이 **고아(orphan)**
가 됨. 따라서 S10은 **라운드 이음새 기반**(생성 완료 후)으로만 동작하고 생성 중간을 절대 끊지 않음 —
directive (e) "조종은 라운드 이음새에서만" + anti-pattern #3(in-flight check_cmd/NvPatch 원자적 완료)를
정확히 준수. lead 생성은 repo 부작용 없는 LLM 호출(`.nerve/scratch`에만 씀)이고 실제 패치 APPLY(NvPatch)는
끝에서 `apply_final_patch` 한 번뿐이라 S10은 거기 손대지 않음.

North star(S10, 상속): redirect/단락은 STEERING + 텔레메트리 — 결정론적 수락 게이트를 **절대 약화하지
않는다**. 둘 다 **거부-방향 전용**: 더 많은 정밀검토/refine/abort로만 밀 수 있고 accept/apply/
goal_satisfied로는 **절대** 못 민다. "looks good" crossfire는 아무것도 가속하지 않고, 오직 결정적
`Block`만 단락하며 그것도 **blocked run으로** 단락한다.

- `nerve-config`: `CrossfireAction { Off(기본) | Redirect | Halt }`(snake_case, `Orchestration.
  crossfire_action`). 기본 **Off** → advisory-only 기존 동작 byte-identical. `redirects()`(Redirect|Halt)
  / `halts()`(Halt) 헬퍼. Halt ⊃ Redirect.
- `nerve-core`: `collect_output_with_crossfire`가 `(AgentOutput, Vec<ReviewerFeedback>)` 반환 — 그
  생성 동안 모은 crossfire를 추가로 돌려줌(report용 synapse 기록은 그대로 유지). 헬퍼
  `verdict_severity_rank`(Lgtm<AcceptWithNits<RequestChanges<Block, 거부-monotonic),
  `most_severe_crossfire`(배치 최고 severity verdict), `merge_crossfire_into_feedback`(거부-편향: verdict는
  거부쪽으로만 **올리고** 절대 안 내림, issue는 append, 그리고 힌트를 `raw_text`에도 렌더 — shipped lead
  어댑터의 refine 프롬프트는 `feedback.raw_text`**만** 읽으므로(nerve-adapter `refine`) 이게 없으면 redirect가
  실제 lead에 도달 못 함; 빈 배치는 base의 byte-identical clone). **Redirect**: refine 직전 `refine_feedback =
  merge(final_feedback, current_crossfire)`를 만들어 refine에만 전달 — **게이트가 읽는 `final_feedback`은
  불변**(게이트가 결과 패치를 독립 재판정하므로 crossfire가 수락을 날조 불가). **Halt**: 라운드 이음새에서
  terminal-accept 체크(라운드마다 **먼저** 실행 → 수락은 절대 못 뒤집음) 다음에 `crossfire_action.halts()
  && most_severe_crossfire(current_crossfire)==Block`이면 `crossfire_halted=true; break`. `current_crossfire`는
  매 라운드 refine 후 **교체**(append 아님)라 stale crossfire가 다음 라운드 halt 판정에 새지 않음. 게이트 배선:
  `blocked |= crossfire_halted`, `goal_satisfied &&= !crossfire_halted`(no_progress_exceeded와 동형). `RunReport.
  crossfire_halted: bool`(additive, `#[serde(default)]`). **tournament은 crossfire 없음**(단일 라운드·scratch
  watcher 없음)이라 `crossfire_halted: false` 필드만 추가.
- `nerve-cli`: `session.ended` envelope에 additive 필드 `crossfire_halted`(blocked의 정확한 사유, 스키마 bump
  아님). 사람용 요약에 단락 메시지. (별도 "live" 이벤트 kind는 **추가 안 함** — halt는 report 시점에만 알 수
  있어 live kind는 과대표현; additive payload 필드가 정직.)
- 검증: `cargo test --workspace` green (core 161→168: S10 7개 [off-record-only / redirect-merges-into-refine /
  halt-blocks-on-live-block / **halt-never-overrides-acceptance**(terminal-accept가 halt보다 선행하는 load-bearing
  가드) / non-block-never-halts / most_severe_crossfire / merge-rejection-biased]; config 47→49: defaults-off +
  3-variant round-trip; cli 51, adapter 92, types 25, tui 13), `clippy --workspace --all-targets -- -D warnings`
  exit 0. kill_on_drop 미설정 확인으로 seam-only 설계 확정, redirect가 게이트-bearing final_feedback 불변 확인,
  halt가 terminal-accept 뒤에 위치해 수락 미오버라이드 확인.
- 적대적 리뷰(codex r1, REQUEST_CHANGES)가 잡은 결함을 반영: 초판 merge는 verdict/issue만 올리고 `raw_text`를
  안 건드려, `raw_text`만 읽는 shipped lead refine 프롬프트(nerve-adapter `refine`)에 redirect가 **무효**였음
  (게이트-방향 버그는 아님). 수정: merge가 crossfire 힌트를 `raw_text`에도 렌더(라벨 + 표준 verdict 토큰).
  redirect 테스트는 `captured[0].raw_text.contains(MARKER)`, off 테스트는 `raw_text`에 마커 부재를 추가 검증.

### S11 — 승인-에스컬레이션 지속성 (sticky per run) (✅ DONE, 2026-06-17)

S9의 nonblocking 데몬 위에서, 운영자가 **진행 중인 특정 run**을 라운드 이음새에서 apply-consent로
**중간 승급**할 수 있게 한다. 승급은 그 run의 단일 apply 지점에서 존중되고 run의 전 생애(모든 라운드/
이음새)에 **sticky**하며, **per-run-id**로 격리된다(run B로 절대 새지 않음). 기본값은 불변(dry-run).

**설계를 지배한 보안 발견(load-bearing)**: 순진한 "apply 시점에 `.nerve/approvals/{run-id}.json`을
읽는다" 설계는 **안전하지 않다**. lead는 `task.cwd`에서 `.nerve/`에 쓰기 가능한 임의의 CLI 서브프로세스다.
S8 체크포인트(`.nerve/checkpoints/{task.id}.json`)에서 자기 run-id를 알아내 생성 중 `apply_consent:true`
파일을 **스스로 써서** 운영자 동의를 위조하고 dry-run→apply로 self-escalate 할 수 있다. 즉 "샌드박스
안 된 lead가 끄적일 수 있다"는 기존 사실을 **동의-위조 / 게이트-승급 벡터**로 바꾼다. North star 위반
(수락/동의를 절대 날조 금지; 승급은 시끄러운 **운영자** opt-in이어야 하고 lead가 닿으면 안 됨).

=> 게이트-권위 동의 신호는 lead가 위조 불가능해야 한다: **신뢰된 데몬 프로세스의 인-메모리 핸들**
(`ApplyConsent(Arc<AtomicBool>)`)이며, lead의 cwd가 닿는 디스크 파일이 **아니다**. 디스크 기록
(`.nerve/approvals/`)은 **감사 전용(AUDIT-ONLY)**이고 apply 게이트가 **절대 읽지 않는다** — lead가
그 파일을 위조해도 아무것도 안 바뀐다.

North star(S11, 반드시 유지): (1) 게이트 권위 = 인-메모리 `ApplyConsent`(데몬 소유, lead 도달 불가);
디스크 기록은 감사 전용·게이트 미참조. (2) grant는 **오직** `options.apply` 트리거에만 먹이고 `blocked`/
`goal_satisfied`는 불변 — `(apply||grant) && !blocked`이므로 **blocked run은 grant가 있어도 절대 apply
안 함**. (3) per-run 격리(`Task::id` 키). (4) 무-grant 기본은 오늘과 byte-identical(`apply == options.apply`).
(5) 승급은 시끄러운 명시적 **운영자** opt-in(`approve` RPC)이며 절대 기본값/lead-도달 아님. (6) grant는
**수락이 아니다** — `ApprovalGrant`는 verdict/blocked/goal_satisfied/patch가 없고 동의+신원만 가진,
`RunReport`/`RunCheckpoint`와 구조적으로 구별되는(양방향 `deny_unknown_fields`) 타입.

- `nerve-core/src/lib.rs`: `ApplyConsent(Arc<AtomicBool>)` newtype — `new()`/`grant()`(store true,SeqCst)/
  `is_granted()`(load,SeqCst), Clone=공유 핸들. `RunOptions.apply_grant: Option<ApplyConsent>`(Default=None →
  기존 동작 byte-identical) + `with_apply_grant()` 빌더 + private `apply_consented()` 리졸버
  (`self.apply || apply_grant.is_some_and(is_granted)`). **두 apply 사이트 모두**(consensus
  `run_synaptic_loop_inner`, tournament `run_tournament_strategy`) `options.apply && !blocked` →
  `options.apply_consented() && !blocked`로 교체 — `apply_consented()`는 거부-방향 전용(실제 운영자 grant로만
  ENABLE), `!blocked`(불변 결정론 게이트)와 AND.
- `nerve-core/src/store.rs`: `ApprovalGrant { run_id, apply_consent, granted_at }`(`deny_unknown_fields`,
  `ApprovalGrant::apply(run_id)` ctor) + `record_approval`(`.nerve/approvals/{run-id}.json` write_json) +
  `load_approval`(absent=Ok(None)) + `approvals_dir`/`approval_path` + `ensure_dirs`에 추가. 전부 **감사 전용**
  으로 문서화 — 게이트가 절대 참조 안 함.
- `nerve-cli/src/main.rs`: `struct TrackedRun { join, consent }`로 `RunRegistry` 값 타입 변경(drain/retain/
  insert 3개 use 사이트 수정). `spawn_streaming_run`에서 `ApplyConsent::new()` 생성 — run 태스크가 한 clone을
  apply 이음새에서 읽고, 레지스트리가 다른 clone을 보유(`approve`가 flip). 신규 `"approve"` RPC: `run_id`
  조회 → 인-메모리 `consent.grant()`(게이트-권위) + 감사용 `record_approval`(디스크) + `approve_ack` 라인.
  미존재 run-id면 `granted:false`로 아무것도 안 씀. `"status"`에 `load_approval`로 standing grant
  (`approval_grant` 라인) 노출 — 디스크 감사에 진짜 reader를 주고 재접속 클라이언트가 승인 상태를 봄.
  (인터랙티브 `/approve`는 SKIP — REPL은 blocking이라 중간에 flip할 라이브 인-메모리 핸들이 없음; async S9
  데몬이 올바른 표면. 누락 아님, 의도적.)
- 검증: `cargo test --workspace` green (core 168→175: S11 7개 [lib 5: handle-shared-starts-ungranted /
  consented-only-from-real-grant / grant-enables-apply-on-accepted / ungranted-handle-is-dry-run /
  **grant-never-applies-a-blocked-run**(grant가 `!blocked`에 안 먹이는 load-bearing 가드) ; store 2:
  round-trip+per-run-isolation+absent=None / **구조적-구별**(deny_unknown_fields 교차 역직렬화 양방향 실패)];
  config 49, cli 51→52, adapter 92, types 25, tui 13), `clippy --workspace --all-targets -- -D warnings` exit 0,
  `build --workspace` clean. lead 위조-방어 확인(게이트는 인-메모리 핸들만 읽음, 디스크는 감사 전용), grant가
  `blocked`/`goal_satisfied` 미오염 확인, 무-grant byte-identical 확인.
- 적대적 리뷰(codex r2, REQUEST_CHANGES)가 잡은 결함을 반영: `approve`가 `reg.get(run_id)`만 보고 grant해서,
  레지스트리에 남아있던 **이미 끝난(finished) run**도 `granted:true`를 돌려주고 `.nerve/approvals/`에 감사
  레코드를 썼음 — apply 게이트는 인-메모리 핸들만 읽고 끝난 run은 apply seam을 이미 지나 **승급-에스컬레이션은
  아님**(보안상 무해)이나, "in-flight only"(F) 계약 위반(끝난 run은 행동 불가인데 오해 소지 감사 레코드 생성).
  수정: `grant_in_flight(reg, run_id)` 헬퍼 추출 — 부재 **또는 `join.is_finished()`**면 `false`+무기록(끝난
  엔트리는 다음 spawn/shutdown까지 레지스트리에 남으므로 존재만으로 부족, `is_finished` 가드가 계약을 강제).
  테스트 `approve_grants_only_in_flight_runs`(finished→무grant·consent불변, unknown→무grant, in-flight→grant·
  consent flip) 추가(cli 51→52).

### S12 — auto-mode 분류기 게이트 (implement↔apply) (✅ DONE, 2026-06-17)

Codex "auto" 승인 모드 / Claude Code plan↔auto-accept 벤치마크. 운영자가 매 run마다 dry-run(implement)
vs apply를 손으로 고르는 대신, **결정론적 분류기**가 최종 패치의 위험도를 보고 모드를 정한다. 단,
north star를 지키도록 **거부-방향/단조(monotone)** 로만: 분류기는 would-be apply를 dry-run으로 **내릴**
수만 있고(위험 패치 veto), dry-run을 apply로 **절대 올리지 못하며**, `blocked`/`goal_satisfied`를 절대
건드리지 않는다. 기본값 **Off** → S12 이전과 byte-identical.

**핵심 설계 결정**: LLM 분류기(위험 의견으로 apply 자동 승급 = anti-pattern #1 + "LLM 의견은 게이트
아님" 위반)를 **기각**하고 **결정론적 패치-위험 분류기**를 채택. 최종 패치(`NvPatch`)만 읽어 결정 —
(1) non-noop 파일 수 > `max_files`, (2) 총 변경 라인(+/- from unified diff) > `max_lines`, (3) `risky_path_globs`
(globset; lockfile/CI/.github/.env/Dockerfile/Makefile 기본) 매칭, (4) `flag_destructive_ops`면 Delete/Rename.
하나라도 걸리면 High. LLM 호출 없음 → 비용·지연 0, 완전 테스트 가능. 오분류는 **과보호**(운영자가 수동
재적용)일 뿐 절대 날조 apply 불가.

**불변식(load-bearing, codex가 적대 검증)**: `apply_classifier_decision(want_apply, patch, cfg) ->
(allow_apply, classification)`에서 **`allow_apply <= want_apply` 항상 성립**(allow ⇒ want; 절대 업그레이드
없음). `want_apply`는 기존 `options.apply_consented() && !blocked` 그대로; 분류기는 그 위에 AND로만 얹힘.
- Off → `(want_apply, None)` byte-identical.
- Advisory → `(want_apply, Some(..))` 텔레메트리만(게이트 불변; High여도 veto 안 함).
- Enforce → `want_apply && High`일 때만 veto(`allow_apply=false`, `downgraded=true`); 그 외 결정 불변.
  `want_apply=false`(dry-run/blocked)면 High여도 그대로 false(내릴 게 없음, `downgraded=false`).

- `nerve-config`: `ApplyClassifierMode { Off(기본) | Advisory | Enforce }`(snake_case) + `classifies()`
  (Advisory|Enforce) / `enforces()`(Enforce) 헬퍼. `ApplyClassifierConfig { mode, max_files(25), max_lines(800),
  risky_path_globs(기본 9개), flag_destructive_ops(true) }`(`deny_unknown_fields`) + `validate()`(enabled인데
  max_files·max_lines 둘 다 0이면 reject). `Orchestration.apply_classifier`(`#[serde(default)]`) + Config.validate
  배선. 테스트 config 49→53(defaults-off / 3-variant round-trip / deny_unknown / validate-both-zero).
- `nerve-core`: `globset.workspace=true` 추가. `ApplyRisk { Low | High }`, `ApplyClassification { risk, reasons,
  files_touched, lines_changed, downgraded }`(`is_high()`), `RunReport.apply_classification: Option<..>`
  (`#[serde(default)]` → 구버전 리포트 byte-identical). 순수 fn `classify_apply`(None/noop=Low),
  `changed_line_count`(unified diff +/- 카운트), `build_risky_glob_set`(불량 glob은 자기만 skip, 전체 apply 경로
  안 죽임), `apply_classifier_decision`(불변식 보유). **두 apply 사이트**(consensus+tournament) `want_apply` 계산
  후 `apply_classifier_decision`로 `allow_apply` 도출 → `apply_final_patch(..., allow_apply, ...)`. 테스트
  core 175→185(None/noop=Low / small=Low / 각 위험신호 High[files·lines·glob(`**/Cargo.lock`이 bare `Cargo.lock`
  매칭 검증)·.github·delete] / off-byte-identical / advisory-no-veto / enforce-downgrades-only-would-be-apply /
  **never-exceeds-want_apply**(전 모드·임계·patch 조합 불변식) / e2e enforce-downgrade[**!blocked && !applied**=수락
  불변·apply만 veto] / e2e off-applies / e2e advisory-applies).
- `nerve-patch`: `FilePatch::is_noop()` pub 승격(분류기가 non-noop 파일만 세도록; 단일 출처).
- `nerve-cli`: `session.ended` envelope에 additive `apply_downgraded`(스키마 bump 아님) — 분류기가 apply를
  dry-run으로 내렸는지(수락은 됐으나 패치는 수동 검토 대기). 헬퍼 `apply_downgraded(report)`. 사람용 요약에
  `auto-mode: {risk} risk (N files, M lines): reasons` + 다운그레이드 시 "kept as dry-run … /diff 또는 /apply"
  안내. (레거시 `session_end` 라인은 v0.3.0 동결 — 안 건드림.) 테스트 cli 52→53(apply_downgraded None/advisory/
  enforce).
- 검증: `cargo build --workspace` clean, `cargo test --workspace` green (config 53, core 185, cli 53, adapter 92,
  types 25, tui 13), `cargo clippy --workspace --all-targets -- -D warnings` exit 0. 불변식 `allow<=want` 전수
  테스트로 확인, Off byte-identical 확인, e2e에서 enforce가 수락(`!blocked`)을 유지한 채 apply만 veto함 확인.

### S13 — 실행형 plan → loop 핸드오프 (Steps → Task/PatrolTask) (✅ DONE, 2026-06-17)

구조적 공백 #3 해소: `PlanReport`는 읽기전용 advisory markdown이고 `## Steps`는 자유 텍스트라 루프로 가는
실행 핸드오프가 없었다. S13은 plan을 **실행형**으로 만든다 — 스텝을 파싱 → `PatrolTask`로 변환 → Mayor 큐에
enqueue → Patrol 워커가 각각을 **진짜 synaptic 루프**로 돌린다(결정론적 검증 게이트 그대로, apply는 기본 OFF).

**North star(반드시 유지)**: (N1) plan은 절대 수락 게이트가 아님 — 디스패치된 스텝은 full 루프(S4/S6/S7/S10/
S11/S12 게이트 활성)로 돌고, plan(LLM markdown)은 task **프롬프트**만 시드한다. (N2) apply는 요란한 opt-in,
디스패치 기본은 dry-run(`RunOptions::new(false)`); `PatrolTask`에 apply 비트 없음 — apply는 per-run S11 consent
결정으로 남는다. (N3) 재귀 중첩 에이전트 없음 — Mayor/Patrol **큐**(process-per-loop, max_depth=1)로 핸드오프.
(N4) cwd는 호출자 고정 — `mayor_patrol::PatrolTask`에 cwd 필드가 없어 plan 텍스트가 실행 디렉토리를 못 바꾼다.
(N5) 변환은 **결정론적**(LLM 없음) — Nerve가 `PLAN_ONLY_SYSTEM_PROMPT`로 만든 canonical markdown의 `## Steps`를
순수 파서로 분해(S12의 결정론 선택과 일관). (N6) additive/inert — `save_plan` best-effort, plan 출력 불변, plan은
`check_cmd`를 못 정함(루프가 config에서 게이트 해석).

- `nerve-core/plan.rs`: `PlanStep { index, title, detail }`; 순수 `parse_plan_steps`(ordered `1.`/`2)` + unordered
  `-`/`*`/`+` 마커 인식, base-indent 추적으로 **중첩 sub-bullet은 새 스텝이 아니라 continuation**, 마커 없으면 빈
  vec); `parse_plan_steps_from_markdown`(extract_section "Steps" → parse); `plan_step_to_patrol_task`(task_id=
  `<plan>-step-NN`, 프롬프트=objective+step detail+affected files, **check_cmd/cwd 미인코딩**).
- `nerve-core/store.rs`: `NerveStore.save_plan/load_plan/list_plans` (`.nerve/plans/<id>.json`, atomic).
  **`validate_store_id`(load-bearing)**: 운영자 입력 `plan_id`를 `[A-Za-z0-9_-]` allowlist(1..=128)로 강제 →
  `..`/`/`/`\`/control 모두 fail-closed(mayor `validate_file_component` 계약과 일치). UUID task id는 통과.
- `nerve-core/mayor_patrol.rs`: 큐 컴포넌트 규칙(1..=128 of `[A-Za-z0-9_-]`)을 공개 술어 `is_valid_queue_id`로
  추출 — `validate_file_component`/`Mayor::enqueue`가 이를 경유하므로 **dispatch 사전검사와 enqueue 검사가
  절대 어긋나지 않는다**(단일 술어).
- `nerve-cli/main.rs`: `nv dispatch-plan <id> [--budget --max-steps --cwd]` + RPC `dispatch_plan` → 공유 코어
  `dispatch_plan_steps`(load → parse → `Mayor::enqueue` per step; **빈 스텝이면 fail-closed로 거부**; enqueue
  ONLY, 루프/apply 안 함) → `plan.dispatched` 이벤트. **(리뷰 정합성 수정)** dispatch는 enqueue 전에 모든
  `PatrolTask`를 먼저 만들고 파생 task id `<plan_id>-step-NN`을 `is_valid_queue_id`로 **원자적으로 사전검증** —
  128바이트 store 한계 근처의 `plan_id`(또는 스텝 수가 많아 접미사가 늘어난 경우)는 128바이트를 넘는 task id를
  파생하므로, **부분 dispatch 없이** 명확한 에러로 fail-closed(이전엔 일부 스텝 enqueue 후 Mayor의 모호한
  InvalidIdentifier로 중단됐을 수 있음 — accepted-but-undispatchable plan id). **(리뷰 nit)** `--max-steps 0`은
  이제 zero-step "success"를 조용히 반환하지 않고 명확한 에러로 fail-closed. 3개 plan 경로(subcommand/RPC/interactive)는 이제 plan을
  best-effort 영속화(`persist_plan_for_dispatch`). **스텁 Patrol 디스패치 클로저를 실제 클로저로 교체**:
  `patrol_dispatch`(Arc<Config>+Arc<adapters>, `run_synaptic_loop` **dry-run**, 루프 에러는 `failed/`로 기록·
  patrol 중단 안 함), `patrol_result_from_report`(blocked→Failed·else Success, cost=usage, patch_sha=미적용 패치 id).
- 검증: `cargo build --workspace` clean, `cargo test --workspace` green (core 185→200 [+15: 파서 ordered/paren/
  unordered/dense-index/continuation/prose-ignore/empty/from-markdown + converter 결정론·safe-id + store
  round-trip/missing/traversal-reject×2 + is_valid_queue_id 경계·enqueue-계약 교차검증], cli 53→61 [+8:
  dispatch-plan flag-parse + enqueue-each/max-steps/empty-fail-closed/max-steps-0-fail-closed/missing-plan +
  overlong-plan-id fail-closed(121바이트 회귀)·max-length-dispatchable(120바이트 경계) + patrol_result
  blocked→Failed], adapter 92, config 53, patch 20, types 25, tui 13),
  `cargo clippy --workspace --all-targets -- -D warnings` exit 0.
- DOUBLE 리뷰: codex r1(d62c419) = REQUEST_CHANGES(정합성: 121..=128바이트 plan id가 무효 task id 파생→dispatch
  실패) → 파생 task id 사전검증으로 해결. codex r1(재검증 HEAD) = ACCEPT_WITH_NITS(nit: `--max-steps 0` 무음
  no-op) → fail-closed로 수정. 동일 clean HEAD에서 연속 2회 no-blocking 리뷰로 LAND.

### S14 — Agent-Teams 조율 원장 (공유 task 원장 + mailbox + 파일락 claim) (✅ DONE, 2026-06-17)

Mayor/Patrol 큐는 권위 상태를 디렉토리(pending/claimed/done/failed)로만 들고, 크로스-patrol 가시성이 없었다 —
TUI/운영자가 "지금 어떤 task를 누가 들고 있나"를 보려면 O(dir-scan)이고, patrol끼리 신호를 주고받을 채널도
없었다. S14는 **조율/관측 전용** 레이어를 추가한다: 큐 상태의 비정규화 투영인 **공유 원장**(`ledger.json`) +
patrol 간 **파일 백드 mailbox** + claim의 파일락 일반화. 핵심은 이게 **수락/apply 신호가 절대 아니라는 것**.

**North star(반드시 유지)**: (N1) 원장/mailbox는 **관측·조율 전용** — 수락/apply 신호가 절대 될 수 없다. 권위
큐 디렉토리 + 결정론적 `blocked` 게이트가 수락/apply의 **유일한** 권위로 남는다(S11과 동형: 디스크 기록은
audit-only, 권위 상태는 딴 데). **원장과 디렉토리가 어긋나면 디렉토리가 이긴다** —
`ledger_is_non_authoritative_status_comes_from_dirs`가 원장을 통째로 지워도 `status`가 디렉토리에서 정확히
나옴을 증명. (N2) 모든 id는 `validate_file_component`(S13 `is_valid_queue_id` 술어) 경유 →
`..`/`/`/`\`/공백/빈 문자열 fail-closed; task/patrol/recipient 어느 것도 큐 루트를 못 벗어난다. (N3) 원자적 +
락 직렬화 쓰기 — RMW는 **전용 `ledger.lock`**(`mayor.lock`과 별개)로 직렬화. `Patrol::claim`의 `mayor.lock`
임계구역 **안에서** 원장 쓰기가 일어나므로, `mayor.lock`을 재사용하면 같은 프로세스가 같은 경로에 두 번째
배타 flock → **자기 교착**. 별도 락 파일로 두 임계구역을 독립시켜 claim을 절대 막지 않는다. (N4) additive/inert
— `coordination_enabled=false`면 모든 `record_*`/mailbox가 early-return → **큐 동작 바이트 동일**. 원장/mailbox는
큐 **루트** 바로 아래(pending/done 밖)에 살아 `count_json_files`/`count_claimed_recursive`/기존 테스트에 영향
없음. (N5) best-effort/비권위 — 원장/mailbox 실패는 호출부에서 `eprintln` 경고만, 권위
enqueue/claim/finish/recover를 **절대 중단 안 함**. (N6) 조율 경로에 **새 LLM 없음**. (N7) mailbox는 lead가
consent를 위조하는 **은닉 채널이 아님** — `MailKind`는 닫힌 enum(Note/Progress/Reclaimed)으로 **apply/consent
변종이 없어** mailbox 메시지가 apply 결정으로 파싱되거나 `blocked`를 완화하는 데 쓰일 수 없다(S11 north star:
apply consent는 lead가 위조 불가).

- `nerve-core/mayor_patrol.rs`: **`MayorLock` → 일반 `FileLock`** (`acquire(workspace_root, name)`); claim은
  `FileLock::acquire(.., "mayor.lock")`, 원장은 `"ledger.lock"`로 분리(N3). `QueueLayout`에 `root` 필드 +
  `ledger_path()`/`mailbox_dir()`(큐 루트 바로 아래, `ensure()` 불변이라 disabled시 바이트 동일). 타입:
  `LedgerState{Pending/Claimed/Done/Failed}`, `LedgerEntry{task_id,state,owner,created/claimed/finished_at,
  cost_microusd,verdict}`, `Ledger{version,entries: BTreeMap}`(결정론적 직렬화), 닫힌 `MailKind{Note,Progress,
  Reclaimed}`(N7), `MailMessage{id,from,to,kind,body,created_at}`. **`Coordinator`**(workspace+layout+enabled):
  `ledger()` 락프리 읽기(없으면 빈 원장), `mutate_ledger`(disabled면 no-op → `ledger.lock` → read/default →
  RMW → `write_json_atomic`), `record_enqueued/claimed/finished/recovered`(전부 `validate_file_component` 후
  mutate; finished는 Success/Skipped→Done·Failed→Failed로 **큐 이동과 같은 방향** 매핑 → 실패를 done으로 오보
  안 함), `send_mail`(disabled no-op, to/from/id 검증, `mailbox/<to>/<id>.json` atomic), `drain_mail`
  (**disabled no-op** — FS 접근 전에 early-return이라 비활성 시 기존 mailbox 파일을 읽지도 삭제하지도 않음[N4];
  enabled시 검증 후 읽고-삭제, malformed는 운영자 검사용으로 남김).
- 프로덕션 와이어링(전부 best-effort, eprintln 경고): `Mayor::enqueue`는 pending 원자 쓰기 **후**
  `record_enqueued`; `Patrol::claim`은 검증 블록 **후** `record_claimed`(보유 중인 `mayor.lock`과 별개 락이라
  교착 없음); `Patrol::run_task`는 result 쓰기 **후** `record_finished`; `Mayor::recover_orphans`는 rename **후**
  `record_recovered` + patrol mailbox에 `Reclaimed` 통지(mailbox에 실제 생산자/소비자를 줘 dead-code 방지).
  `Mayor`에 `ledger()`/`drain_mail()` 공개.
- `nerve-config/lib.rs`: `MayorPatrolConfig`에 `#[serde(default = "default_coordination_enabled")]
  coordination_enabled: bool`(serde 기본 **true**) + Default + 기본함수; `loads_default_mayor_patrol_config`에
  기본 true + explicit-false roundtrip 단언 추가. `deny_unknown_fields` 유지.
- `nerve-cli/main.rs`: `nv mayor`에 `--ledger`(원장 스냅샷 출력) + `--mailbox <PATROL_ID>`(수신함 drain·출력)
  플래그; `run_mayor_subcommand`에서 status **전에** 라우팅. `print_ledger`/`print_mailbox` 헬퍼. parse 테스트.
- `nerve-core/lib.rs`: `Coordinator, Ledger, LedgerEntry, LedgerState, MailKind, MailMessage` 재export.
- 검증: `cargo build --workspace` clean, `cargo test --workspace` green (core 200→208 [+8: pending→claimed,
  finished done&failed, recover→원장갱신+Reclaim mail, disabled-writes-nothing+큐불변, 비권위(원장 삭제해도
  status 정확), traversal-id 거부(원장+mailbox), 동시 enqueue 무손실(12 스레드), mailbox send/drain
  roundtrip+격리], cli 62→63 [+1: S14 조율 플래그 parse + 기존 flag-parse 확장], config 53[기존 테스트 확장:
  기본 true + explicit-false roundtrip], adapter 92, patch 20, types 25, tui 13, integration 16+4),
  `cargo clippy --workspace --all-targets -- -D warnings` exit 0.
- DOUBLE 리뷰: codex 독립 리뷰(자체 `CARGO_TARGET_DIR`로 build/test/clippy 재실행 + N1-N7를 REFUTE 시도 —
  특히 N1 비권위/디렉토리-우선, N3 `ledger.lock`≠`mayor.lock` 무교착, N5 disabled 바이트 동일, N7 mailbox
  비-consent). codex r1(43deef3) = REQUEST_CHANGES(**N4 위반**: `drain_mail`이 `coordination_enabled=false`를
  무시 — 비활성인데도 recipient 검증 후 `<queue>/mailbox/<recipient>`를 읽고 유효 파일을 **삭제**; 실제 `nv`로
  재현 `drained=1`+파일 삭제). N1/N2/N3/N5/N6/N7 권위 경계는 hold. → `drain_mail` 첫 줄에 `if !self.enabled
  { return Ok(Vec::new()); }` early-return 추가(`send_mail`/`mutate_ledger`와 동일 가드, FS 접근 전) + 비활성
  drain이 기존 파일을 안 읽고 안 지우는 회귀 테스트 + traversal 테스트를 `\`/탭/NUL/129바이트로 강화(codex nit).
  동일 clean HEAD에서 연속 2회 no-blocking 리뷰로 LAND.

### S15 — Conductor 라이브 상태 + 일괄 cancel (S9 의존) (✅ DONE, 2026-06-17)

S9 데몬은 라운드 이음새를 **라이브** 스트리밍하지만 in-flight run을 **중단**할 길이 없었다(라운드-이음새
cancel-token을 S9에서 S15로 의도적 연기). S15는 (1) **일괄 cancel** — 라운드-이음새 cancel-token + per-run 및
bulk cancel RPC — 과 (2) **Conductor 라이브 상태**(인-메모리 레지스트리 스냅샷 RPC)를 더한다. 마지막 스텝.

**North star(반드시 유지)**: (N1) cancel은 **절대 수락/apply를 위조하지 않음** — 거부-방향 전용 신호. `cancelled`를
기존 `blocked` OR-체인에 항 하나로 추가(consensus `lib.rs`, tournament `lib.rs`)하고 `goal_satisfied`에서 AND-NOT.
apply는 `apply_consented() && !blocked`(S11)이므로 cancelled⇒blocked⇒**apply 불가** + goal_satisfied=false.
S10 `crossfire_halted` 경로를 그대로 재사용 — 새 수락/apply 표면 없음. (N2) **라운드 이음새에서만** — cancel은
`synapse.record_round(...)` **직후**, 그리고 **terminal-accept 체크 이후**에 검사(=crossfire_halted와 동일 위치)라
**비수락 라운드에서만** 발화하고 이미 반환된 수락을 절대 못 뒤집는다. 생성 중간엔 안 됨(lead 모델 서브프로세스는
kill_on_drop 아님). (N3) **데몬 소유·lead 도달 불가**(S11 ApplyConsent 미러): `CancelToken`=`Arc<AtomicBool>`
newtype, 데몬이 레지스트리에 한 clone, run 태스크가 RunOptions로 다른 clone — lead 서브프로세스는 절대 도달/위조
불가. (N4) **무-토큰 byte-identical**: `RunOptions.cancel_token: Option<CancelToken>` Default=None ⇒ 이음새 검사
no-op, `cancelled` 항상 false ⇒ S15 이전과 비트 동일. (N5) **bulk cancel = 명시적 운영자 opt-in**: RPC 전용,
`all:true` 명시 필요, 끝난 run은 no-op(`grant_in_flight`의 `!is_finished` 가드 미러). (N6) Conductor 라이브
상태는 **관측 전용**(S14 원장 미러) — liveness/진행만 보고, applied/blocked/goal_satisfied 키 부재.

- `nerve-core/lib.rs`: `CancelToken(Arc<AtomicBool>)` newtype(`new`/`cancel`/`is_cancelled`, SeqCst, Clone=공유
  핸들) — ApplyConsent와 동형, 거부-안전 doc. `RunOptions.cancel_token` + `with_cancel_token` + private
  `is_cancelled()`. `RunReport.cancelled: bool`(`#[serde(default)]`, crossfire_halted 옆 — 가산·구버전 역직렬화
  유지). `cancelled_feedback`(Block verdict, no_progress_feedback 미러). **consensus 루프**: `cancelled` 플래그를
  이음새(record_round 직후, terminal break 뒤·Block break 앞)에서 검사→break, `blocked |= cancelled`,
  goal_satisfied `&& !cancelled`, RunReport에 `cancelled`. **tournament**(단일 라운드): record_round 후
  `let cancelled = options.is_cancelled();` → 동일하게 blocked/goal_satisfied/RunReport 반영(생성은 이미 끝났으니
  apply-게이팅 — cancelled tournament run은 blocked·미적용, 단 라운드 중간 중단은 불가, 정직히 명시).
- `nerve-core/store.rs`: 3개 테스트 픽스처 RunReport에 `cancelled: false` 추가(가산 필드).
- `nerve-cli/main.rs`: `TrackedRun{join,consent,cancel: CancelToken}`. `cancel_in_flight`(grant_in_flight 미러 —
  부재/끝난 run no-op) + `cancel_all_in_flight`(끝난 run 제외, 취소 수 반환). `spawn_streaming_run`이 토큰 생성→
  run options(`with_cancel_token`)+레지스트리에 clone. RPC **`cancel`**(`all:true`면 bulk `cancel_all_ack{count}`,
  아니면 `run_id`→`cancel_ack{run_id,cancelled}`; **디스크 쓰기 없음**) + RPC **`conductor`**(레지스트리 prune 후
  per-run `conductor_run{run_id,running,cancelled}` + `conductor_end{live}`, **수락 키 부재**). `session.ended`
  엔벨로프에 가산 `"cancelled"` 키(스키마 bump 아님).
- 검증: `cargo build --workspace` clean, `cargo test --workspace` green (core 208→212 [+4: cancel-at-seam
  blocks&never-applies / cancel-never-overrides-acceptance(N2) / uncancelled-token-inert(N4) / tournament-cancel-
  blocks&never-applies], cli 63→65 [+2: cancel-targets-only-in-flight / cancel-all-cancels-live-only],
  config 53, adapter 92, patch 20, types 25, tui 13, integration 16+4),
  `cargo clippy --workspace --all-targets -- -D warnings` exit 0.
- DOUBLE 리뷰: codex 독립 리뷰(자체 `CARGO_TARGET_DIR`로 build/test/clippy 재실행 + N1-N6를 REFUTE 시도 —
  특히 N1 cancelled⇒blocked⇒미적용/goal_satisfied=false, N2 수락 미오버라이드, N3 lead 도달 불가, N4 무-토큰
  바이트 동일, N6 conductor 관측-전용). 동일 clean HEAD에서 연속 2회 no-blocking 리뷰로 LAND.

---

## 🛡️ P1 — 향후 강화 로드맵 (Future Hardening)

> 출처: P0 완료(S1–S15) 후 5-에이전트 감사 워크플로우(`wf_3739824a-24f`, 원시 발견 51건) → 중복 제거 → **18항목 / 4 웨이브**.
> **불변식(모든 항목 공통):** 강화는 부수효과를 *가두기만* 하고 성공을 *날조하지 않는다*; off일 때 inert(가산); `Required`에서 fail-closed; 결정론적 `blocked` 검증기가 끝까지 **유일한 수락 권위**. 각 항목은 P0와 동일 게이트(cargo build/test green + clippy `-D warnings` + codex 연속 2회 no-blocking)로 LAND.
> **횡단 규칙:** 코드 실행을 가능케 하는 *모든* 새 config 필드/파일 채널은 `ConfigSource` provenance(`NERVE_TRUST_PROJECT_VERIFIER` 패턴, 위 §`roadmap:140-143`) 경유 — repo-local 파일이 operator를 더 강한 실행으로 opt-in 못 시키게(H5/H6/H8/H11/H12 수락기준, H18 린트로 강제).
> 상태: **전부 ⬜ 미착수** (P1은 별도 지시 시 착수).

### 권장 시퀀스

> Wave 1 (gate-adjacent leaks operators can misread as safe — do first, highest security value): H1 MCP allowlist inversion, H2 per-check private temp, H3 macOS daemon-mediated bypass mitigation, H4 Required runtime confinement self-test. These touch stated guards that can silently fail OPEN or leak side effects; H2 is also a structural prerequisite for Wave 2. Sequence within the wave: H2 first (it is a clean, cross-platform refactor of the writable-root seam that H4's canary and the Wave-2 Landlock rules both build on), then H4 (depends on the wrap path being testable), then H3 and H1 in parallel (independent surfaces).
> 
> Wave 2 (deepen sandbox confinement — defense-in-depth, all additive/BestEffort, gated behind new SandboxConfig fields): H5 Linux Landlock layer (depends on H2's private-temp writable root and on H4's self-test pattern for the FullyEnforced assertion), H6 optional seccomp denylist (off by default, never gates acceptance; shares the pre_exec seam with H5), H7 Linux real-kernel confinement CI proof (independent; unblocks trusting H5/H6 and the existing bwrap path), H8 macOS SBPL tightening + sandbox-exec deprecation canary (independent of Linux items). H5/H6/H8 all require the SandboxConfig type to grow (a small shared sub-task in H5).
> 
> Wave 3 (close honest-but-narrow integrity guarantees that could be misread as stronger): H9 Windows RPC token ACL (platform parity for a stated guard), H10 budget audit HMAC + softened wording, H11 MCP argument-policy gating, H12 LLM-proposed env validation, H13 Auto-mode machine-readable unconfined signal. These are independent of each other and of the sandbox waves; they harden integrity surfaces without touching the accept gate.
> 
> Wave 4 (steerability, observability, and honest scope-completion — lowest gate risk): H14 kill_on_drop / process reaper to fix S10/S15 mid-generation orphan (enables true live cancel), H15 cgroups v2 resource enforcement (Linux) + document macOS nproc as unenforced, H16 binary-file round-trip for patch rollback + honest scope doc, H17 progress-parser + coordination-reconcile observability hardening, H18 standing-guard CI lints that re-assert the anti-patterns and per-surface ConfigSource provenance for any NEW execution-enabling knob. H18 should land early enough to guard Waves 2-3 but is listed last because it depends on knowing the final shape of the new config surfaces; in practice start its skeleton alongside Wave 2.
> 
> Cross-cutting: any item that adds a SandboxConfig field, a config knob that can enable code execution, or a new file-backed channel MUST route through ConfigSource provenance (the NERVE_TRUST_PROJECT_VERIFIER pattern, roadmap:140-143) so a repo-local file can never opt the operator into stronger execution — this is enforced as an acceptance criterion on H5, H6, H8, H11, H12 and codified as a lint in H18.


### 상태표

| ID | Wave | effort | 상태 | 항목 |
|---|---|---|---|---|
| H1 | Wave 1 | M | ✅ | Invert MCP read-only enforcement from substring DENYLIST to deny-by-default ALLOWLIST (or annotation-driven) |
| H2 | Wave 1 | M | ✅ | Replace whole-system-temp writable grant with a per-check private TMPDIR (0700, RAII-cleaned) |
| H3 | Wave 1 | M | ✅ | Mitigate macOS daemon-mediated write+network bypass behind an opt-in strict SBPL profile |
| H4 | Wave 1 | M | ✅ | Add a runtime confinement self-test so Required fails closed when a wrap is silently ineffective |
| H5 | Wave 2 | L | ⬜ | Add an optional Linux Landlock filesystem layer (BestEffort) composed over the existing bwrap jail |
| H6 | Wave 2 | M | ⬜ | Add an opt-in seccomp-bpf denylist of dangerous syscalls (off by default, never gates acceptance) |
| H7 | Wave 2 | M | ⬜ | Add a Linux real-kernel confinement proof test in CI (mirror the macOS proof) |
| H8 | Wave 2 | M | ⬜ | Harden the macOS SBPL baseline and add a sandbox-exec deprecation canary |
| H9 | Wave 3 | M | ⬜ | Apply an owner-only ACL to the Windows RPC token file (platform parity for a stated guard) |
| H10 | Wave 3 | M | ⬜ | HMAC the budget audit hash chain (or external anchor) and soften 'tampering' wording |
| H11 | Wave 3 | M | ⬜ | Add optional per-tool MCP argument policy (path-root confinement, argv validation) on top of name gating |
| H12 | Wave 3 | S | ⬜ | Validate/allowlist LLM-proposed env vars in the /goal converter and surface them in the confirmation prompt |
| H13 | Wave 3 | S | ⬜ | Emit a machine-readable signal in the JSON report when Auto mode falls back to unconfined |
| H14 | Wave 4 | M | ⬜ | Set kill_on_drop (or add a child reaper) so S10/S15 cancel can stop in-flight generation and not orphan model CLIs |
| H15 | Wave 4 | L | ⬜ | Enforce per-check resource limits via cgroups v2 on Linux; document macOS RLIMIT_NPROC as unenforced |
| H16 | Wave 4 | M | ⬜ | Round-trip binary/non-UTF-8 files through the patch snapshot/rollback path; honestly scope what patching covers |
| H17 | Wave 4 | M | ⬜ | Broaden the progress parser and add ledger/checkpoint reconcile + dedicated writer for multi-instance observability |
| H18 | Wave 4 | M | ⬜ | Codify the anti-patterns and per-surface ConfigSource provenance as standing CI lints / invariant tests |

### 항목 상세

#### H1 — Invert MCP read-only enforcement from substring DENYLIST to deny-by-default ALLOWLIST (or annotation-driven)

**Wave 1 · effort M · ✅ DONE** · 의존: —

> **진행 로그 (✅ DONE):** `read_only` 하에서 admission을 **deny-by-default**로 역전. `nerve-types`에 `McpReadOnlyPosture { DenyByDefault(기본·fail-closed), LegacyDenylist }` enum + `McpToolInfo`에 `read_only_hint`/`destructive_hint: Option<bool>` 추가(enum을 nerve-types에 둔 이유: nerve-adapter는 nerve-types에만 의존 → adapter가 nerve-config 의존 없이 posture 보유). `nerve-adapter/mcp.rs`: `parse_tools_list`가 `annotations.readOnlyHint`/`destructiveHint`를 **방어적으로** 파싱(누락·비-boolean → None = 증거 아님, fail-closed). `call_tool` admission 순서 — (1) allowlist(hard boundary, 기존 유지) → (2) `read_only`면 posture 결정: **DenyByDefault**는 `read_only_admits = allowlisted || evidence`(증거=annotation 양성), 미충족 시 신규 `McpError::ToolNotReadOnly` → (3) **두 posture 공통** write-pattern **veto**(`tool_matches_write_pattern` → `ToolBlocked`). **monotone 보장(N5):** veto를 allowlist 멤버에도 적용 — H1 활성화가 legacy가 막던 툴을 새로 열어주지 않음(allowlist-scoped 서버는 DenyByDefault에서 기존과 동일 동작; annotation-less·no-allowlist 서버만 강화 = 로드맵 risk note와 일치). `read_only_admits`/`tool_has_read_only_evidence`는 순수 함수로 분리(단위 테스트). **PROVENANCE(P1 교차규칙):** `nerve-config`에 `McpConfig.read_only_posture`(`#[serde(default)]` → DenyByDefault) + 순수 resolver `resolve_mcp_read_only_posture(source, configured, consent)` — `builtin_verifier_exec_trusted` 미러: 약한 LegacyDenylist는 User/Default source이거나 Project+명시적 OOB consent(`NERVE_TRUST_PROJECT_VERIFIER`)에서만 인정, **Project source가 consent 없이 요청하면 DenyByDefault로 다운그레이드**(repo가 operator write posture를 약화 불가). nerve-cli `resolve_mcp_posture` 헬퍼가 4개 MCP 진입점(run_mcp_subcommand·handle_mcp_slash의 list/probe/call)에 적용 + 다운그레이드 시 loud `warn` 1회. **honest scope(anti-pattern #5):** annotation 신뢰=semi-trusted 서버(거짓말 가능) → allowlist만이 hard boundary; README 갱신(deny-by-default·veto·legacy posture·provenance 명시). accept gate(nerve-core) 무관 — MCP dispatch 한정. 테스트 +14: adapter +9(순수 진리표 2, annotation 파싱 1, call_tool reject 6: N1 out-of-pattern mutator·cache 부재·veto(annotation read-only+write name)·lying annotation은 allowlist 못 뚫음·allowlist reject·legacy veto), ignored mock는 `readOnlyHint:true`로 annotation-admit E2E 경로 검증, config +5(기본값·round-trip·resolver provenance 3종). 검증: nerve-adapter 90→99(+1 ignored), nerve-config 54→59, nerve-cli 65, 워크스페이스 test green, host `clippy -D warnings` 0, Linux cross all-targets 0, Windows lib 0. **게이트 통과:** 동일 clean HEAD `42c741f`에서 codex 독립 적대 리뷰 **r1=LGTM, r2=LGTM**(2연속 no-blocking) — N1–N6 전부 safe 재확인: N1 deny-by-default가 hole 폐쇄(out-of-pattern mutator `apply_patch`는 `ToolNotReadOnly`로 차단), N2 allowlist는 hard boundary(거짓 annotation도 set된 allowlist 못 뚫음), N3 write-pattern veto가 양 posture·allowlist 멤버까지 최후 적용(우회 불가), N4 Project+legacy+no-consent → DenyByDefault 다운그레이드 + loud warn(live smoke-test 확인), N5 LegacyDenylist는 pre-H1 동작 정확 재현·새 기본값은 strictly more restrictive(monotone), N6 accept gate(nerve-core) 무변경·blocked 툴은 Err 반환(success 위조 없음). 독립 build/test/clippy green(523 passed/1 ignored), 산출물 `/tmp/codex-security-scans/Nerve/42c741f_h1_review_20260617T142346Z`.

- **왜(보안 근거):** read_only=true is a STATED safety guard (README: 'enforce write-tool blacklist') that gates what tools the lead/reviewer LLMs may invoke. A denylist cannot enumerate every destructive tool name across arbitrary third-party MCP servers, so a server naming its mutating tool outside the pattern set (apply_patch, commit, delete_resource, rm, non-English names) runs despite read_only=true. This is the one place a stated guard silently fails OPEN — it lets gate-external side effects occur under a posture the operator believes is read-only, eroding the 'dangerous execution must be a loud explicit opt-in' anti-pattern.
- **현재 상태:** tool_matches_write_pattern (crates/nerve-adapter/src/mcp.rs:529-538) is a case-insensitive SUBSTRING match against default_write_tool_patterns(). The per-server allowlist (spec.allowed_tools) is a true allowlist checked first BUT is opt-in and usually empty; the default posture is denylist-by-pattern. call_tool (mcp.rs:332-360) applies it by name only.
- **목표 상태:** When read_only=true and no explicit allowlist is set, default to DENY-by-default: a tool is callable only if (a) it appears in an operator allowlist, or (b) the server's MCP tool annotation reports readOnlyHint=true / destructiveHint=false. The substring denylist remains only as an additional belt-and-suspenders veto, never as the primary admit decision.
- **접근법:** In crates/nerve-adapter/src/mcp.rs: add an annotation-fetch path that reads each tool's readOnlyHint/destructiveHint from the server's tool list (already retrieved during start()); add a default-deny resolver that admits a tool only on positive read-only evidence or explicit allowlist membership; keep tool_matches_write_pattern (mcp.rs:533) as a secondary deny veto. Surface the chosen posture in the confirmation/log path. Add a config knob (route through ConfigSource provenance so a repo file cannot widen it).
- **파일 seam:** crates/nerve-adapter/src/mcp.rs:61-65; crates/nerve-adapter/src/mcp.rs:332-360; crates/nerve-adapter/src/mcp.rs:529-538; crates/nerve-types/src/lib.rs (McpServerSpec.allowed_tools surface)
- **리스크:** Inverting the default can newly block tools an existing config relied on (annotation-less servers). Mitigate by gating the inversion behind a config flag defaulting to the safer posture, with a one-release deprecation warning on the legacy denylist path. Must not over-promise: a malicious server could lie in its annotations, so document that annotation trust assumes a semi-trusted server and the allowlist is the hard boundary.
- **수락 기준:** cargo build/test green + clippy -D warnings; new tests prove (1) an out-of-pattern mutating tool is BLOCKED under read_only by default, (2) an explicit allowlist still admits exactly its members, (3) a lying-annotation server is still constrained by the allowlist; two consecutive no-blocking codex reviews; invariant: the change only ever makes the MCP posture MORE restrictive by default and never routes through a repo-forgeable surface; the deterministic accept gate is untouched.

#### H2 — Replace whole-system-temp writable grant with a per-check private TMPDIR (0700, RAII-cleaned)

**Wave 1 · effort M · ✅ DONE** · 의존: —

> **진행 로그 (✅ DONE):** `nerve-core/goal.rs`에 `private_check_tmpdir(sandbox)` 헬퍼 추가 — 샌드박스 `is_enabled()`일 때만 `tempfile` 사설 디렉터리를 mint하고, umask(흔히 0755)를 무시하도록 `set_permissions(0o700)`로 **owner-only 강제**, `TempDir` RAII drop으로 자동 삭제. 이 디렉터리가 **유일한 extra writable root**이자 자식 `TMPDIR`(env 주입은 `goal.env` 루프 **뒤** = 권위 → 자식 temp 쓰기가 단일 grant 안에 떨어짐). 생성 실패는 `GoalError::PrivateTmpDir` → `CheckResult::Fail`(**fail-closed**, 더 넓은 grant로 강등 안 함). **Off는 byte-identical**: `decide()`가 Off에서 `extra_writable`를 읽기 전에 `Unconfined` 반환 + TMPDIR 미주입. 테스트 +2: 헬퍼(Off→None / enabled→0700 + RAII 삭제), macOS 실커널(`$TMPDIR` 쓰기 허용 + 사설 grant 밖 sibling 쓰기 **거부**·파일 미생성). 검증: nerve-core 212→214, 워크스페이스 test green, `clippy -D warnings` 0. codex 동일 clean HEAD(`a579b86`) 연속 2회 무차단(r1·r2 모두 `ACCEPT_WITH_NITS`, N1–N6 검증, 독립 build/test/clippy green)으로 LAND. 두 리뷰 공통 nit(Linux에서 `/tmp` 하드코딩·`TMPDIR` 무시 도구는 이제 샌드박스 하에서 쓰기 거부 — pre-H2 whole-temp grant가 허용하던 것)은 게이트 변경이 아니라 **fail-safe confinement**이며, `goal.rs` TMPDIR 주입부에 호환성 주석으로 명시(follow-up doc 커밋).

- **왜(보안 근거):** The deterministic gate executes project-controlled code that is currently granted the ENTIRE shared system temp dir as a writable root, so gate-run code can read/clobber sibling processes' temp artifacts and plant files later runs pick up — a cross-run/cross-process influence channel during the supposedly-isolated check. Shrinking the writable surface to one private dir tightens the isolation the gate's trustworthiness depends on, and is the structural prerequisite for the Wave-2 Landlock filesystem rules.
- **현재 상태:** extra_writable = vec![std::env::temp_dir()] is hardcoded in GoalEvaluator::spawn_and_wait (crates/nerve-core/src/goal.rs:118) and passed to sandbox::decide (goal.rs:120). canonical_writable_roots always prepends cwd (sandbox.rs:159-164) then grants the whole temp dir; bwrap --bind's it (sandbox.rs:283-288), Seatbelt subpath-allows it (sandbox.rs:204-212). The grant is not configurable and not surfaced in SandboxConfig.
- **목표 상태:** Each check mints a fresh per-invocation private directory (mkdtemp/0700) under the system temp root, passes ONLY that dir as the extra writable root, injects it as TMPDIR/TMP/TEMP into the (already env_clear'd) child env, and removes it via RAII after the child exits. The whole shared temp dir is no longer writable.
- **접근법:** In crates/nerve-core/src/goal.rs:118: create a tempfile::TempDir (Builder::prefix.tempdir_in(env::temp_dir())) BEFORE spawn; canonicalize it (reuse canonical_writable_roots logic at sandbox.rs:159-164 so /var/folders and /private/tmp resolve); pass the canonical path as the sole extra writable root; explicitly set TMPDIR/TMP/TEMP in the env block (goal.rs:146-158) — do NOT rely on inheritance after env_clear; keep the TempDir guard alive across spawn_and_wait and drop after child exit. Order is load-bearing (create -> canonicalize -> bind/allow -> set TMPDIR) to avoid the Claude Code #36759 hang where TMPDIR pointed at a non-existent dir.
- **파일 seam:** crates/nerve-core/src/goal.rs:118; crates/nerve-core/src/goal.rs:146-158; crates/nerve-core/src/sandbox.rs:159-164; crates/nerve-core/src/sandbox.rs:283-288
- **리스크:** Getting create-then-bind-then-set-TMPDIR order wrong causes build tools that read $TMPDIR to silently hang (the #36759 failure mode). A few tools hardcode /tmp and will now be DENIED — correct fail-safe direction, but could surface as a new check failure that previously leaked into /tmp. Both mitigated by an explicit test asserting child writes land in the private dir, plus a real-kernel confined test mirroring sandbox.rs:470-505.
- **수락 기준:** cargo build/test green + clippy -D warnings; new test spawns a child writing to $TMPDIR and asserts it lands in the private dir and the dir is gone after drop; confined real-kernel test (macOS, mirroring sandbox.rs:470-505) proves the shared temp is no longer writable while the private dir is; two consecutive no-blocking codex reviews; invariant: the writable surface only ever SHRINKS, Pass still keys solely on child exit status, no change to the accept gate.

#### H3 — Mitigate macOS daemon-mediated write+network bypass behind an opt-in strict SBPL profile

**Wave 1 · effort M · ✅ DONE** · 의존: —

> **진행 로그 (✅ DONE):** `nerve-config`의 `SandboxConfig`에 `#[serde(default)] strict: bool`(기본 false) 추가. `nerve-core/sandbox.rs`의 `seatbelt_profile(roots, allow_network, strict)`에 strict 분기 추가 — `(allow default)` 뒤에(SBPL last-match-wins) `(deny mach-lookup (global-name "com.apple.cfprefsd.agent")(...daemon))`로 **cfprefsd 매개 `defaults write` 영속 우회**를 차단, 그리고 `allow_network=false`일 때만 `(deny mach-lookup ...mDNSResponder...)`로 **DNS-over-IPC exfil 우회**를 차단(network 허용 시엔 정당한 DNS라 미적용). **strict=false면 mach-lookup deny 전무 → 프로파일 byte-identical**(additive·inert). `seatbelt_decide`가 `config.strict` 전달. 모듈 "Honest limitations" 독에 strict가 닫는 2개 채널을 명시하되 **완전성 주장 없음**(launchd/distnoted 등 잔존 — 하드 격리는 container/VM). **PROVENANCE(P1 교차규칙):** strict는 **monotone-restrictive**(오직 `(deny …)`만 추가) → repo-local(Project) config가 켜도 자기 체크를 더 조일 뿐 실행 활성화·완화 불가 → verifier-execution opt-in과 달리 operator-consent gate 불필요(필드 독+config 테스트에 근거 명시). 테스트 +6: 순수 단위 3(strict=off는 mach-lookup 무 / hermetic strict는 cfprefsd+mDNS deny & allow-default 선행 / network 허용 strict는 cfprefsd 유지·mDNS 제거), macOS 실커널 1(`strict_profile_denies_cfprefsd_mediated_write`: 비-strict에선 cfprefsd 영속=우회 실재 입증, strict에선 미영속 — 고유 도메인 2개+패닉 전 cleanup, 결정성 3회 확인; **positive control 추가**(r1 nit): strict 프로파일로 `/usr/bin/true`를 sandbox-exec에 태워 exit 0을 확인 → strict SBPL이 **거부되지 않고 적용됐음**을 직접 입증하므로 미영속이 깨진 프로파일의 vacuous pass가 아니라 cfprefsd mach-lookup deny에 기인함을 보장), config 2(strict 기본 false·round-trip+provenance 주석). 검증: nerve-config 52→54, nerve-core 218→222, 워크스페이스 test green, host `clippy -D warnings` 0, Linux cross 0, Windows lib 0. 모든 SandboxConfig 리터럴(테스트)은 `..Default::default()`로 미래 필드(H5/H6/H8) churn 방지. **게이트 통과:** 동일 clean HEAD `0dfacfb`에서 codex 독립 적대 리뷰 **r1=LGTM, r2=LGTM**(2연속 no-blocking) — N1–N6 전부 safe 재확인(strict=off byte-identical, strict는 `(deny …)`만 후행 추가라 `(allow default)`에 가려지지 않고 더 제한적, monotone이라 provenance consent 불요, accept gate는 child 실 exit에만 의존), positive control 직접 검증(valid strict=exit 0 / invalid SBPL=exit 65·미spawn), build/test/clippy green(config 54·core 222·cli 65).

- **왜(보안 근거):** This is the highest-severity real residual: a concrete WRITE-confinement and NETWORK-confinement bypass available to any project code the deterministic gate runs on macOS. Under (allow default), Mach IPC reaches daemons outside the sandbox — `defaults write` -> cfprefsd persists a plist under ~/Library/Preferences despite (deny file-write*), and DNS via mDNSResponder exfiltrates over crafted hostnames despite (deny network*). This directly contradicts the hermetic-verifier intent (allow_network=false to block exfil) and means a green result on macOS is not provably hermetic.
- **현재 상태:** The macOS profile (crates/nerve-core/src/sandbox.rs:202-217) is (allow default) minus out-of-root file-write* and minus network. The bypass is documented honestly (sandbox.rs:54-66) but explicitly 'documented, not closed' because blanket deny-by-default IPC breaks build tools.
- **목표 상태:** An OPT-IN strict profile that adds targeted (deny mach-lookup (global-name ...)) rules for the known highest-risk persistence/exfil daemons (cfprefsd, mDNSResponder) on top of the existing baseline, plus narrowed (deny file-read*) re-allow scoping where it does not break build tools. Default profile is unchanged (additive/inert when the strict flag is off). Documentation continues to state plainly that this is best-effort and NOT a security boundary against a determined adversary; hard isolation remains container/VM.
- **접근법:** In crates/nerve-core/src/sandbox.rs:202-217 (seatbelt_profile): emit additional targeted (deny mach-lookup) / (deny ipc-posix-shm) directives for the curated daemon set when a new strict mode is selected; thread a strict flag through SandboxConfig (extend lib.rs:267-280) routed via ConfigSource provenance. Keep (deny network*) as default (sandbox.rs:213). Add a real-kernel test proving cfprefsd-mediated write and mDNSResponder DNS are denied under strict (mirroring the existing direct-write proof at sandbox.rs:470-505).
- **파일 seam:** crates/nerve-core/src/sandbox.rs:202-217; crates/nerve-core/src/sandbox.rs:54-66 (limitations doc); crates/nerve-config/src/lib.rs:267-280 (SandboxConfig)
- **리스크:** Daemon enumeration is whack-a-mole — a curated deny list closes the two known channels but cannot claim completeness, so it MUST be documented as 'raises the bar on the known channels, not a hard guarantee.' Strict rules may break tools that legitimately use cfprefsd; keep it opt-in and default-off. Over-claiming completeness here would itself violate anti-pattern #5.
- **수락 기준:** cargo build/test green + clippy -D warnings; real-kernel macOS test proves cfprefsd write and mDNSResponder DNS denied under strict and the default profile is byte-identical when strict is off; doc explicitly enumerates what is STILL not covered; two consecutive no-blocking codex reviews; invariant: strict mode is additive/inert when disabled, only ever more conservative, routed through provenance, and never touches the accept gate.

#### H4 — Add a runtime confinement self-test so Required fails closed when a wrap is silently ineffective

**Wave 1 · effort M · ✅ DONE** · 의존: H2

> **진행 로그 (✅ DONE):** `nerve-core/goal.rs`의 `decide()` → `Wrap` arm에 **Required 전용**(또한 `cfg(any(macos,linux))` — 유일한 Wrap 생성 백엔드; 그 외 플랫폼은 Required가 이미 decision-time에 Refuse) confinement canary 추가. 실 체크를 신뢰하기 전에, **동일한 sandbox 프로파일**(`sandbox::decide` 재사용)로 카나리를 spawn해서 grant 밖 디렉터리(`profile_writable`에 **불포함**인 fresh probe dir)로의 쓰기가 실제로 **거부되는지** 증명한다. 판정 신호는 **파일 존재**(`canary_confined(escape_exists, live_exists) = !escape && live`) — exit code는 invertible하므로 쓰지 않음. **positive control**: 카나리가 grant 안에 LIVE 마커도 쓰므로 "escape 파일 없음"을 "카나리가 아예 안 돎"과 혼동하지 않는다(LIVE 없으면 fail-closed). escape 경로는 env로만 전달(스크립트 문자열 미보간 → 따옴표/공백/개행 주입 차단), `;`(not `&&`)로 in-grant 쓰기가 막혀도 escape 시도는 보장. canary가 confined가 아니면(`Ok(false)`) 또는 mint/spawn/timeout 실패면 실 체크를 **돌리지 않고** `CheckResult::Fail`(`CONFINEMENT_SELFTEST_FAILED`, fail-closed). probe dir은 `TempDir` RAII로 제거, LIVE 마커는 실 체크의 TMPDIR을 더럽히지 않도록 판정 후 제거. **success 위조 불가**: 카나리는 confined로 믿던 실행을 Fail로 바꿀 뿐, Fail을 Pass로 바꾸지 않음. **Auto/Off 불변**: Required에서만 동작(Auto는 정의상 best-effort, 추가 latency 없음). 테스트 +4: `canary_confined` 진리표(전 플랫폼), macOS 실커널 **effective**(escape 거부 → confined·파일 미생성), macOS 실커널 **fail-closed**(profile에 probe까지 grant해 ineffective wrap 시뮬레이션 → escape 성공 → NOT confined), macOS end-to-end(canary가 healthy Required run을 깨지 않음). 검증: nerve-core 214→218, 워크스페이스 test green, host `clippy -D warnings` 0, Linux cross `clippy -D warnings`(`x86_64-unknown-linux-gnu`) 0, Windows lib `clippy -D warnings`(`x86_64-pc-windows-msvc`) 0. **범위 명시(오버셀 금지):** 본 H4는 **파일시스템 쓰기** confinement self-test로 한정. 원안의 괄호 항목 "network connect deny self-test"는 **의도적으로 H7(Linux real-kernel CI)/H8(macOS SBPL)로 이연** — `sh -c` 자식에서 TCP connect를 시도할 **이식성 있는 보장 도구가 없음**(bash `/dev/tcp`는 POSIX `sh` 미보장, `nc`/`python3` 부재 가능) → fail-closed 카나리를 부재 가능 도구 위에 올리면 최소 시스템에서 Required를 깨거나(거부) fail-open(둘 다 더 나쁨); macOS에선 `(deny network*)`가 write-deny와 **동일 원자 SBPL 프로파일**이라 write 카나리가 프로파일 적용을 이미 입증, Linux `--unshare-net`의 독립 검증은 실커널 환경(H7)에서 수행. (Pre-existing·H4 무관: `mayor_patrol.rs`의 `backdate_to_seconds_ago`가 `cfg(unix)`인데 호출부는 비게이트 → Windows **test** 빌드만 깨짐. lib는 정상. 별도 추적.)

- **왜(보안 근거):** Required is the operator's HARD guarantee that code never runs unconfined. Fail-closed is enforced at decision time (no_backend_decide -> Refuse -> Fail) but once decide() returns Wrap the runtime is trusted BLIND: if the kernel rejects the SBPL profile or bwrap silently no-ops (e.g. unprivileged userns disabled), the failure degrades to an ordinary check Fail and Required can run effectively-unconfined while still being trusted. A gate that runs project code under weaker-than-believed isolation undermines the verifier's trustworthiness.
- **현재 상태:** Required-with-no-backend returns Refuse (sandbox.rs:137-141) -> Fail (goal.rs:131-136) — correct. macOS presence-checks sandbox-exec (sandbox.rs:177) and Linux requires a trusted absolute bwrap (sandbox.rs:247-251), but after Wrap the wrapped command is spawned via the generic path (goal.rs:139-182) with NO Required-specific assertion that confinement was actually established at runtime. No canary write/connect self-test exists.
- **목표 상태:** Under Required, before trusting a Wrap verdict, run a fast canary inside the same confinement that attempts a known-denied out-of-root write (and, when allow_network=false, a known-denied connect); if the canary does NOT observe denial, treat it as 'sandbox could not be established' and fail closed (distinguished error, code never runs) rather than as an ordinary Fail.
- **접근법:** In crates/nerve-core/src/goal.rs:139-182 (post-Wrap path): for SandboxMode::Required, first spawn the wrapper around a tiny canary command that attempts an out-of-root write/connect and reports the result; only proceed to the real check if denial is confirmed. Add a distinguished CheckResult/GoalError variant for 'confinement self-test failed' so it is not confused with a project failure. Reuse the wrap argv from sandbox::decide so the canary runs under the identical profile.
- **파일 seam:** crates/nerve-core/src/goal.rs:118-137; crates/nerve-core/src/goal.rs:139-182; crates/nerve-core/src/sandbox.rs:102-124 (decide)
- **리스크:** A per-check canary adds latency and a second spawn; keep it minimal and cache nothing (determinism). The canary must itself be argv-transparent (no '--' injection escape; reuse the locked logic at sandbox.rs:182-189). Must not weaken Auto: Auto keeps its loud-unconfined behavior; only Required gains the hard self-test.
- **수락 기준:** cargo build/test green + clippy -D warnings; tests prove (1) a deliberately-broken wrap under Required yields the distinguished 'confinement-failed' Fail and the project code never runs, (2) a working wrap passes the canary and runs normally, (3) Auto behavior is unchanged; two consecutive no-blocking codex reviews; invariant: this strictly strengthens fail-closed, never fabricates success, and the deterministic gate keys Pass only on the real check's exit after the canary clears.

#### H5 — Add an optional Linux Landlock filesystem layer (BestEffort) composed over the existing bwrap jail

**Wave 2 · effort L · ⬜ 미착수** · 의존: H2, H4

- **왜(보안 근거):** On Linux the entire write-confinement rests on bwrap's mount namespace; a host daemon reachable over a bound socket can still mediate writes (same class as the macOS bypass). An LSM-enforced Landlock ruleset is kernel-mediated and cannot be defeated by an in-namespace daemon, closing the documented bwrap residual as defense-in-depth — strengthening the isolation the gate depends on without extra privileges.
- **현재 상태:** bwrap_args (crates/nerve-core/src/sandbox.rs:265-295) emits --ro-bind / / + per-root --bind rw + --unshare-net. No Landlock, no seccomp, no extra namespace unshares. The only child-side hook is the pre_exec closure running apply_ulimit (goal.rs:165-180). SandboxConfig carries only mode + allow_network (lib.rs:267-280) — no field for a syscall/path policy.
- **목표 상태:** When enabled and the kernel supports it (>=5.13, landlock in the LSM stack), the confined child self-restricts via a Landlock ruleset granting file-write only under cwd + the H2 private temp dir and denying everything else, applied in BestEffort so older kernels gracefully fall back to bwrap-only. Under Required, require FullyEnforced (refuse if only PartiallyEnforced/NotEnforced) — do NOT silently under-enforce.
- **접근법:** Add the landlock crate (v0.4.x). Build the Ruleset in the PARENT, then call restrict_self() in an additional pre_exec closure ALONGSIDE apply_ulimit at goal.rs:165-180 (must stay async-signal-safe-ish: prctl/landlock syscalls qualify). Generate the writable set from the same roots H2 produces. Keep --unshare-net as the network kill-switch (do NOT replace it with Landlock TCP rules: Landlock net is TCP-only, no UDP/raw/unix). Surface RulesetStatus as an operator warning; under Required treat anything less than FullyEnforced as confinement-failed (ties into H4). Extend SandboxConfig (lib.rs:267-280) with a Landlock policy field routed via ConfigSource provenance.
- **파일 seam:** crates/nerve-core/src/goal.rs:165-180 (pre_exec seam); crates/nerve-core/src/sandbox.rs:265-295 (bwrap_args / writable roots); crates/nerve-config/src/lib.rs:267-280 (SandboxConfig)
- **리스크:** BestEffort can SILENTLY under-enforce on old kernels — under Required this MUST be surfaced and fail closed, or it weakens the Required promise. pre_exec runs between fork and exec and must avoid heap/lock surprises (build ruleset in parent, only restrict_self in child). New Linux-only codepath + dependency to maintain. Landlock never restricts already-open fds — document that.
- **수락 기준:** cargo build/test green + clippy -D warnings; arg/policy unit tests on all platforms + a Linux real-kernel test (gated to CI Linux, ties to H7) proving an out-of-root write is denied with FullyEnforced and that a non-supporting kernel falls back to bwrap-only with a surfaced warning; under Required, less-than-FullyEnforced yields confinement-failed; two consecutive no-blocking codex reviews; invariant: additive/inert when disabled, only ever more conservative, BestEffort downgrade is never silent under Required, accept gate untouched.

#### H6 — Add an opt-in seccomp-bpf denylist of dangerous syscalls (off by default, never gates acceptance)

**Wave 2 · effort M · ⬜ 미착수** · 의존: H5

- **왜(보안 근거):** Even with bwrap + Landlock owning filesystem/network confinement, the untrusted check process can issue exotic/dangerous syscalls (mount, ptrace, keyctl, bpf, userfaultfd, clone-with-new-namespaces). A targeted seccomp denylist shrinks the kernel attack surface the gate exposes when it runs project-controlled code, complementing — not replacing — the path/network confinement.
- **현재 상태:** No seccomp filter anywhere (grep for seccomp returns zero code hits). The pre_exec seam (goal.rs:165-180) currently installs only setrlimit.
- **목표 상태:** An OPT-IN, default-OFF denylist of clearly-dangerous syscalls installed via seccompiler (pure Rust, no C link, TSYNC) in the same pre_exec hook, after PR_SET_NO_NEW_PRIVS. It is secondary hardening: it NEVER becomes the basis of the Required fail-closed promise (that stays with bwrap+Landlock) and NEVER gates acceptance.
- **접근법:** Add the seccompiler crate. Install the BPF filter in the pre_exec closure at goal.rs:165-180, ordered after apply_ulimit and after PR_SET_NO_NEW_PRIVS (Landlock's restrict_self already sets NNP). Prefer a DENYLIST of dangerous syscalls (an allowlist is brittle against moving glibc/musl/cargo/node syscall usage and risks spurious KillProcess that looks like a check failure, breaking determinism). Surface as a SandboxConfig knob, default off, provenance-gated.
- **파일 seam:** crates/nerve-core/src/goal.rs:165-180 (pre_exec seam); crates/nerve-config/src/lib.rs:267-280 (SandboxConfig)
- **리스크:** A too-tight filter breaks toolchains and a denied syscall looks like a check failure, harming determinism — so ship a conservative denylist, default OFF, and document that it is attack-surface reduction, not a confinement boundary. Architecture-specific syscall tables; must handle the running arch. Forgetting exit/sigreturn deadlocks the child — guard with tests.
- **수락 기준:** cargo build/test green + clippy -D warnings; Linux test proves a denied dangerous syscall (e.g. mount) is blocked while a normal cargo/test workload still runs unaffected; the filter is provably inert when the knob is off; two consecutive no-blocking codex reviews; invariant: off by default, never the Required basis, never gates acceptance, accept gate untouched.

#### H7 — Add a Linux real-kernel confinement proof test in CI (mirror the macOS proof)

**Wave 2 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** Required mode promises fail-closed confinement on Linux, but today only the bwrap ARGV is unit-tested — the runtime path is never exercised on a real kernel. A bwrap version/flag drift, a missing namespace capability, or a distro with unprivileged userns disabled could silently degrade confinement while still emitting the expected args, so the gate could run project code effectively unconfined while reporting success-direction results.
- **현재 상태:** The real-kernel proof (seatbelt_profile_denies_direct_out_of_root_write, sandbox.rs:470-505) is macOS-only (cfg(target_os="macos")). bwrap is validated only at arg-generation level (sandbox.rs:39-45 docstring concedes this).
- **목표 상태:** A Linux-gated real-kernel test that actually runs a confined canary under bwrap (and, after H5/H6, Landlock/seccomp) and asserts an out-of-root write is denied and network is unshared — run on CI Linux runners — mirroring the macOS proof.
- **접근법:** Add a #[cfg(target_os="linux")] real-kernel test in crates/nerve-core/src/sandbox.rs alongside the macOS proof (sandbox.rs:470-505) that resolves trusted bwrap (sandbox.rs:312-318), wraps a canary attempting an out-of-root write, and asserts denial; gate it to run only where bwrap + userns are available, skipping (not failing) with a clear message where unavailable so it does not break dev hosts. Wire a CI Linux job to execute it.
- **파일 seam:** crates/nerve-core/src/sandbox.rs:470-505 (test region); crates/nerve-core/src/sandbox.rs:240-295 (bwrap path under test); CI workflow (Linux runner)
- **리스크:** CI runners may lack unprivileged userns; the test must skip-with-message rather than false-fail, while still failing loudly when bwrap IS available but does NOT confine. Care to keep it deterministic and fast.
- **수락 기준:** cargo build/test green + clippy -D warnings; the Linux real-kernel test passes on a CI Linux runner and proves out-of-root write denial + net unshare; it skips cleanly (not silently passes) where the kernel lacks support; two consecutive no-blocking codex reviews; invariant: purely additive test coverage, no production behavior change, accept gate untouched.

#### H8 — Harden the macOS SBPL baseline and add a sandbox-exec deprecation canary

**Wave 2 · effort M · ⬜ 미착수** · 의존: H3

- **왜(보안 근거):** macOS has NO seccomp/Landlock equivalent and sandbox-exec is deprecated (though still functional with no removal timeline). The current baseline allows broad reads and arbitrary exec; tightening within SBPL limits reduces what gate-run code can harvest before exfiltrating, and a canary protects against an OS update silently breaking the only macOS backend — which would let Required degrade.
- **현재 상태:** The profile (sandbox.rs:202-217) is (allow default) minus out-of-root writes/network; reads are unrestricted (sandbox.rs:67) and exec is allowed. sandbox-exec is presence-checked (sandbox.rs:177) but there is no canary that fails loudly if a macOS update breaks SBPL semantics.
- **목표 상태:** A tightened (opt-in, additive) read-scoping / exec-narrowing SBPL where it does not break build tools, plus a CI/startup canary that asserts sandbox-exec still enforces a known-denied operation; honest documentation that macOS confinement is permanently weaker than Linux (no syscall filter; Seatbelt silently DROPS denied ops) and that hard isolation is container/VM. App Sandbox and Endpoint Security explicitly OUT of scope (entitlement/signing/root, unsuitable for a headless runner).
- **접근법:** In crates/nerve-core/src/sandbox.rs:202-217: add optional (deny file-read* ...) re-allow scoping for sensitive paths (~/.ssh, ~/.aws, keychain) behind the strict flag introduced in H3; reuse the H3 strict-mode plumbing. Add a canary test/check (extending the proof at sandbox.rs:470-505) run in CI and optionally at startup that fails loudly if a known-denied write is NOT denied. Keep default profile unchanged.
- **파일 seam:** crates/nerve-core/src/sandbox.rs:202-217; crates/nerve-core/src/sandbox.rs:470-505 (canary test); crates/nerve-config/src/lib.rs:267-280 (strict flag shared with H3)
- **리스크:** Read-denial can break tools that legitimately read those paths; keep strict and default off. Seatbelt silently drops denied ops, so the canary is the only signal an update broke enforcement — must fail loudly. Do NOT over-claim: read scoping is a partial sensitive-path deny, not a read jail.
- **수락 기준:** cargo build/test green + clippy -D warnings; canary test proves a known-denied write is still denied (and fails loudly if not); strict read-scoping test proves sensitive-path read denial under strict with default profile byte-identical when off; doc enumerates what macOS still cannot cover; two consecutive no-blocking codex reviews; invariant: additive/inert when off, only more conservative, accept gate untouched.

#### H9 — Apply an owner-only ACL to the Windows RPC token file (platform parity for a stated guard)

**Wave 3 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** The RPC bearer token authenticates daemon clients; on Unix it is forced to 0o600, but the Windows path writes with NO permission hardening, relying on the parent directory's default ACL. The 'restrictive permissions' security property silently does NOT hold on one of three shipped platforms — a local-user-readable token lets another local account drive the daemon (and thus reach the apply path), an integrity gap an operator would not expect.
- **현재 상태:** atomic_write_token has a #[cfg(unix)] path forcing 0o600 and re-asserting set_permissions (rpc.rs:262-267), but the #[cfg(not(unix))] path (crates/nerve-core/src/rpc.rs:270-286) opens create/truncate/write with no ACL hardening. README scopes the claim to Unix (README.md:709).
- **목표 상태:** The Windows token file is created with an owner-only DACL mirroring the 0o600 Unix guarantee, so the 'restrictive permissions' property holds on all shipped platforms.
- **접근법:** In crates/nerve-core/src/rpc.rs:270-286 (the not(unix) atomic_write_token): after creating the staging file and before/at rename, set an explicit owner-only DACL via the windows / windows-acl crate (SetNamedSecurityInfo / explicit ACE for the current user, removing inherited broad ACEs). Keep the atomic staging+rename shape. Update README.md:709 to state the parity once landed.
- **파일 seam:** crates/nerve-core/src/rpc.rs:270-286; crates/nerve-core/src/rpc.rs:262-267 (unix reference impl); README.md:709
- **리스크:** Windows ACL APIs are fiddly and require a Windows CI lane to verify; an incorrect DACL could lock the daemon out of its own token. Mitigate with a Windows test asserting the file is readable by the owner and not by a second principal. Adds a Windows-only dependency.
- **수락 기준:** cargo build/test green + clippy -D warnings; Windows test asserts owner-only access on the token file; two consecutive no-blocking codex reviews; invariant: strengthens an existing integrity guarantee to platform parity, never weakens the Unix path, and does not touch the accept gate.

#### H10 — HMAC the budget audit hash chain (or external anchor) and soften 'tampering' wording

**Wave 3 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** The budget audit chain is presented as tamper detection backing the operator-policy ceiling ('an interactive budget raise cannot exceed operator policy'). But the chain is UNKEYED SHA-256 over a local file, so the same local user/agent who can write the file can edit any entry and recompute every subsequent prev_hash to produce a fully 'Intact' forged chain. It detects accidental/naive single-entry edits, not a determined local actor — an over-sold integrity guarantee that could mislead an operator into trusting a forged ceiling history.
- **현재 상태:** verify_chain (crates/nerve-core/src/budget_audit.rs:223-248) and hash_entry (budget_audit.rs:250-257) use plain SHA-256 of canonical JSON, no secret key, no external anchor. nv doctor reports 'may have been tampered with' on a break (README.md:405,710).
- **목표 상태:** The chain is keyed with an operator secret kept OUTSIDE .nerve/ (HMAC-SHA-256) or its head hash is anchored to an append-only/external sink, so a local file-writer cannot silently re-derive a valid chain; and the doctor wording is softened to describe exactly what is and is NOT detected.
- **접근법:** In crates/nerve-core/src/budget_audit.rs:250-257 (hash_entry) and :223-248 (verify_chain): replace SHA-256 with HMAC keyed by an operator secret resolved from outside .nerve/ (env var or a user-config-dir key file, NEVER repo-local — route any config through ConfigSource provenance). Provide a clear migration for existing v0.2.0/unkeyed prefixes (already tolerated as a contiguous prefix at budget_audit.rs:232-235). Soften the doctor message to 'detects accidental edits and naive tampering; a key-holder can still rewrite the chain — keep the key off the host you defend against.'
- **파일 seam:** crates/nerve-core/src/budget_audit.rs:218-257; crates/nerve-core/src/budget_audit.rs:176 (chain link write); README.md:405,710
- **리스크:** Key management UX (where the secret lives, rotation) and a clean migration from unkeyed chains. If the key lives on the same host as the attacker, HMAC adds little — so the honest framing in the doctor wording is itself part of the deliverable. Must not break existing audit logs.
- **수락 기준:** cargo build/test green + clippy -D warnings; tests prove (1) an entry edited without the key fails verification, (2) existing unkeyed prefixes still verify under migration, (3) doctor wording reflects the real threat model; two consecutive no-blocking codex reviews; invariant: the audit chain is observability/integrity-evidence only — it remains NON-authoritative for acceptance and never gates the deterministic verifier.

#### H11 — Add optional per-tool MCP argument policy (path-root confinement, argv validation) on top of name gating

**Wave 3 · effort M · ⬜ 미착수** · 의존: H1

- **왜(보안 근거):** MCP calls are gated ONLY by tool name; arguments are never inspected. A tool that passes the name guard but accepts dangerous arguments (a 'query' tool taking an arbitrary command, a 'read_file' following path traversal / absolute paths outside the project) is unconstrained, so an LLM-driven call could read/exfiltrate outside the intended scope without tripping any guard — the trust boundary wrongly assumes a tool's name fully describes its capability.
- **현재 상태:** call_tool (crates/nerve-adapter/src/mcp.rs:332-360) enforces the allowlist and the read-only name check, then sends the request with NO argument inspection.
- **목표 상태:** An OPTIONAL per-tool argument policy applied before dispatch: path-typed arguments confined to the project root (reject traversal / absolute escapes), and operator-defined argv validation for known-risky tools. Additive: tools without a policy behave as today.
- **접근법:** In crates/nerve-adapter/src/mcp.rs:332-360: after name gating, run an optional policy resolver keyed by tool name that validates declared argument shapes (path-root confinement for path args, simple allow/deny predicates for command-like args) and refuses the call on violation. Source policies from operator config routed via ConfigSource provenance (a repo file must not loosen them).
- **파일 seam:** crates/nerve-adapter/src/mcp.rs:332-360; crates/nerve-types/src/lib.rs (per-tool policy surface)
- **리스크:** Over-broad path confinement could break legitimate tools that need absolute paths; keep policies opt-in and explicit. Cannot fully model arbitrary server semantics — document it as defense-in-depth on top of name+allowlist gating, not a complete capability model.
- **수락 기준:** cargo build/test green + clippy -D warnings; tests prove a path-traversal / out-of-root path argument is rejected when a policy is set and that policy-less tools are unchanged; two consecutive no-blocking codex reviews; invariant: only ever restricts calls further, provenance-gated, never touches the accept gate.

#### H12 — Validate/allowlist LLM-proposed env vars in the /goal converter and surface them in the confirmation prompt

**Wave 3 · effort S · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** The whole point of GOAL_INTENT validation is that an LLM cannot smuggle an unsafe GoalSpec past operator confirmation. check_cmd is PATH-safety validated and cwd is locked, but the env map the LLM proposes flows into the spec unmodified — env is the one field forwarded without converter-level scrutiny, relying solely on the confirmation prompt as backstop. Validating it hardens the 'an LLM cannot propose an unsafe GoalSpec' guarantee (defense-in-depth alongside the executor's env_clear+whitelist).
- **현재 상태:** In crates/nerve-core/src/goal_intent.rs:138-146 the env BTreeMap from the LLM's RawIntent is passed through to the GoalSpec unmodified; GoalSpec::validate checks argv/cwd/timeout but not env keys/values. The executor's env_clear+whitelist (goal.rs:146-158) mitigates at execution but the converter applies no scrutiny.
- **목표 상태:** The converter validates/allowlists LLM-proposed env keys and values (reject suspicious keys, surface all proposed env prominently in the confirmation prompt) so a malicious env injection is visible BEFORE acceptance and cannot quietly ride along.
- **접근법:** In crates/nerve-core/src/goal_intent.rs:138-146: add an env validator (key allowlist / format checks) mirroring the argv PATH-safety check at goal_intent.rs:39; reject or flag entries that fail; ensure the user-confirmation prompt explicitly lists every proposed env var. Keep cwd-lock and argv validation unchanged.
- **파일 seam:** crates/nerve-core/src/goal_intent.rs:138-146; crates/nerve-core/src/goal_intent.rs:39 (argv validation reference)
- **리스크:** Low — additive validation. Risk is over-rejecting legitimate env (e.g. RUSTFLAGS); make the allowlist sensible and the confirmation prompt the final human gate. No change to execution-time env_clear behavior.
- **수락 기준:** cargo build/test green + clippy -D warnings; tests prove a suspicious env key is rejected/flagged and that all proposed env appears in the confirmation surface; two consecutive no-blocking codex reviews; invariant: confirmation remains the human gate; this only makes the proposed spec MORE visible/restricted and never auto-accepts, leaving the deterministic verifier as sole acceptance authority.

#### H13 — Emit a machine-readable signal in the JSON report when Auto mode falls back to unconfined

**Wave 3 · effort S · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** Auto is the 'confine-if-possible' middle mode, but when no backend is available it runs UNCONFINED with only a stderr warning (Required correctly refuses). In non-interactive/CI use the warning is swallowed, so an operator who chose Auto believing it confines may run repo code unconfined without noticing — a place the confinement guarantee is incidentally lost silently. Making the downgrade machine-readable lets pipelines detect and react.
- **현재 상태:** no_backend_decide returns Auto -> Unconfined { warning } (sandbox.rs:126-140), surfaced only via tracing::warn (goal.rs:122-124). The fallback is invisible in the structured JSON report.
- **목표 상태:** When Auto degrades to unconfined, the structured run/check report carries an explicit machine-readable 'ran_unconfined' field (not just a log line), and docs distinguish Auto ('confine-if-possible, else run openly') from Required ('fail closed').
- **접근법:** Thread the Unconfined { warning } signal (sandbox.rs:88, surfaced at goal.rs:121-129) into the CheckResult / run report struct so it appears in the JSON output; document the Auto-vs-Required semantics next to SandboxConfig (lib.rs:267-280) and in the README. Purely additive telemetry — never changes the verdict.
- **파일 seam:** crates/nerve-core/src/sandbox.rs:126-140; crates/nerve-core/src/goal.rs:121-129; crates/nerve-config/src/lib.rs:267-280 (doc)
- **리스크:** Low — additive telemetry. Must remain non-authoritative: the unconfined signal records reality, it does not relax or strengthen any verdict.
- **수락 기준:** cargo build/test green + clippy -D warnings; test asserts the JSON report exposes ran_unconfined=true when Auto degrades and false/absent otherwise; two consecutive no-blocking codex reviews; invariant: pure telemetry, never blocks or fabricates acceptance, deterministic gate untouched.

#### H14 — Set kill_on_drop (or add a child reaper) so S10/S15 cancel can stop in-flight generation and not orphan model CLIs

**Wave 4 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** S10 steering and S15 cancel act only at the ROUND SEAM because the subprocess adapter's Command never sets kill_on_drop — dropping an in-flight generation future orphans the model CLI child. A decisive live Block cannot stop in-flight work and a cancelled run leaks orphaned LLM CLI processes. This is gate-SAFE today (steering is reject-direction only, applied after terminal-accept) but limits the steerability that structural gap #2 set out to deliver and is a resource/cleanup hazard.
- **현재 상태:** The subprocess adapter spawns via Command::new(...).spawn() at crates/nerve-adapter/src/lib.rs:388-397 with NO kill_on_drop anywhere in the file. Consequently S10 redirect/halt (roadmap:431-436) and S15 tournament cancel (roadmap:717-719) take effect only at the next seam.
- **목표 상태:** Dropping/cancelling an in-flight generation reliably terminates the model CLI child (no orphan), enabling cancel to interrupt mid-generation while preserving the gate-safe semantics (cancelled => blocked => not applied).
- **접근법:** In crates/nerve-adapter/src/lib.rs:388-397, set .kill_on_drop(true) on the tokio Command (or add an explicit reaper that tracks the child handle and kills it on cancel). Verify interaction with the spawn-with-retry wrapper (lib.rs:388) and the existing timeout/start_kill paths in goal.rs:196-197. Ensure cancel still maps to blocked+not-applied (no acceptance change).
- **파일 seam:** crates/nerve-adapter/src/lib.rs:388-397; crates/nerve-core/src/lib.rs (round seam / cancel handling)
- **리스크:** kill_on_drop can race with normal completion and with the retry wrapper; must ensure a normally-finished child is not double-killed and that partial output is handled. Killing mid-generation must remain reject-direction only — a killed generation can NEVER be treated as an accept (it maps to blocked/not-applied), preserving anti-pattern #3 (no interrupt=kill on verifier/rollback paths — only generation is killable, never the verifier or rollback).
- **수락 기준:** cargo build/test green + clippy -D warnings; test proves dropping an in-flight generation future leaves no orphaned child and that a cancelled run yields blocked+not-applied; two consecutive no-blocking codex reviews; invariant: only model GENERATION is interruptible (never the verifier or rollback), cancel never fabricates acceptance, deterministic gate remains sole authority.

#### H15 — Enforce per-check resource limits via cgroups v2 on Linux; document macOS RLIMIT_NPROC as unenforced

**Wave 4 · effort L · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** Resource exhaustion (fork bomb, memory blowup) by an adversarial check/patch the gate runs is only partially mitigated: setrlimit is per-process (a fork bomb where each child stays under per-process limits evades it) and macOS rejects RLIMIT_NPROC for unprivileged callers, degrading to a silent no-op that returns Ok — giving operators false confidence nproc is enforced. True aggregate limits need cgroups; the macOS gap needs honest documentation.
- **현재 상태:** ulimit.rs uses setrlimit(2) per check (crates/nerve-core/src/ulimit.rs:73-98); the module header notes 'v1.0 will replace this with cgroups (Linux) per Tier 2g' (ulimit.rs:1-5); macOS nproc silently no-ops returning Ok (ulimit.rs:122-126). Config at nerve-config/src/lib.rs:93,292.
- **목표 상태:** On Linux, aggregate CPU/memory/PID limits enforced via a per-check cgroups v2 group (resists fork bombs); on macOS, nproc is documented as unenforced (no silent false-Ok claim) while RLIMIT_AS/FSIZE/CPU continue to apply.
- **접근법:** Add a Linux cgroups v2 path: create a per-check cgroup, set pids.max / memory.max / cpu.max, move the child into it, tear it down after exit; apply alongside the existing setrlimit pre_exec (goal.rs:165-180) rather than replacing the working RLIMIT_AS/FSIZE/CPU. In crates/nerve-core/src/ulimit.rs:122-126, change macOS nproc from silent-Ok to an explicit 'unenforced on this platform' status surfaced to the operator. Document both clearly.
- **파일 seam:** crates/nerve-core/src/ulimit.rs:1-5,73-98,122-126; crates/nerve-core/src/goal.rs:165-180 (apply seam); crates/nerve-config/src/lib.rs:93,292
- **리스크:** cgroups v2 may require delegated cgroup access (systemd user slices) which CI/containers may not grant — must degrade gracefully to setrlimit with a surfaced note, never silently. Tear-down must be robust to avoid leaking cgroups. macOS status change must not be read as a failure by existing callers.
- **수락 기준:** cargo build/test green + clippy -D warnings; Linux test proves a fork bomb is capped by pids.max (where cgroup delegation is available) and degrades with a surfaced note otherwise; macOS nproc reports 'unenforced' explicitly rather than Ok; two consecutive no-blocking codex reviews; invariant: resource limits only constrain the check, never affect the verdict toward acceptance, deterministic gate untouched.

#### H16 — Round-trip binary/non-UTF-8 files through the patch snapshot/rollback path; honestly scope what patching covers

**Wave 4 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** The README's atomic apply/rollback safety story ('multi-file apply captures pre-apply snapshots and restores them automatically') silently does NOT cover binary files: every file routes through read_to_string/write_string (UTF-8), so a binary/non-UTF-8 file in a multi-file patch errors and a partially-applied patch could fail to restore cleanly — undermining the rollback guarantee the apply path depends on.
- **현재 상태:** apply/rollback route through read_to_string/write_string in crates/nerve-patch/src/lib.rs:459-469,248,580-581 (UTF-8 only); a non-UTF-8 file targeted by modify/delete/rename errors (PatchError::Io). Git binary-diff format is unsupported.
- **목표 상태:** Snapshot/rollback read and write bytes (Vec<u8>) so binary files at least round-trip safely for the rollback guarantee; git binary patches are either supported or explicitly rejected with a clear error (no silent partial-apply that cannot restore).
- **접근법:** In crates/nerve-patch/src/lib.rs:459-469 and :580-581, switch the snapshot/restore I/O from read_to_string/write_string to byte-based read/write so the pre-apply snapshot and rollback handle binary content; keep the unified-diff line machinery for text. For binary content changes, either add git-binary-patch support or return a clear Unsupported error BEFORE any partial mutation so multi-file apply stays atomic. Update README to state the honest scope.
- **파일 seam:** crates/nerve-patch/src/lib.rs:459-469; crates/nerve-patch/src/lib.rs:248; crates/nerve-patch/src/lib.rs:580-581; README/nerve-101.md:235 (scope doc)
- **리스크:** Byte-based I/O must preserve existing text behavior (line endings, SHA-256 pre-check). The key correctness property is that a multi-file apply touching a binary either fully succeeds or fully rolls back — partial-apply-without-restore is the failure to eliminate.
- **수락 기준:** cargo build/test green + clippy -D warnings; test proves a multi-file patch containing a binary file either applies+rolls back cleanly or is rejected atomically before any mutation, and that text patching is byte-for-byte unchanged; two consecutive no-blocking codex reviews; invariant: apply remains dry-run-by-default and --apply-gated; this strengthens rollback integrity and never relaxes the apply gate or the deterministic verifier.

#### H17 — Broaden the progress parser and add ledger/checkpoint reconcile + dedicated writer for multi-instance observability

**Wave 4 · effort M · ⬜ 미착수** · 의존: —

- **왜(보안 근거):** Two safe-by-design but ecosystem-limited observability gaps: (1) the stall/no-progress backstop only parses libtest/pytest summaries, so for Go/Jest/JUnit/ctest the loop falls back to identical-hash stall detection a churning lead can evade longer; (2) checkpoint/ledger writes are single-writer inline best-effort, so under concurrent Mayor/Patrol (Mode C) writeback can contend/corrupt and a stale ledger silently misreports cross-patrol status. Neither is a gate weakening, but both reduce the operator's ability to observe/steer long runs (structural gap #2).
- **현재 상태:** parse_progress (crates/nerve-core/src/goal.rs:288-319) recognizes only libtest/pytest; unrecognized -> None. record_round does inline synchronous checkpoint writes ('a dedicated writer task is a future S9 concern', crates/nerve-core/src/lib.rs:477-479,541-545). Ledger/mailbox writes are best-effort warn-only with no reconcile (crates/nerve-core/src/mayor_patrol.rs:935-945).
- **목표 상태:** Progress recognizers extended (pluggably) to the other auto-detected ecosystems (go test, npm/jest) keeping the reject-only/worst-stream invariant; a dedicated async checkpoint writer task and a defined concurrency story under multi-instance; and a ledger-rebuild/reconcile command (or doctor check) that regenerates the projection from the authoritative pending/claimed/done/failed dirs.
- **접근법:** Extend parse_progress (goal.rs:288-319) with additional summary recognizers behind the existing pessimistic-min-across-streams logic; add a dedicated writer task replacing the inline record_round checkpoint write (lib.rs:477-479,541-545); add an `nv doctor`/reconcile command that re-derives ledger.json from the queue dirs (mayor_patrol.rs:935-945) — dirs remain authoritative (if they disagree, dirs win, already proven by test).
- **파일 seam:** crates/nerve-core/src/goal.rs:288-319; crates/nerve-core/src/lib.rs:477-479,541-545; crates/nerve-core/src/mayor_patrol.rs:935-945
- **리스크:** Progress recognizers must stay strictly reject-only/pessimistic (a misparse must never increase apparent progress). The async writer must not introduce a path where checkpoint state is treated as authoritative for acceptance. Reconcile must never let the ledger override the dirs.
- **수락 기준:** cargo build/test green + clippy -D warnings; tests prove (1) new recognizers report worst-stream progress and unrecognized output still yields None, (2) concurrent checkpoint writes do not corrupt under multi-instance, (3) reconcile rebuilds the ledger from dirs and dirs win on disagreement; two consecutive no-blocking codex reviews; invariant: progress is additive reject-only telemetry and ledger/checkpoints stay NON-authoritative — none of this can move a verdict toward acceptance.

#### H18 — Codify the anti-patterns and per-surface ConfigSource provenance as standing CI lints / invariant tests

**Wave 4 · effort M · ⬜ 미착수** · 의존: H5

- **왜(보안 근거):** The five anti-patterns (yolo-default, LLM-opinion gate, mid-gen kill of verifier/rollback, recursive nesting, fake consensus) and the per-surface consent guarantee (Project-sourced execution refused without NERVE_TRUST_PROJECT_VERIFIER) are currently honored by convention and code review. As P1 adds new SandboxConfig fields, MCP policies, env validation, and config knobs, each is a chance to silently regress a gate-bypass (a new execution-enabling knob that does NOT route through ConfigSource would let a repo-local file opt the operator into code execution). Mechanizing these as tests makes the negative space of the thesis enforceable, not aspirational.
- **현재 상태:** Anti-patterns are documented prose guardrails (roadmap:78-85); ConfigSource provenance is enforced for the builtin verifier specifically (roadmap:140-143) but is a per-surface guarantee re-applied by hand. No standing test/lint asserts that every execution-enabling config surface routes through provenance or that the closed MailKind enum (roadmap:638-653) stays closed.
- **목표 상태:** Standing invariant tests / CI lints that fail if: (a) a new config field that can enable code execution does not route through ConfigSource provenance; (b) any disk-backed channel gains apply/consent semantics (MailKind stays closed; .nerve approvals stay audit-only and unread by the gate); (c) the default sandbox/verifier modes change from Off; (d) the apply path defaults to anything but dry-run. These are the same gate the P1 items themselves must clear, applied repo-wide.
- **접근법:** Add invariant tests in the relevant crates asserting: SandboxConfig/verifier defaults remain Off (nerve-config/src/lib.rs); the apply gate reads ONLY the in-memory ApplyConsent and never .nerve/approvals (roadmap:489-491,508-511); MailKind has exactly Note/Progress/Reclaimed (roadmap:638-653); RunOptions defaults apply=false (roadmap:590-597). Add a documented checklist/lint that every new execution-enabling knob must thread ConfigSource provenance (the NERVE_TRUST_PROJECT_VERIFIER pattern). Land the skeleton early (alongside Wave 2) so it guards subsequent items.
- **파일 seam:** crates/nerve-config/src/lib.rs:267-280 (SandboxConfig defaults); crates/nerve-core (ApplyConsent / approvals audit-only path, roadmap:489-491); crates/nerve-core/src/mayor_patrol.rs (MailKind closed enum); crates/nerve-core (RunOptions apply default)
- **리스크:** Low — these are tests/lints, not behavior changes. The main risk is incompleteness (grep-not-AST: a new surface could be added without a corresponding lint); mitigate by making the provenance routing a typed requirement where possible rather than a name-based grep, and documenting the checklist for reviewers.
- **수락 기준:** cargo build/test green + clippy -D warnings; the new invariant tests FAIL on a deliberately-introduced regression (default flipped to On, approvals read by the gate, MailKind widened with a consent variant, apply defaulted true) and pass on the real tree; two consecutive no-blocking codex reviews; invariant: this item only ADDS enforcement of the existing thesis (deterministic verifier sole authority, dry-run default, loud opt-in for execution) and cannot itself weaken any gate.

