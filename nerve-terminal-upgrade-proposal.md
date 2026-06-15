# Nerve 터미널 업그레이드 제안서

> 작성일: 2026-05-22
> 개정 이력:
> - 2026-06-15 (1차): Loop Engineering 프레이밍 + Cherny 3단계 정지 조건 반영
> - 2026-06-15 (2차): `/goal` vs `/loop` 혼동 정리, Nerve 모드 선택/방향성 §0.6 신설
> 대상 버전: Nerve v0.1.9 → v0.2.0 (v1.0까지의 단계적 로드맵 포함)
> 검토 범위: Claude Code 2.1.x / Codex CLI v0.130 신규 기능 + Boris Cherny의 loop engineering 담론

---

## 0. 요약 (TL;DR)

Nerve는 이미 슬래시 명령 20종, 서브커맨드 11종, raw TTY 라인 에디터,
세션·패치 영속성을 갖추고 있다. 그러나 Claude Code와 Codex가 최근 1년 사이
추가한 **두 축**이 비어 있다.

1. **가시성** — 진행 중인 lead/reviewer 작업의 실시간 상태(라운드, 비용, ETA)
   가 정적 `/status` 출력에만 의존한다.
2. **목표 지향 자동화** — `max_refinement_rounds`만으로 종료를 판정하므로
   "조건 충족 시 자동 stop / 미충족 시 자동 재시도" 워크플로우가 없다.

이 문서는 위 두 격차를 메우기 위한 3단 우선순위 로드맵을 정의한다.
**Tier 1(상태 바 + `/goal` + 템플릿 검색) + Tier 2g(`/budget` 인터랙티브 노출)
네 가지**를 v0.2.0으로 묶는 것이 ROI가 가장 크다.
(상태 바·템플릿 = **가시성** 축, `/goal`·`/budget` = **목표 지향 자동화** 축
— 위 두 격차에 1:1로 분배된다.) 이 네 항목이 모두 들어오면
Cherny가 식별한 것으로 정리되는(§0.5.3) 프로덕션 loop의 3가지 정지 조건
(max iteration / no-progress / budget cap)이 v0.2.0에서 모두 충족된다.

전체 로드맵은 Boris Cherny가 'Acquired Unplugged' (2026-06-02)에서 정리한
것으로 알려진 **3단계 loop 진화**와 loop engineering 담론의 **harness 관점**
에서 재배치되어 있다 (§0.5 인용 정책 박스 참조 — 1차 transcript 미확보,
Njuguna 2026-06 등 2차 정리 매체 기준).

### 방향성 요약 (§0.6 인용)

Nerve는 **achievement-first orchestrator** 포지션을 노린다.

- **Mode A (Achievement)** — `/goal` 패턴, v0.2.0 우선 완성 대상
- **Mode B (Recurrence)** — `/loop` 패턴, v0.5.0 검토 (수요 검증 후)
- **Mode C (Multi-instance)** — Mayor/Patrol, v1.0.0 단계

이 순서는 의도적이다. 단일 에이전트 도구(Claude Code/Codex)와의 차별점인
**adversarial reviewer + deterministic gate** 를 Mode A에서 끝까지 다듬은
다음 위로 쌓는다.

---

## 0.5 Loop Engineering 관점 (보강)

이 절은 Addy Osmani의 ["Loop Engineering" 글](https://addyosmani.com/blog/loop-engineering/)과
Boris Cherny(Claude Code 책임자, Anthropic)의 'Acquired Unplugged'(2026-06-02)
발언을 기준으로 Nerve의 위치를 다시 잡는다.

> ※ **인용 정책**: 본 절의 Cherny 발언은 'Acquired Unplugged' 1차 영상
> transcript가 미확보 상태이며, 모든 인용은 Ezekiel Njuguna(Medium 2026-06)
> 정리 및 §2.8의 2차 정리 매체 기준이다. 1차 출처 확보 시 인용 톤을
> 단정형으로 강화 예정.

### 0.5.1 Cherny가 정리한 것으로 알려진 loop 진화 3단계

| 단계 | 모습 | 인간의 역할 |
|------|------|-------------|
| 1단계 | 자동완성 + per-line 지시 | 직접 코드 작성, 모델은 보조 |
| 2단계 | 5~10개 Claude 세션 병렬, 사람이 각각 프롬프트 | 모든 세션의 운전수 |
| 3단계 | "I don't prompt Claude anymore. I have loops that are running. They're the ones that prompting Claude." | loop **설계자**, 종료 조건 정의자 |

Cherny는 2025년 12월 한 달 동안 Claude Code 레포에 머지한 **259개 PR 전부**
가 Claude Code 자신이 작성한 것이라고 보고한 것으로 정리된다 (§2.8 Njuguna
Medium 2026-06 기준). 2025년 11월 IDE 삭제 후 재설치 없음 — 동 정리 매체
기준.

### 0.5.2 Harness Engineering 5단계 (OBSERVE → PLAN → ACT → REFLECT → REPEAT)

Cherny가 묘사한 것으로 정리되는 harness 패턴은 단일 LLM 호출이 아닌 **5단계
자율 순환**(loop engineering 담론에서 흔히 OBSERVE→PLAN→ACT→REFLECT→REPEAT로
정리됨)이다. Nerve의 기존 synaptic loop와 매핑하면:

| Harness 단계 (loop engineering 담론 정리) | Nerve의 현재 구현 | 격차 |
|-------------|------------------|------|
| OBSERVE | adapter `AgentEvent::Stdout` 수집 | 외부 신호(테스트·CI) 미연결 |
| PLAN | 없음 (lead prompt가 곧 dispatch) | `/plan` 모드 부재 |
| ACT | lead.implement + `/apply` | OK |
| REFLECT | reviewer.review + refinement round | OK |
| REPEAT | `0..=max_refinement_rounds` 고정 카운터 | **종료 조건이 카운터뿐** |

### 0.5.3 Cherny가 식별한 것으로 정리된 "프로덕션 loop 필수 3가지 정지 조건"

현재 Nerve는 (1)과 (3)이 코어에 구현되어 있고, (2)만 미구현이다.
사용자가 인터랙티브 모드에서 (3)을 등록·조정·관찰하는 표면은 비어 있다.

1. **Max iteration** — 진전 여부와 무관한 hard ceiling
   (현행 `max_refinement_rounds`, ✅ 구현됨)
2. **No-progress detection** — 의미 있는 변화 없이 순환할 때 토큰 소비
   전 중단 (❌ 미구현, §3 Tier 1b에서 도입)
3. **Budget cap** — 누적 토큰·비용 임계 초과 시 종료 (✅ 코어 구현됨 —
   `nerve-core/src/lib.rs:392` `exceeds_budget()`가 매 라운드 호출되며
   `RunReport.budget_exceeded` 플래그를 세팅. ⚠ 인터랙티브 표면·UX는 비어 있음)

→ 이 3가지가 §3의 **Tier 1b(`/goal`)와 Tier 2g(`/budget` 인터랙티브 노출)**
의 직접 근거다. Loop engineering 담론에서 자주 인용되는 "the only thing
agentic about it is the Anthropic bill at the end of the month"처럼, 비용
게이트 없는 loop은 프로덕션이 아니다. Nerve는 게이트 자체는 갖췄으니
사용자가 그것을 만지고 보는 표면을 v0.2.0에서 만든다.

### 0.5.4 Validator 패턴 — Nerve는 이미 갖고 있다

Loop engineering 담론에서 자주 인용되는 spring 2026 `/goal` 구현 패턴은
"작은 validator 모델이 작업 완료를 확인할 때까지 ralph loop 패턴을 실행"하는
형태로 알려져 있다 (§2.8 Njuguna 정리 기준; 'ralph loop' 용어 자체는 Geoffrey
Huntley의 'ralph wiggum as a software engineer'에서 비롯되어 Cherny 발화로
직접 귀속할 수 없다). **Nerve의 Reviewer는 이미 validator 역할을 하고 있다.**
새로 만들 필요 없이:

- Reviewer `Verdict::LGTM` ⇔ validator pass
- Reviewer `Verdict::Block` ⇔ validator hard fail (단독 종료 강제)
- Reviewer `Verdict::RequestChanges` ⇔ validator soft fail → REPEAT
- `Verdict::Block` + `RunReport.no_progress=true` 플래그 ⇔ No-progress
  강제 종료 (신규 enum 변형 없이 기존 Block 재사용, 결정표는 §3 Tier 1b
  "Verdict × CheckResult 결정표" 참조)

단, `Verdict::Block`은 budget cap 초과 및 no-progress 강제 종료에도 재사용된다
(§3 Tier 2g, §3 Tier 1b ma-1 참조; `nerve-core/src/lib.rs:408`). 세 경우는
`RunReport.budget_exceeded` / `no_progress` 플래그로 구분되며, 사용자 표면
(`/status`, 상태 바)에서 (1) "검증 실패로 인한 Block", (2) "예산 초과로 인한
Block", (3) "no-progress로 인한 Block"의 세 라벨로 보이도록 §3 Tier 1a·1b·2g가
함께 다룬다.

따라서 `/goal`은 **새 validator를 도입하는 게 아니라, 기존 reviewer의 판정
+ 사용자 정의 deterministic check(shell/regex)를 AND 결합**하는 형태로 간다
(§3 Tier 1b 참조).

### 0.5.5 Cherny의 운영 예시

- `/loop babysit all my PRs. Auto-fix build issues, and when comments come in,
  use a worktree agent to fix them.` — 자연어 한 줄로 loop 등록
- Steve Yegge가 회자한 **Gas Town** 패턴(2차 정리 기준 2026-01, 1차 출처
  미확보 — §2.8 참조): 20~30개 Claude Code 인스턴스를 Mayor 에이전트가 조율,
  patrol 에이전트가 연속 루프, **상태는 전부 git에 저장**되어 크래시/재시작 견딤

→ "상태를 git에 저장"은 Nerve의 `.nerve/` 디렉터리 패턴과 정확히 같은 철학.
다중 인스턴스 조율은 §3 Tier 3 신규 항목으로 흡수한다.

### 0.5.6 Osmani가 경고한 실패 패턴

Osmani의 'Loop Engineering' 글 및 관련 담론에서 일반적으로 회자되는 세 가지
실패 패턴. Loop을 잘못 쓰면:

- **미검증 자동 배포** — validator 없는 loop이 실수를 반복
- **Comprehension Debt** — loop이 빠를수록 개발자-코드 괴리 증대
- **Cognitive Surrender** — "자동이니까" 판단 회피

→ Nerve의 `dry-run` 기본값과 `--apply` 명시 요구는 이미 첫 번째를 방어한다.
나머지 둘은 **§3 Tier 1a 상태 바(가시성 회복)** 와 **§3 Tier 2f `/plan`
(사람 승인 게이트)** 가 완화한다.

### 0.5.7 `/goal` vs `/loop` — 흔한 혼동 정리

Anthropic의 Claude Code 공식 문서(`goal.md`, `scheduled-tasks.md`; §2.8 참조)는
두 명령을 **"다음 턴이 무엇으로 시작되는가"** 기준으로 구분하는 것으로 정리할
수 있다. 아래 표는 두 문서의 항목과 Nerve 측 해석(적합 작업, 비용 특성)을
함께 담은 것이며, 두 문서가 본 표의 구분 기준을 그대로 명시하지 않을 수
있다. 이 차이는 Nerve의 mode 선택(§0.6)을 결정짓는 핵심이다.

| 차원 | `/goal` | `/loop` |
|------|---------|---------|
| 다음 턴 시작 트리거 | 이전 턴이 끝나는 즉시 | 시간 간격이 경과할 때 |
| 종료 판정 주체 | validator 모델이 yes/no 평가 | 사용자 stop / 7일 만료 / Claude self-end |
| 동시성 | 세션당 1개만 | 세션당 최대 50개 |
| 모델링 대상 | **달성(achievement)** | **반복(recurrence)** |
| 적합한 작업 | finish line이 명확한 작업 | 외부 이벤트 폴링·정기 작업 |
| 비용 특성 | 짧고 진한 burst | 길게 얇게 지속 |

**구체 use case 매핑**:

| 작업 | 적합 | 이유 |
|------|------|------|
| "모든 테스트 통과까지 auth 모듈 마이그레이션 끝내" | `/goal` | 측정 가능한 종료 조건 |
| "5분마다 배포 상태 체크" | `/loop 5m` | 주기적 폴링, 종료 시점 미상 |
| "디자인 문서 acceptance criteria 충족까지" | `/goal` | 검증 가능한 종료 조건 |
| "PR 댓글 올 때마다 처리" | `/loop` | 외부 이벤트 대기 |
| "큰 파일을 size budget 아래로 쪼개기" | `/goal` | size라는 측정 가능한 조건 |
| "매일 아침 이슈 트리아지" | `/loop` (cron) | 시간 기반 스케줄 |

(수치 출처: §2.8의 `goal.md`/`scheduled-tasks.md` 1차 URL은 §2.8에 확보됨.
다만 본 표의 anchor·수치 매핑(`#requirements`, `#seven-day-expiry`, '세션당
1개/최대 50개', '7일 만료')은 2026-06 시점 정리 기준이며 docs 개정 시 변경
가능. 동일 docs의 다른 항목(line 284·642 모델 이름)이 '1차 확인 필요'로
표기된 것과 톤 정합.)

**한 줄 요약**: `/goal`은 **"끝까지 가는"** 명령, `/loop`은 **"계속 깨우는"**
명령. 이 구분은 §0.6에서 Nerve가 어느 쪽을 우선 채택할지의 근거가 된다.

---

## 0.6 Nerve의 모드 선택과 방향성 (Mode Selection & Direction)

본 절은 §0.5의 loop engineering 담론을 Nerve의 도메인(코드 패치 + Lead/Reviewer
adversarial orchestration)에 적용해, **"어떤 mode를 언제 쓸 것인지, 무엇을
선결해야 하는지, 어디로 가는지"** 를 정의한다.

> ※ **인용 정책 승계**: 본 절(§0.6)의 Cherny/Yegge 귀속 표현(예: 'Cherny 매핑',
> 'Cherny의 3단계 loop 진화', 'Steve Yegge의 Gas Town')은 모두 §0.5 인용 정책
> 박스를 그대로 따른다 — 1차 transcript/출처 미확보 상태이며, Njuguna(Medium
> 2026-06)·§2.8 2차 정리 매체 기준이다. 1차 출처 확보 시 톤 강화 예정.

### 0.6.1 Nerve의 도메인 특성

Nerve는 **다음 세 가지로 정의되는 좁고 깊은 도메인**을 갖는다.

1. **결과물의 형태가 결정적**: unified diff 패치 (apply/rollback 가능)
2. **adversarial validator가 내장**: Lead가 만든 patch를 Reviewer가 비평
3. **사용자 확인이 필수**: `dry-run` 기본, `--apply` 명시 요구

이 세 특성은 **`/goal` 모델(achievement)이 자연 맞춤**이고 **`/loop`
모델(recurrence)은 부자연**이라는 결론으로 이어진다. 코드 패치는 "5분마다
한 번씩 같은 패치를 적용"하는 작업이 아니다.

### 0.6.2 Nerve의 세 가지 운영 모드 정의

Cherny의 3단계 loop 진화(§0.5.1)에 맞춰 Nerve의 운영을 세 모드로 구획한다.

#### Mode A — Achievement Loop (Synaptic Goal Mode)

> **"이 조건이 충족될 때까지 Lead/Reviewer 라운드를 자동으로 이어간다"**

- **트리거**: 사용자가 `nv "<task>" /goal "<condition>"` 으로 1회 호출
- **반복 단위**: 한 번의 synaptic round (lead.implement → reviewer.review)
- **종료**: Reviewer LGTM ∧ deterministic check pass, OR max_rounds, OR
  no-progress, OR budget cap
- **Cherny 매핑**: `/goal` + harness 5단계 (OBSERVE→PLAN→ACT→REFLECT→REPEAT)
- **적합 상황**:
  - 명확한 finish line이 있는 코드 변경 (테스트 통과, 빌드 성공)
  - 측정 가능한 종료 조건 (파일 크기, 함수 개수, 린트 0건)
  - 디자인 문서·아키텍처 결정의 acceptance criteria 만족
  - 마이그레이션 작업 (call site 0개까지)
- **선결 요구사항**:
  - ✅ Lead/Reviewer 어댑터 (구현됨)
  - ✅ refinement round 카운터 (구현됨)
  - ⚠ deterministic check evaluator (§3 Tier 1b)
  - ⚠ no-progress detector (§3 Tier 1b — patch hash 비교)
  - ⚠ budget cap (§3 Tier 2g)
  - ⚠ 실시간 가시성 (§3 Tier 1a status bar)

#### Mode B — Recurrence Loop (Scheduled Synaptic Mode)

> **"주기적으로 같은 task를 깨워서 새 synaptic round를 시작한다"**

- **트리거**: `nv loop <interval> "<task>"` 또는 외부 cron이 `nv "<task>"` 호출
- **반복 단위**: 전체 synaptic loop 한 사이클 (한 task = 여러 round)
- **종료**: 사용자 stop, 7일 만료, 또는 N회 연속 no-op patch (변경 없음)
- **Cherny 매핑**: `/loop` (시간 트리거)
- **적합 상황**:
  - PR 댓글 들어올 때마다 자동 fix-up patch
  - CI 실패 → 자동 진단 patch 시도 (사람 승인 후 apply)
  - 매일 아침 lint 정리 / 의존성 업데이트 dry-run
- **부자연한 상황** (Mode B로 풀지 말 것):
  - 새 기능 구현 — finish line이 흐릿하면 비용만 누적
  - 한 번 끝내야 하는 마이그레이션 — Mode A가 맞음
- **선결 요구사항**:
  - ⚠ daemon 영구화 (현재 `nv daemon --rpc` 부분 구현)
  - ❌ cron 표현식 또는 fixed/dynamic interval (미구현)
  - ❌ 이벤트 트리거 (webhook, GitHub Action) (미구현)
  - ❌ 자동 만료 / loop ID 관리 (미구현)
- **결론**: **v0.2.0 스코프 밖**. Mode A가 안정화된 이후 v0.5.0 검토.

#### Mode C — Multi-Instance Orchestration (Mayor/Patrol Mode)

> **"여러 patrol(Lead/Reviewer 쌍)이 동시에 큐의 task를 갈아치운다"**

- **트리거**: `nv mayor --patrols N` 가 작업 큐를 watch, idle patrol에 dispatch
- **반복 단위**: patrol마다 Mode A 한 사이클
- **종료**: 큐 비움 또는 global budget 소진
- **Cherny 매핑**: Steve Yegge의 Gas Town (출처 확인 필요 — §2.8;
  20~30 Claude Code 인스턴스 + Mayor)
- **적합 상황**:
  - PR 백로그를 야간 배치로 처리 (10개 PR → 10개 patrol 병렬)
  - 대규모 리팩토링을 파일별 task로 쪼개 분산
  - 보안 audit을 디렉터리별로 병렬화
- **선결 요구사항**:
  - ⚠ Mode A 완성 (각 patrol이 Mode A를 돌리기 때문)
  - ⚠ worktree 격리 (§3 Tier 2d)
  - ⚠ global budget + per-patrol sub-budget (§3 Tier 2g)
  - ⚠ RPC 이벤트 확장 (§3 Tier 2e — patrol 상태 추적)
  - ⚠ 작업 큐 JSON (§3 Tier 3j)
- **결론**: **v1.0.0 단계**. Tier 2 전부 + Tier 1a + 1b가 모두 안정화된
  이후에만 안전.

### 0.6.3 Nerve의 정체성 — Claude Code/Codex와의 차별점

| 축 | Claude Code / Codex | Nerve |
|----|---------------------|-------|
| 추상화 레벨 | 단일 에이전트 harness | **다중 에이전트 orchestrator** |
| Validator | 작은 모델(`goal.md` 정리 기준, 모델 이름 1차 확인 필요) | **또 다른 강한 LLM (Codex/Claude)** |
| Loop 단위 | 한 turn = 한 LLM 호출 | **한 round = lead + reviewer (2 호출)** |
| Validator 호출 수 | 1 (모델 자기 검증) | **2 (강한 모델이 비평, 토큰 ~2배)** |
| 최적화 대상 | 속도·반응성 | **품질·검증 강도** |
| 사용자 인터페이스 | 인터랙티브 채팅 | **CLI 명령 + dry-run/apply 게이트** |

**결정적 차별점**: Nerve의 validator는 **"같은 모델이 자기 작업을 채점"하는
구조가 아니라 "다른 강한 모델이 비평"** 한다는 점. Osmani가 'Loop Engineering'
에서 인용한 Cherny 식 표현 "자신의 숙제를 채점하는 모델은 너무 관대"의 정확한
안티테제다 (§2.8 Osmani 글 기준; 1차 출처 확보 시 톤 강화 예정).

→ Nerve의 가치 명제: **adversarial review를 내장한 achievement loop**.

### 0.6.4 단계적 로드맵 (v0.2.0 → v1.0.0)

| 버전 | 목표 | 포함 항목 | 활성화되는 mode |
|------|------|----------|----------------|
| v0.2.0 | Mode A 기본 완성 | §3 Tier 1 a+b+c + 2g | Mode A (기본형) |
| v0.3.0 | Mode A 정밀화 | §3 Tier 2 d+e+f + Tier 1b Phase 2 (자연어→GoalSpec 등록) | Mode A (worktree 격리 + plan 게이트 + RPC 라이브) |
| v0.4.0 | 가시성 완성 | §3 Tier 3g (ratatui) | Mode A (TUI 패널) |
| v0.5.0 | Mode B 도입 결정 시점 | (외부 수요 검토 후) `nv loop` 신설 또는 보류 | Mode B (선택) |
| v1.0.0 | Mode C 활성화 | §3 Tier 3 h+i+j | Mode C (Mayor/Patrol) |

→ **버전 간 cross-reference**: Tier 2 전체(d/e/f/g)는 v0.3.0에서 완료되며,
Mode C(§0.6.5)는 v1.0.0에서 활성화된다. v0.5.0은 Mode B의 §0.6.5 기준 6개
요구사항 중 5개가 신규이므로 **v0.5~v0.7 사이 추가 마이너 버전 1~2회 누적**이
필요하며, v1.0.0은 Mode C의 §0.6.5 기준 5개 모두 신규 또는 Tier 2 의존이라
**Mode A·B 안정성 의존 + 인증 토큰 분리·작업 큐 등으로 v0.7~v0.9 누적 작업**이
필요하다. v0.5.0 → v1.0.0 점프는 실제로 Mode B 수요 검증 결과에 따라 0.6~0.9
사이 여러 마이너 릴리스를 거쳐 누적 도달하는 경로이며, 단일 점프가 아니다.

**의도적 순서**: Mode A를 먼저 끝까지 다듬는다. Cherny가 식별한 것으로
정리되는(§0.5.3 참조) 프로덕션 loop의 3가지 정지 조건이 v0.2.0에서 모두
충족되도록 한 후, Mode B는
**수요가 명확할 때만** 도입한다. Mode B를 미리 만들면 도메인에 맞지 않는
복잡도가 누적될 위험이 있다 (§0.5.6의 "Comprehension Debt").

### 0.6.5 요구사항 만족 매트릭스

각 모드가 작동하려면 반드시 충족해야 하는 요구사항과 현재 상태.

#### Mode A — Achievement Loop

| 요구사항 | 현재 상태 | 충족 항목 |
|----------|----------|----------|
| 종료 조건 등록 명령 | ❌ | §3 Tier 1b (`/goal`) |
| Validator (Reviewer + check) | ⚠ Reviewer만 | §3 Tier 1b deterministic check |
| Hard ceiling (max iter) | ✅ `max_refinement_rounds` | (유지 — `/goal`과 카운터 공유, §3 Tier 1b ma-7 참조) |
| No-progress 감지 | ❌ | §3 Tier 1b patch hash 비교 (별도 카운터 미신설, `max_refinement_rounds` 내에서 작동 — ma-7 참조) |
| Budget cap (코어 게이트) | ✅ `exceeds_budget()` 매 라운드 호출 (lib.rs:392, 150/186/212/283/317) | (유지) |
| Budget cap 인터랙티브 노출 | ❌ 슬래시 명령·게이지 없음 | §3 Tier 2g (`/budget`) |
| 실시간 가시성 | ❌ | §3 Tier 1a status bar |
| Resume 시 goal 복원 | ❌ | §3 Tier 1b 후속 작업 |
| Dry-run 게이트 | ✅ 구현됨 | (유지) |

→ Mode A의 9개 요구사항 중 5개가 §3 Tier 1·2g에 매핑되어 v0.2.0에 진입한다
(분배: Tier 1a 1건 + Tier 1b 3건[종료 조건 등록·Validator + check·No-progress
감지] + Tier 2g 1건 = 5. Resume 시 goal 복원은 Tier 1b 후속으로 v0.3.0 이후
별도 처리). 나머지 3개(Hard ceiling, Budget cap 코어 게이트, Dry-run
게이트)는 이미 구현되어 있다. **9 = 5(v0.2.0 진입) + 3(이미 구현) + 1(후속)
이 산술 정합**. v0.2.0이 Mode A의 사용자 표면을 정의대로 완성한다.

#### Mode B — Recurrence Loop (선결 분석)

| 요구사항 | 현재 상태 | 필요 작업 |
|----------|----------|----------|
| 영구 daemon | ⚠ 부분 (`nv daemon --rpc`) | daemon이 사용자 세션과 독립적으로 살도록 보강 |
| Interval 등록 (fixed/dynamic) | ❌ | cron 파서 + dynamic 평가자 |
| 이벤트 트리거 (webhook) | ❌ | HTTP 엔드포인트 또는 file watcher |
| Loop ID + 만료 | ❌ | `.nerve/loops/<id>.json` 인덱스 |
| 누적 비용 추적 | ❌ 인터벌별 누산기 신규 | 코어 `exceeds_budget()` 재사용 + Mode A `/budget` 표면 확장 |
| 외부 통지 | ❌ | Slack/이메일 hook (선택) |

→ 6개 중 5개가 신규. **Mode B는 v0.5.0까지 보류가 타당**.

#### Mode C — Multi-Instance (선결 분석)

| 요구사항 | 현재 상태 | 의존 |
|----------|----------|------|
| Worktree 격리 | ❌ | §3 Tier 2d |
| 작업 큐 | ❌ | §3 Tier 3j 신규 |
| Patrol 상태 RPC | ❌ | §3 Tier 2e |
| Global budget + per-patrol sub-budget (ceiling) | ❌ | §3 Tier 2g + 신규 |
| 동시 인증 토큰 분리 | ❌ | `CLAUDE_CONFIG_DIR` 인스턴스별 |

→ 5개 모두 신규 또는 Tier 2 의존. **Mode C는 Mode A + Tier 2 전체 완료
후에만 안전**.

### 0.6.6 방향성 한 줄

> **Nerve는 Claude Code/Codex의 단일 에이전트 loop 위에, adversarial review를
> 내장한 *achievement-first orchestrator*로 자리잡는다.**

- **Achievement-first**: Mode A를 v1.0까지의 1차 가치 명제로 삼는다. Claude
  Code 공식 문서의 `/goal` 패턴(§0.5.4, §2.8 `goal.md` 기준)을 단일 에이전트
  turn이 아니라 dual-LLM (lead+reviewer) round 위에 얹는다. Mode B와 C는
  Mode A의 안정성 위에만 얹는다.
- **Adversarial validator**: validator를 작은 모델로 두지 않고 강한 모델
  (Codex 또는 Claude)을 reviewer로 쓴다. 검증 비용을 감수하고 품질을 얻는다.
- **Deterministic gate**: LLM 판정만 믿지 않고 shell exit code + regex로
  사용자 정의 결정적 체크를 AND 결합한다 (§3 Tier 1b).
- **사람 in-the-loop**: dry-run 기본 + `/plan` (§3 Tier 2f) + `/apply` 명시
  요구로 cognitive surrender(§0.5.6)를 방어한다. Reviewer가 LLM 출력에서
  추출한 `suggested_patch`(`nerve-types/src/lib.rs:102`)도 자동 적용
  대상이 되어서는 안 된다(원칙). 현재 dry-run 게이트(`options.apply=false`)가
  1차 방어선이지만, `ConflictPolicy::ReviewerPriority`/`MergeAttempt` 정책 +
  `--apply` 조합에서는 `suggested_patch`가 `final_patch`로 자동 승격되어
  추가 확인 없이 적용될 수 있는 경로가 코어에 존재한다(`nerve-core/src/lib.rs:573-580`,
  §5 위험표 "코어 공통" 행 참조). `/goal` 자동 재투입 경로에서는 이 조합을
  거부하거나 인터랙티브 재확인 게이트를 부착해야 한다 (코드 가드는 1b
  구현 시 추가, 본 절은 명세 원칙).
- **Operational visibility**: 코어에 이미 있는 게이트(max iteration,
  `exceeds_budget()`)를 상태 바와 슬래시 명령으로 사용자 손에 쥐어준다
  (§3 Tier 1a, 2g). 게이트는 보이지 않으면 존재하지 않는 것과 같다 —
  §0.5.3에서 정리한 3가지 정지 조건이 인터랙티브 표면에 드러나야 사용자가
  신뢰한다.

이 다섯 가지가 Nerve의 "고유 곡선"이다. Claude Code의 속도·생태계와
Codex의 비용 효율은 §2에서 폭넓게 *차용*하되, 그 위에 **adversarial reviewer
+ deterministic gate** 라는 *추가 축*을 얹는 것이 Nerve의 차별점이다.
단순한 속도·비용 경쟁이 아니라, **dual-LLM adversarial review를 v0.2.0부터
인터랙티브 표면에서 기본값으로 강제하는 코드 패치 orchestrator** 포지션을
노린다 — 같은 모델이 자기 작업을 채점하지 않는 구조 자체가 Nerve의 정체성이다.
(단, `suggested_patch` 자동 승격 차단 가드는 §3 Tier 1b에서 완성된다 — §5
위험표 "코어 공통 (Mode A 전반)" 행 참조.)

---

## 1. 현재 Nerve 터미널 구현 인벤토리

### 1.1 슬래시 명령 (20개, `crates/nerve-cli/src/main.rs`)

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

- POSIX raw mode: `libc::tcsetattr` (main.rs:1664~1710),
  `RawTerminalGuard`로 suspend/resume
- 히스토리: Up/Down 화살표로 prompt history 순회 (main.rs:981~989)
- 자동완성: `/` 입력 → 슬래시 명령 팔레트(20개 하드코딩, main.rs:794~920),
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

**Claude Code / Codex 공식 문서**

- Claude Code Changelog — https://code.claude.com/docs/en/changelog.md
- Agent View Guide — https://code.claude.com/docs/en/agent-view.md
- `/goal` Command — https://code.claude.com/docs/en/goal.md
- Scheduled Tasks & `/loop` — https://code.claude.com/docs/en/scheduled-tasks.md
- Codex CLI Features — https://developers.openai.com/codex/cli/features
- Codex CLI v0.130 Reference — https://blakecrosley.com/guides/codex

**Loop Engineering 담론** (§0.5 근거)

> ※ Cherny 'Acquired Unplugged' 1차 출처(공식 팟캐스트/영상 URL) 미확보.
> 아래는 모두 2차 정리·해설 매체이며, §0.5의 Cherny 직접 인용은 모두 이
> 매체들의 요약을 거친 것이다. 1차 출처 확보 시 본 블록 최상단에 추가 예정.

- Addy Osmani, "Loop Engineering" — https://addyosmani.com/blog/loop-engineering/
- Ezekiel Njuguna, "What a Loop Actually Is: Boris Cherny's Three-Stage
  Definition" (Cherny의 'Acquired Unplugged' 발표 2026-06-02 정리) —
  https://medium.com/mountain-movers/what-a-loop-actually-is-boris-chernys-three-stage-definition-33dd2bfe01b3
- The New Stack, "The Anthropic leader who built Claude Code says he
  ditched prompting — now he just writes loops" —
  https://thenewstack.io/loop-engineering/
- explainx.ai *(aggregator)*, "Anthropic Engineer: Stop Prompting Claude, Build Loops" —
  https://explainx.ai/blog/anthropic-engineer-loops-prompts-ai-coding-harness-engineering-2026
- explainx.ai *(aggregator)*, "Loop Engineering: How to Design Coding Agent Loops That Run
  While You Sleep (2026 Guide)" —
  https://explainx.ai/blog/loop-engineering-coding-agents-claude-code-guide-2026
- The Neuron, "Claude Code's Creators Explain Agent Loops & How They Code" —
  https://www.theneuron.ai/explainer-articles/claude-code-creators-boris-cherny-and-cat-wu-explain-how-to-use-agent-loops/
- Geoffrey Huntley, "ralph wiggum as a software engineer" — 'ralph loop' 용어
  출처 (§0.5.4 참조). URL 미확보, 일반 검색 기준
- Steve Yegge, "Gas Town" — 출처 확인 필요 (Sourcegraph 블로그/Substack/
  팟캐스트 중 미확정, 2026-01 추정). §0.5.5, §0.6.2 Mode C, §3 Tier 3j의
  Mayor/Patrol 패턴 단일 근거

---

## 3. 우선순위별 차용 제안

> **§2↔§3 매핑 정책**: §2의 Claude Code/Codex 기능 중 §3 Tier 1~3에 등장하지
> 않는 항목(예: `--fork-session`, MCP `tool.allowed`, transcript navigation
> 일부)은 v1.0 이후 보류로 본다. 반대로 §3 Tier 2g(`/budget`)와 Tier 3j
> (Mayor/Patrol)는 §2가 아닌 §0.5의 Cherny/Yegge 담론에서 직접 유도된
> 항목이라 1:1 매핑이 없다 — 이 두 항목은 Nerve의 도메인 특화 확장이다.
>
> **인용 정책 승계**: §0.5 인용 정책(Cherny/Yegge 1차 출처 미확보, 2차 정리
> 매체 기준)은 §3 Tier 1b 헤딩의 'Cherny식'·Tier 3j '영감' 표현, §4·§5에서
> §0.5.3을 가리키는 'Cherny의 3가지 정지 조건' 류 표현 등 §3·§4·§5의 모든
> Cherny/Yegge 귀속 표현에도 그대로 적용된다 — 1차 출처 확보 시 톤 강화 예정.

### Tier 1 — 비용 낮고 가치 큼 (1a/1c 각 0.5~1일, 1b 2~3일)

#### 1a. 라이브 상태 바 (Agent View 스타일)

- **무엇을**: 인터랙티브 모드 헤더에 상시 한 줄
  ```
  nerve:claude:apply  lead⟳  rev✓  round 2/3  ⏱42s  $0.018
  ```
- **데이터 출처**: `RunReport.usage`(cost_microusd, tokens), refinement 카운터,
  `AgentEvent::Stdout/Stderr` 빈도
  - cost 표시(`$0.018`)는 §3 Tier 2g `/budget`이 요구하는 게이지의 piggyback이며,
    코어의 `exceeds_budget()`가 이미 매 라운드 호출 중(`lib.rs:392` + 5개 call site: 150/186/212/283/317)이라
    신규 hook이 아니다 — Tier 1a 상태 바는 그 값을 픽업해 노출만 한다
- **구현 위치**:
  - `crates/nerve-cli/src/main.rs:904` (`InteractiveLineEditor`) 위에
    `StatusBar` 구조체 추가
  - `crates/nerve-core/src/lib.rs`의 orchestrator → mpsc 채널로 상태 push
  - 아이콘 매핑: idle `∙`, thinking `⟳`, done `✓`, blocked `✗`
- **영감**: Claude Code agent view ✽/✻/∙/✢ + elapsed/spend 표시

#### 1b. `/goal` 명령 — Cherny식 종료 조건 + Validator 결합

**1b.1 명세**

- **무엇을**: `/goal tests pass && diff applied` 형태로 종료 조건 등록
- **동작**: orchestrator가 max_rounds 이전이라도 조건 충족 시 stop;
  미충족이면 reviewer 피드백을 새 lead 프롬프트로 자동 재투입.
  `/goal` 자동 loop은 task 시작 시점의 워크스페이스 루트를 freeze하며,
  진행 중 인터랙티브 `/cd` 호출은 자동 재투입의 cwd에 영향을 주지 않는다
  (sec-7).
- **Validator 결합** (§0.5.4 참조): 종료 판정은 두 신호의 AND
  - **Reviewer 판정**: `Verdict::LGTM` (이미 구현됨)
  - **Deterministic check**: 사용자가 등록한 shell exit code + regex
- **Evaluator 설계**: LLM 호출 없이 **shell exit code + regex 매칭**만
  지원 (Claude Code의 `/goal`은 작은 모델 평가자를 쓰는 것으로 정리된다
  — §2.8 `goal.md` 참조; 정확한 모델 이름은 1차 확인 필요. Nerve는 코드
  패치 도구라 deterministic check가 도메인에 더 맞다)
- **Cherny 3가지 정지 조건 매핑** (§0.5.3):
  - Max iteration → `max_refinement_rounds` (현행 유지, hard ceiling)
  - **No-progress detection (신규)** → 연속 2라운드 동안 patch hash가
    동일하면 `Verdict::NoProgress`로 강제 종료. `RoundRecord`에
    `patch_sha` 필드 추가
    - **호환 가드**: `RoundRecord`는 현재 `#[serde(default)]` 미적용
      (`nerve-types/src/lib.rs:157-162`). 신규 `patch_sha` 필드는 반드시
      `#[serde(default)]` 어노테이션으로 추가해야 기존 `.nerve/sessions/`
      JSON과의 역직렬화 호환이 유지된다. `Option<String>`으로 두면
      "이 라운드에 패치가 없었음"과 "동일한 빈 패치"를 의미적으로 구분할
      수 있어 no-progress 판정의 false positive를 막을 수 있다.
    - **patch_sha 출처**: hash 입력은 `select_final_patch`의 출력
      (`nerve-core/src/lib.rs:573-580`) 기준. `ConflictPolicy::ReviewerPriority`
      또는 `MergeAttempt` 정책에서 lead patch가 매 라운드 달라져도 reviewer가
      동일 final_patch에 수렴하면 NoProgress로 판정된다. 단일 필드만으로 충분.
    - **정규화 규칙 (필수)**: hash 계산 전 `files`를 path 기준 정렬 → 각 파일의
      `path` + LF로 통일된 `modified` 내용만 직렬화 → SHA-256. `original_sha256`/
      `modified_sha256` 같은 별도 해시 필드와 메타데이터는 입력에서 제외해야
      동일 의미의 patch가 동일 SHA를 갖는다. 정규화 누락 시 no-progress 가드가
      우회되어 hard ceiling까지 토큰을 소비한다. 코어에 `NvPatch::canonical_hash()`
      헬퍼 신설 권장.
  - Budget cap → §2g(`/budget`)에서 별도 처리
- **구현 위치**:
  - `crates/nerve-config/src/lib.rs`에 `GoalSpec { check_cmd, success_pattern, max_no_progress }`
    타입 추가
  - `crates/nerve-core/src/lib.rs`의 `run_synaptic_loop` 종료 조건 hook
  - `/goal` 슬래시 명령 핸들러는 `main.rs`의 match-name 블록(약 1240–1393,
    `/quit` arm 1392 위) 안에 신규 arm으로 추가. 구현 완료 후 §1.1
    인벤토리의 슬래시 명령 카운트는 20 → 21로 갱신해야 한다 (Tier 2g `/budget`
    까지 들어오면 → 22).
- **자연어 등록 (Phase 2, v0.3.0 후속 — ma-5 참조)**: Loop engineering 담론에서
  자주 인용되는 "/loop babysit all my PRs..." 같은 형태처럼(§0.5.5 참조) 자연어
  한 줄을 받아 lead가 GoalSpec JSON으로 변환 → 사용자 확인 후 등록

**1b.2 운영 가드 (보안·호환·책임 분리)**

- **보안 가드 (sec-1)**: `/goal`의 deterministic check가 임의 shell command를
  실행하는 표면이라 다음 가드가 필수다:
  1. `check_cmd`는 **argv 배열** (`Vec<String>`)로 받고 `sh -c` 금지. 명시적
     opt-in으로만 `--shell` 허용.
  2. 실행 cwd는 task 시작 시 freeze된 워크스페이스 루트로 고정(§3 Tier 1b 동작 + sec-7 참조).
  3. PATH/환경변수 화이트리스트 (`HOME`, `LANG`, `NERVE_*`만 통과; `nerve.config.json::orchestration.check_env` (신규 필드, v0.2.0 추가)로 추가 허용).
  4. `tokio::time::timeout` 적용 (v0.2.0 어댑터 timeout과 같은 기본 5분 — §5 위험표 마지막 행, §4 묶음 선결 참조).
  5. `.nerve/goals/<id>.json` 영속화 전 path traversal 검사 (`../`, 절대 경로 거부).
  6. **Phase 2 자연어→GoalSpec 변환 결과**는 raw `check_cmd` + cwd + env까지
     사용자 확인 prompt에 표시한 뒤에만 저장. LLM이 만든 shell command가 곧바로
     evaluator에 들어가는 경로를 차단.
  7. **check_cmd stdout/stderr는 평가기 측에서 N MiB(기본 1 MiB) streaming cap**.
     `Command::output()` 금지 — `tokio::process::Command` + `stdout(Stdio::piped())`로
     `ChildStdout::take(N)` 누적 읽기, cap 초과 시 `CheckResult::Fail { reason: "output exceeded 1 MiB" }`
     + 라운드 종료. RPC `goal_check.output` 4 KiB truncate(§3 Tier 2e sec-4 3항)는 *emit 시점*
     이라 평가기 메모리 자체는 별도로 보호해야 한다. `nerve.config.json::orchestration.check_output_cap_bytes`로
     오버라이드.
  8. **stdin 격리**: check_cmd의 stdin은 `Stdio::null()` 강제, stdout/stderr는
     `Stdio::piped()`로 캡처해 부모 raw TTY와 분리. evaluator는 부모 stdin을 자식에게
     상속시키지 않는다 — `RawTerminalGuard`(`main.rs:1664-1710`) 활성 중 raw 키스트로크
     leak·echo 깨짐·SIGINT/SIGTSTP 전파 꼬임 방지.
  9. **자원 한도(권장)**: v0.2.0은 cgroups/job object 강제 미지원. `nerve.config.json::orchestration.check_ulimit`
     (옵션)으로 사용자가 nproc/메모리 cap을 등록 가능(Linux=`prlimit`, macOS=`launchctl limit`).
     `docs/security.md`에 권장 ulimit 예시 안내, v1.0에서 cgroups v2 재검토. fork bomb·OOM·fd
     고갈은 argv 강제·timeout만으로 차단되지 않는 표면이므로 minor 가드로 명시.

- **Verdict 호환 가드 (ma-1)**: No-progress 종료는 신규 enum 변형을 만들지
  않고 **기존 `Verdict::Block` + `RunReport.no_progress=true` 플래그** 패턴을
  쓴다(§0.5.4 매핑표 4번째 행 참조). 같은 `Block`이라도 (a) reviewer hard fail,
  (b) budget exceeded(`RunReport.budget_exceeded=true`), (c) no-progress
  (`RunReport.no_progress=true`) 세 경우는 플래그로 구분되어 §3 Tier 1a 상태 바
  종료 사유 라벨에 다르게 표시된다.

- **Evaluator 책임 분리 (ma-2)**: `GoalSpec` 타입은 `nerve-config`에 두고,
  `GoalEvaluator` 실행기는 `nerve-core::goal` 모듈에 신설한다. orchestrator는
  `run_synaptic_loop` 내부의 `reviewer.review()` 직후 evaluator를 호출하고,
  종료 판정에서 `Verdict::is_terminal_success`와 `CheckResult::Pass`를 AND 결합
  — Reviewer LGTM ∧ CheckResult Pass일 때만 종료된다 (호출지: `nerve-core/src/lib.rs:155-160`
  종료 분기). `CheckResult` enum(`Pass`, `Fail { reason }`, `Skipped`, 신설)은
  `nerve-types`에 두어 RPC 이벤트에도 그대로 흐른다.

- **Verdict × CheckResult 결정표 (ma-6)**:

  | Reviewer Verdict | Check Result | 결정 | 비고 |
  |------------------|--------------|------|------|
  | LGTM | Pass | **stop (성공)** | 종료 조건 충족 |
  | LGTM | Fail | next round + check 출력을 lead 프롬프트에 주입 | reviewer는 만족했지만 사용자 기준 미달 → lead 재시도 |
  | LGTM | Skipped (check_cmd 없음) | **stop (성공)** | check 미등록 시 reviewer 단독 판정 |
  | RequestChanges | Pass | next round | reviewer 우선 — check가 통과해도 코드 품질 문제 해결 후 다시 평가 |
  | RequestChanges | Fail | next round | 양쪽 모두 미달, 정상 refinement |
  | Block | * (any) | **stop (Block 우선)** | hard fail은 항상 종료 강제 |

  본 결정표는 **reviewer-emitted Verdict**에 한정된다. no-progress / budget
  exceeded로 인한 Block은 결정표 외부에서 orchestrator가 합성하며, ma-1 호환
  가드의 플래그(`no_progress` / `budget_exceeded`)로 구분된다.

  `Block`은 단독으로 종료를 강제한다 (§0.5.4 참조).

- **카운터 정책 (ma-7)**: `/goal` 자체 카운터를 따로 두지 않고 `max_refinement_rounds`
  를 공유한다. round_index 소진 시 `RunReport.goal_satisfied=false`로 마킹하고
  종료. `RoundRecord.round`는 라운드당 1회 기록, `CheckResult`는 `Option<CheckResult>`
  로 누적되어 history에 보존된다. §0.6.5 Mode A 매트릭스의 "Hard ceiling (max iter)"
  요구는 `/goal` 카운터와 *공유*되어 별도 카운터 미신설.
  **swap 시점**: `/goal` 재등록은 다음 라운드 시작 boundary에만 반영(라운드 중간
  swap 금지) — Verdict 결정 시점과 일관.

- **v0.2.0 스코프 (ma-5)**: 본 Tier 1b는 **Phase 1만** v0.2.0에 들어간다 —
  즉 `/goal "<argv 형식 check_cmd>"` 직접 등록 + 결정표 + no-progress + 보안
  가드 **1~5번까지**. **보안 가드 #6번(Phase 2 자연어→GoalSpec 변환 사용자 확인)
  과 Phase 2 자연어 등록**(LLM 변환)은 v0.3.0 후속으로 분리한다 (§4 묶음 설명
  참조). 이는 자연어→shell 변환의 보안 가드(위 6번)가 v0.2.0 스코프를 넘기
  때문이다.

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
- **트랜잭션 보장 (sec-5)**:
  1. main으로 머지 직전 `git rev-parse HEAD`를 `.nerve/worktrees/<patch-id>/main-pre.ref`
     에 ref backup 저장. 머지 도중 실패(컨플릭트, fs 에러, SIGINT) 시 `git reset
     --hard <backup>`으로 원복.
  2. `git worktree remove` 실패 시 `--force` 옵션 시도 → 그래도 실패하면 잔재
     디렉터리를 `.nerve/scratch/orphaned-worktrees/<patch-id>/`로 이동(인증 캐시
     누설 방지를 위해 권한 0600 적용).
  3. `nv doctor`에 "고아 worktree 검사" 추가: `.nerve/scratch/orphaned-worktrees/`
     비어 있어야 통과.
  4. dirty worktree(`git status --porcelain` non-empty) 검출 시 머지 거부.
  5. **symlink escape 차단**: 머지 직전 `git diff --name-only main-pre.ref HEAD`로 신규/변경
     파일 수집, 각 파일에 `std::fs::symlink_metadata().is_symlink()` + `canonicalize()`로
     prefix 검증. symlink 또는 main repo prefix 밖을 가리키는 파일 발견 시 머지 거부 +
     orphaned-worktrees로 격리. lead 어댑터가 NvPatch 외 직접 git/shell 명령으로 symlink를
     worktree 브랜치에 커밋해 main `.git/config`/인증 토큰 경로를 변조하는 경로 방어.
  6. **디스크 가용 공간 사전 검사**: 머지 시작 전 `statvfs(.nerve/)`로 가용 공간 확인(기본
     임계 100 MiB 미만이면 거부 + 사용자 안내). `main-pre.ref` 쓰기 ENOSPC 시 `ApplyError::DiskFull`
     로 즉시 abort하고 patch는 worktree에 보존. `nv doctor`에 `.nerve/scratch/orphaned-worktrees/`
     비어 있음 검사에 더해 디스크 잔량 임계 검사 추가.
  7. **reset 자체 실패 폴백**: `git reset --hard <backup>` 실패(read-only fs, immutable flag,
     AppleDouble metadata, ACL) 시 `git reset --merge` 폴백 → 그래도 실패하면 main HEAD를
     `.nerve/scratch/main-recovery/<ts>.bundle`로 `git bundle create` 백업 + RED 경고 +
     `nv doctor --recover` 안내. 잔재 worktree `mv` 실패 시에는 in-place `chmod 0600` +
     `.nerve/scratch/orphaned-worktrees/manifest.jsonl`에 위치 기록.

#### 2e. RPC 이벤트 스트리밍 확장

- 현재 4종 → 추가:
  - `round_start { round, lead_agent, reviewer_agent }`
  - `lead_stdout_chunk { round, bytes }`
  - `reviewer_stdout_chunk { round, bytes }`
  - `goal_check { goal_id, passed, output }`
  - `cost_update { tokens, cost_microusd }`
- 외부 UI(별도 `nerve-agent-view` 프로세스)에서 JSONL 소비 가능
- 구현: `main.rs:1824` emit 함수 일반화 + adapter `AgentEvent` pass-through
- **보안 (sec-4)**:
  1. RPC transport는 **Unix socket 우선 (`0600` perms, 사용자별 격리)**.
     TCP 사용 시 localhost 바인드 강제 + bearer 토큰 인증 의무화.
  2. `lead_stdout_chunk`/`reviewer_stdout_chunk`는 기본적으로 `bytes` 카운터만
     emit. raw 본문은 명시적 opt-in (`nv daemon --rpc --include-content`)에서만.
  3. `goal_check.output`은 사용자 shell 명령 stdout이라 API 키·토큰 누설 위험.
     N바이트(기본 4 KiB) truncate + 정규식 기반 시크릿 마스킹. 기본 패턴은 `AKIA...`,
     `sk-...`, `sk-ant-...`, `ghp_...`, `vrcl_...`, `org-...`, GCP service account JSON
     필드(`private_key_id`, `private_key`)를 포함하며 `nerve.config.json::secret_patterns`로
     사용자/커뮤니티 확장 가능. 라인 경계 회피(`sk-` + `proj-` + `XYZ...` 분할 출력)를
     막기 위해 sliding-window(기본 64자) 매칭 + base64/hex 인코딩 우회 대응으로 Shannon
     entropy ≥ 4.5 + length ≥ 32 휴리스틱으로 의심 토큰 마스킹. **JSONL invariant**:
     `output`/`stdout_chunk`의 raw payload는 serde JSON string으로만 직렬화하여
     `\n`/`{`/`}` 자동 escape — 한 줄=한 이벤트 invariant를 LLM 출력이 위장 이벤트로
     injection하지 못하게 강제.
  4. 파일 경로는 워크스페이스 루트 기준 상대 경로로 정규화, 절대 경로 금지.
  5. Mode C에서 patrol마다 별도 Unix socket 또는 토픽 prefix로 컨슈머 격리.
  6. **RPC payload hard cap + bounded channel**: 모든 RPC 이벤트는 직렬화 후 N KiB(기본
     64 KiB) hard cap. 초과 시 metadata(`truncated: true, original_size: N`) 동반 + 본문은
     head/tail 256B만 emit. raw 본문 opt-in(`--include-content`)에서도 동일 적용. emit
     queue는 컨슈머별 bounded channel(기본 1024 events) — 가득 차면 oldest drop +
     `dropped_count` metric 노출. multi-instance(Mode C)에서 한 patrol의 빠른 emit이 다른
     patrol consumer 버퍼를 막아 cascade hang/OOM되는 경로 방어.
  7. **envelope schema 버저닝**: RPC 이벤트는 `{schema_version: "1.x", kind, payload}`
     envelope 고정. `schema_version`은 semver string — minor bump는 필드 추가만 허용하고
     컨슈머는 unknown 필드 silently ignore, major bump는 daemon이 handshake 시 컨슈머
     max-supported version으로 downgrade하거나 거부. v0.2.0은 v1.0 envelope 고정.
     downgrade(daemon v0.5 → consumer v0.2) 시 unknown 필드는 round-trip 보존 정책.
  8. **bearer 토큰 lifecycle**: 1항의 TCP bearer 토큰은 `nv daemon --rpc` 시작 시 32B
     랜덤 생성 → `.nerve/session-meta/rpc-token`(0600) 저장, 데몬 종료 시 자동 삭제.
     토큰 stdout 출력은 `--print-token` 명시 opt-in에서만. rotation은 `nv rpc rotate-token`
     수동(v0.2.0은 자동 회전 미지원). Mode C에서는 patrol마다 별도 토큰 → 5항 socket
     격리와 결합. 누설 의심 시 데몬 재시작 = 토큰 새로 발급.

#### 2f. `/plan` (Plan mode)

- lead 호출 전 read-only 분석 → 단계 목록 출력 → 사용자 승인 후 실제
  dispatch
- 기존 `--dry-run`에 prompt prefix("write a plan only, no changes") +
  승인 UI를 얹는 형태
- 큰 리팩토링에서 토큰 낭비 방지에 큰 효과

#### 2g. `/budget` — 예산 인프라 인터랙티브 노출 (Cherny 3번째 stop condition)

> ⚠ **재포지셔닝 주의**: 예산 게이트 자체는 코어에 이미 구현되어 있다
> (`nerve-core/src/lib.rs:392 exceeds_budget()`, 라인 150/186/212/283/317에서
> 매 라운드 호출, 초과 시 `Verdict::Block` + `RunReport.budget_exceeded=true`).
> Tier 2g가 추가하는 것은 **인프라가 아니라 사용자가 그것을 보고 만지는
> 표면**이다. 신규 verdict 변형이나 hook을 만드는 게 아니다.

- **무엇을** (인터랙티브 표면 3가지):
  1. **`/budget` 슬래시 명령** — `/budget tokens=200000 cost=$5` 형태로 현재
     세션의 한도를 임시 오버라이드. 인자 없이 호출 시 현재 한도·누적·잔여
     출력
  2. **Tier 1a 상태 바 게이지** — `$0.018 / $5.00 (3.6%)` 실시간 표시.
     임계 70% 도달 시 색상 경고, 100% 도달 시 `✗ budget` 아이콘
  3. **종료 사유 가시화** — 현재 budget 초과는 `Verdict::Block`으로 종료
     되지만 `ReviewerFeedback.message`만 보면 일반 block과 구분이 안 됨.
     `RunReport.budget_exceeded=true` 플래그를 인터랙티브 출력에서
     `🚧 budget exceeded — see /status` 형태로 명시
- **데이터 출처**: `RunReport.usage`(tokens, cost_microusd) 이미 누적됨.
  실시간 게이지는 Tier 1a 상태 채널의 piggyback
- **기본값 출처**: `nerve.config.json`의 `orchestration.max_total_tokens` /
  `max_estimated_cost_microusd` (이미 로드되어 `exceeds_budget()`에 전달
  되는 중). `/budget`은 이 값을 세션 단위로 일시 오버라이드만 함
- **구현 위치**:
  - `/budget` 슬래시 핸들러: `crates/nerve-cli/src/main.rs` 의 슬래시 매치
    분기에 신규 arm
  - 세션 오버라이드: `nerve-core::Orchestration`을 mutable로 전달하거나
    오버라이드 레이어를 `run_synaptic_loop` 진입부에 주입
  - 게이지 렌더링: Tier 1a 상태 바 구조체 내부
- **주의 — 신규 작업이 아닌 것들**:
  - `Verdict::BudgetExceeded` enum 변형 추가 ❌ (현재 `Verdict::Block` 재사용)
  - `exceeds_budget()` 호출 추가 ❌ (이미 6곳에서 호출 중)
  - 라운드 종료 hook 신설 ❌ (이미 라운드 종료부에 wired)
- **§0.5.3에서 정리한 원칙**: 예산 게이트 없는 loop은 프로덕션이 아니다 —
  Nerve는 게이트는 갖췄으니, 게이지·명령·종료 사유 가시화로 *사용자가 그것을
  관리 가능한 상태로* 끌어올린다
- **Mode B 확장 경로**: §0.6.5 Mode B "누적 비용 추적" 행이 v0.5.0에서
  도입될 경우, `/budget`은 interval 인자(예: `/budget cost=$5 per=day`)를
  받아 인터벌별 누산 표면을 같은 슬래시 명령으로 흡수한다 — Mode A에서
  완성된 표면이 Mode B로 자연 확장되는 경로
- **권한 모델 (sec-3)**:
  1. **Raising은 글로벌 ceiling 강제**: `/budget cost=$10000`처럼 임의 증액
     불가. `nerve.config.json::orchestration.max_estimated_cost_microusd`
     (글로벌 한도)를 ceiling으로 강제하고, 그 위로 raise하려면 별도
     `--force` 플래그 + 인터랙티브 확인 prompt.
  2. **Lowering만 자유**: 세션 한도를 글로벌 이하로 줄이는 것은 자유 (안전
     방향).
  3. **Mode C 분배**: Mayor가 부여한 sub-budget이 patrol의 ceiling이 된다
     (용어 정의: **per-patrol sub-budget = patrol ceiling**, §0.6.2 / §0.6.5
     Mode C 매트릭스와 통일). patrol이 자기 `/budget`을 sub-budget 위로 raising
     시 Mayor 토큰 없이는 거부. global cap을 patrol이 우회할 수 없게 강제.
  4. **변경 감사**: `/budget` 호출마다 `.nerve/session-meta/budget-audit.json`
     (신규 파일)에 (이전 값, 새 값, 호출 시각, 사용자 확인 여부) 기록. `/status`
     출력에 누적 변경 횟수 노출.
  5. **입력 sanity**: `/budget` 파서는 (a) 음수·NaN을 u64 파서 단계에서 거부
     (`InvalidValue`), (b) `cost=$0`/`tokens=0`은 config validation(`nerve-config/src/lib.rs:216-221`)
     과 일관되게 거부, (c) 단위 명시 필수 — `$` 접두는 cost, `tokens` 접미는 tokens,
     단위 누락 시 거부, (d) 빈 값/공백 거부, (e) decimal cost(`$5.00`)는 microusd
     정수(`5_000_000`)로 변환 후 저장 — 변환 실패 시 `InvalidValue`. `nv doctor` 시작
     시 현재 세션 budget이 sane한지 1회 검사.
  6. **audit log hash-chain**: `.nerve/session-meta/budget-audit.json`은 append-only +
     각 entry가 직전 entry의 SHA-256을 `prev_hash` 필드로 포함하는 hash chain. `/budget`
     호출 시 직전 hash 검증 후 새 entry append. `nv doctor`가 chain integrity 검사 +
     깨졌으면 RED 경고. NvPatch 블랙리스트(§5 "코어 공통 NvPatch" 행)는 lead/reviewer
     경로 한정 보호이므로 외부 LLM CLI(claude/codex)가 cwd 안 파일을 자유 편집해
     audit row를 삭제·변조하는 경로는 chain으로 검출. Mode C에서 patrol이 sub-budget을
     위로 raising한 흔적 은폐도 동일하게 방어.
  7. **advisory lock + atomic write**: `/budget` 핸들러는 `.nerve/session-meta/budget.lock`
     advisory file lock(`fs2::FileExt::lock_exclusive`)으로 직렬화. lock 획득 후
     (read audit → validate raise/lower → atomic write audit + 세션 한도) 한 트랜잭션.
     `nerve-core/src/store.rs::write_json_atomic` 패턴(tempfile + rename(2)) 재사용해
     partial JSON write 방지. lock 5초 timeout 시 "다른 인스턴스가 budget 수정 중"
     경고. Mode C에서 Mayor/patrol은 동일 lock 공유.

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

#### 3j. Mayor / Patrol 멀티 인스턴스 (Gas Town 패턴)

- **영감**: Steve Yegge가 회자한 Gas Town 패턴(2차 정리 기준 2026-01,
  1차 출처 미확보 — §2.8 참조) — 20~30개 Claude Code 인스턴스를 Mayor
  에이전트가 조율, patrol 에이전트가 연속 루프, 상태는 git에 저장
- **Nerve 적용**:
  - `nv mayor` — 하나의 Mayor 프로세스가 작업 큐를 들고 있다가 `.nerve/queue/`
    의 task 파일이 들어오면 idle한 patrol 슬롯에 할당
  - `nv patrol --watch <glob>` — N개의 patrol이 각자 worktree에서 대기,
    Mayor가 dispatch한 task를 처리 후 결과를 `.nerve/results/` 에 기록
  - 상태 전부 git + `.nerve/` JSON — 크래시/재시작 견딤 (§0.5.5에서 정리한
    "상태를 git에 저장" 철학과 일치 — 원 귀속은 Yegge의 Gas Town 패턴)
- **선결 조건**: 2d(worktree), 2g(budget), 1b(`/goal`)
- **활용 시나리오**: PR 백로그를 patrol 풀이 동시에 갈아치우는 야간 배치
- **위험**: 동시 인증 토큰 소비량 폭증 → `/budget` global cap이 필수 안전망

---

## 4. v0.2.0 권장 묶음

**Tier 1 a + b + c + Tier 2g(`/budget`) 네 가지를 v0.2.0으로 묶을 것을 권장한다.**

- 1a 상태 바와 2g `/budget`은 데이터 출처가 동일(`RunReport.usage`)해서
  같은 사이클에 묶으면 비용 회계 관련 코드를 한 번에 정리할 수 있다.
- §0.5.3에서 정리한 3가지 정지 조건 중 max iteration과 budget cap **게이트**
  는 이미 코어에 있고(`max_refinement_rounds`, `exceeds_budget()`), 2g는
  그 budget 게이트를 사용자 표면(슬래시 명령 + 상태 바 게이지 + 종료 사유
  명시)으로 끌어올린다. no-progress는 1b에서 신규 추가된다. **이 묶음이
  들어가면 사용자가 §0.5.3 정리 기준 3가지 정지 조건을 모두 *보고 만질 수
  있는* 상태가 된다.**
- 인터랙티브 모드를 켰을 때 **즉시 체감되는 변화** 네 가지:
  1. 상시 노출되는 상태 바 (cost gauge 포함)
  2. `/goal`로 자동 종료/재시도 + no-progress 보호 (Tier 1b **Phase 1만**;
     Phase 2 자연어 등록은 v0.3.0 후속)
  3. `/budget`으로 비용 폭주 방지
  4. 검색 가능한 템플릿
- **선결 (1b 의존)**: 어댑터 timeout 가드(§5 위험표 첫 행) — `nv interactive`
  와 `/goal` 검증이 신뢰 가능해지려면 `tokio::time::timeout` 도입이 묶음 안에
  들어와야 한다.
- 예상 작업량: **4.5~6.5일 (라운드업 5~7일)** (1a/1c 각 0.5~1일 + 1b /goal
  2~3일 + 2g 1일 + adapter timeout 0.5일). 단일 PR로 묶기 적합하나, 1b가 가장
  큰 비중.
- §4.2 체크리스트에 'adapter hang 시뮬레이션 테스트(어댑터 무한 sleep 시
  5분 후 timeout 종료)' 항목 추가.

### 4.1 진입 파일·라인 요약 (Tier 1 + 2g 작업 시작점)

| 작업 | 진입점 |
|------|--------|
| 상태 바 구조체 | `crates/nerve-cli/src/main.rs:904` (`InteractiveLineEditor` 인접) |
| orchestrator → 상태 채널 | `crates/nerve-core/src/lib.rs` (`run_synaptic_loop`, line ~113) |
| `/goal` 슬래시 핸들러 | `crates/nerve-cli/src/main.rs` 슬래시 match 블록(약 1240–1393, `/quit` arm 위) |
| `GoalSpec` 타입 + no-progress 카운터 | `crates/nerve-config/src/lib.rs` |
| `Orchestration::check_env` 신규 필드 | `crates/nerve-config/src/lib.rs` (`Orchestration` 구조체, `#[serde(default)] check_env: Vec<String>` 신설) |
| `/budget` 슬래시 핸들러 | `crates/nerve-cli/src/main.rs` 슬래시 match 블록(약 1240–1393) |
| Budget 누적 체크 | `crates/nerve-core/src/lib.rs::run_synaptic_loop` 라운드 종료부 (`exceeds_budget()` line 392, 호출지 150/186/212/283/317에 wired) |
| `RoundRecord.patch_sha` 필드 (no-progress 감지) | `crates/nerve-types/src/lib.rs:158` (`RoundRecord`) |
| `GoalEvaluator` 실행기 + `CheckResult` enum | `crates/nerve-core/src/goal.rs` (신설), `crates/nerve-types/src/lib.rs` (`CheckResult` enum 신설) |
| `NvPatch::canonical_hash()` 헬퍼 (no-progress 정규화) | `crates/nerve-patch/src/lib.rs` (`canonical_hash` 헬퍼 신설) |
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
| Tier 1a 상태 바: 깜빡임 | raw TTY 모드와 다른 출력 간 깜빡임 | 출력 갱신 시 cursor save/restore (`\x1b[s` / `\x1b[u`) |
| Tier 1b /goal: 무한 루프 | 조건 평가가 무한 루프 유발 | `max_refinement_rounds` hard ceiling + no-progress 카운터 |
| Tier 1b /goal: check_cmd injection | `check_cmd` injection (임의 shell 실행 표면) | argv 배열 강제(`sh -c` 금지) + Phase 2 자연어 변환 사용자 확인 (v0.3.0 Phase 2 추가) + cwd freeze (sec-7, `/goal` 자동 재투입의 cwd 변경 보호) + env 화이트리스트 + `tokio::time::timeout` + `.nerve/goals/<id>.json` path traversal 검사 (§3 Tier 1b "보안 가드" 참조) |
| Tier 1b /goal: stdout DoS | `check_cmd` stdout DoS (`yes`/`dd if=/dev/zero`/무한 `echo` 등이 timeout 전 평가기 메모리 폭주, RPC 4 KiB truncate는 emit 시점이라 메모리 보호 안 됨) | 평가기 측 streaming 1 MiB cap (`tokio::process` + `Stdio::piped()` + `ChildStdout::take`) + 초과 시 `CheckResult::Fail` (§3 Tier 1b sec-1 7항 참조) |
| Tier 1b /goal: stdin leak | `check_cmd`가 부모 raw TTY stdin 점유 → 키스트로크 leak·echo 깨짐·SIGINT/SIGTSTP 전파 꼬임 | `Stdio::null()` 강제 + stdout/stderr `Stdio::piped()` 캡처 (§3 Tier 1b sec-1 8항 참조) |
| Tier 1b /goal: resource exhaustion | resource exhaustion (fork bomb `:(){ :\|:& };:`, OOM, fd 고갈) — argv 강제·timeout만으로 차단 불가 | v0.2.0은 `check_ulimit` config 옵션 + docs/security.md 권장 ulimit 예시, v1.0에서 cgroups v2 재검토 (§3 Tier 1b sec-1 9항 참조) |
| Tier 1b /goal: concurrent race | concurrent `/goal` 등록 race + `.nerve/goals/<id>.json` partial JSON write (Mode C는 §0.6.5 기준 v0.2.0 미지원) | `store.rs::write_json_atomic`(rename(2)) 패턴 재사용 + goal 변경은 다음 라운드 boundary에만 반영 (§3 Tier 1b ma-7 "swap 시점" 참조) |
| Tier 1b /goal: RoundRecord 호환 | `RoundRecord`에 `patch_sha` 추가 시 기존 세션 JSON 역직렬화 깨짐 | 필드를 `Option<String>` + `#[serde(default)]`로 도입 (§3 Tier 1b 본문 참조) |
| Tier 1b /goal: patch_sha 정규화 | `patch_sha` 비정규화 시 동일 의미 patch가 다른 hash 생성 → no-progress 가드 우회 | `NvPatch::canonical_hash()` 헬퍼 신설(path 정렬·LF 통일·메타 제외; §3 Tier 1b "정규화 규칙" 참조) |
| Tier 1b 선결 (v0.2.0 진입): 어댑터 timeout | `Command::output().await`(`nerve-adapter/src/lib.rs:208-213`)에 timeout 부재 — claude/codex 바이너리 hang 시 영구 대기, refinement loop 전체가 멈춤. **§4.2 체크리스트의 `nv interactive`/`/goal` 검증이 timeout 가드 없이는 신뢰 불가**(Tier 1b의 max iter·no-progress·budget cap 세 정지 조건도 adapter hang 위에서 무력화됨) | `tokio::time::timeout`으로 래핑 (기본 5분, `nerve.config.json::adapter.timeout_secs`로 오버라이드). 타임아웃 시 `AdapterError::Timeout` + 다음 라운드는 skip. **§4 v0.2.0 묶음에 명시 포함** |
| Tier 1b 선결 (v0.2.0 진입): 어댑터 응답 크기 cap | `Command::output().await`이 stdout/stderr 전체를 메모리 버퍼링 — 거대 응답(GB 단위 JSON dump) 시 timeout 5분 안에 OOM. timeout 가드만으로는 cap 미충족, Mode C에서 cascade OOM | `Command::output()` 대신 `spawn() + ChildStdout::take()` streaming 읽기, 누적 N MiB(기본 16 MiB, `nerve.config.json::adapter.max_output_bytes`) 초과 시 `AdapterError::OutputTooLarge` + child kill, doctor에서 cap 도달 metric 진단 (§3 Tier 2e sec-4 cross-ref) |
| Tier 1c 템플릿 검색: 동시성 | 사용 카운터 동시성 | `nerve-core/src/store.rs`의 atomic write 패턴 재사용 |
| 코어 공통 (NvPatch): 메타 디렉터리 쓰기 | LLM이 제안한 patch가 `.git/`, `.nerve/` 메타 디렉터리에 쓰기 (`crates/nerve-patch/src/lib.rs:938`의 비공개 헬퍼 `ensure_safe_relative_path` (`FileOperation::validate` 등 내부에서만 호출)는 traversal/abs path는 막지만 메타 디렉터리 블랙리스트 없음) | `NvPatch::validate`에 메타 디렉터리(`.git/`, `.nerve/`) 블랙리스트 추가. lead/reviewer 양쪽 patch 모두 동일 규칙 |
| 코어 공통 (Mode A 전반): suggested_patch 자동 승격 | Reviewer `suggested_patch`가 `ConflictPolicy::ReviewerPriority`/`MergeAttempt` 정책에서 `select_final_patch` (`nerve-core/src/lib.rs:573-580`)로 자동 승격, 1라운드부터 발생 — `/goal` 자동 재투입 무관 | `/goal` 자동 재투입 경로에서도 `suggested_patch`는 dry-run/diff 미리보기 강제 + `/apply` 명시 요구. `--apply` + `ReviewerPriority` 조합은 §0.6.6 "사람 in-the-loop" 원칙으로 거부하거나 인터랙티브 재확인 게이트 부착 |
| Tier 2d worktree: 버전 차이 | git 버전·플랫폼 차이 | `nv doctor`에 `git --version` 검사 추가 |
| Tier 2d worktree: 머지 트랜잭션 | 머지 도중 실패 시 main 트리에 partial-state 잔재, `git worktree remove` 실패 시 인증 캐시 누설 | main HEAD ref backup → 머지 실패 시 `git reset --hard`로 원복, 잔재 worktree는 `.nerve/scratch/orphaned-worktrees/`(0600)로 격리, doctor 고아 검사 (§3 Tier 2d "트랜잭션 보장" 참조) |
| Tier 2d worktree: symlink escape | symlink escape — lead 어댑터가 worktree 브랜치에 `escape -> ../../main/.git/config` 같은 symlink 커밋 후 머지 시 main `.git/config`·인증 토큰 경로 변조 | 머지 직전 `git diff --name-only main-pre.ref HEAD` + `symlink_metadata().is_symlink()` + `canonicalize()` prefix 검사, 발견 시 머지 거부+격리 (§3 Tier 2d sec-5 5항 참조) |
| Tier 2d worktree: disk full | disk full로 트랜잭션 abort 불가 — `main-pre.ref` 쓰기 ENOSPC 시 트랜잭션 시작도 못 함 | `statvfs(.nerve/)` 사전 검사(임계 100 MiB) + `ApplyError::DiskFull` 분리 + doctor 잔량 임계 검사 (§3 Tier 2d sec-5 6항 참조) |
| Tier 2d worktree: reset 실패 | `git reset --hard` 자체 실패(read-only fs/immutable flag/ACL/AppleDouble) 시 main partial state 잔재 | `git reset --merge` 폴백 → `git bundle create` 백업 → RED 경고 + `nv doctor --recover`, 잔재 worktree `mv` 실패 시 in-place chmod 0600 + manifest 기록 (§3 Tier 2d sec-5 7항 참조) |
| Tier 2e RPC: 외부 호환성 | 외부 컨슈머 호환성 | 이벤트에 `version` 필드 + unknown type ignore 가이드 |
| Tier 2e RPC: 누설 | `stdout_chunk`/`goal_check.output`이 prompt·시크릿·파일 경로 그대로 노출, multi-instance에서 컨슈머 격리 부재 | Unix socket 0600 (TCP 사용 시 토큰 인증) + raw 본문 opt-in + 시크릿 정규식 마스킹 + 경로 정규화 + Mode C patrol별 socket 격리 (§3 Tier 2e "보안" 참조) |
| Tier 2e RPC: DoS | slow consumer + 거대 payload buffering으로 daemon OOM, Mode C에서 patrol간 cascade hang | per-event hard cap 64 KiB(초과 시 head/tail 256B + truncated metadata) + per-consumer bounded channel 1024 events + oldest-drop + `dropped_count` metric (§3 Tier 2e sec-4 6항 참조) |
| Tier 2e RPC: schema migration | major schema migration 절차 부재 — `lead_agent` 필드 의미 변경/downgrade 호환 미정 | envelope `{schema_version, kind, payload}` semver 고정 — minor=필드 추가+ignore, major=handshake downgrade/거부, v0.2.0=v1.0 envelope 고정 (§3 Tier 2e sec-4 7항 참조) |
| Tier 2e RPC: 토큰 lifecycle | bearer 토큰 lifecycle 부재 — 발급/저장/회전/누설 대응 미정 → 장기 세션 토큰 누설 확대 | 32B 랜덤 + `.nerve/session-meta/rpc-token`(0600) + 데몬 종료 시 삭제 + `--print-token` opt-in + `nv rpc rotate-token` 수동, Mode C patrol별 분리 (§3 Tier 2e sec-4 8항 참조) |
| Tier 2e RPC: 마스킹 bypass | 토큰 분할/base64-hex 인코딩/신규 provider 패턴/JSONL log injection bypass | sliding-window(64자) 매칭 + Shannon entropy ≥ 4.5 휴리스틱 + `secret_patterns` config 확장(`sk-ant-`/`vrcl_`/`org-`/GCP SA JSON) + serde JSON string escape로 JSONL invariant 강제 (§3 Tier 2e sec-4 3항 확장 참조) |
| Tier 2f /plan: read-only 무시 | lead가 read-only 분석을 무시하고 patch 생성 시도 | prompt prefix("write a plan only")를 hard refuse하는 reviewer 룰 추가 + dry-run 강제 |
| Tier 2g /budget: cost 추정 | 부정확한 cost_microusd 추정 | adapter usage 파서에 fallback (`0`이면 token×요금표) + 경고 로그 |
| Tier 2g /budget: 권한 | 사용자가 `/budget cost=$10000`처럼 임의 raising → global cap 무력화, Mode C에서 patrol이 sub-budget 우회 | raising은 글로벌 ceiling 강제 + `--force` 시 인터랙티브 확인, Mode C patrol은 Mayor sub-budget 위로 raising 거부, `.nerve/session-meta/budget-audit.json`에 변경 감사 (§3 Tier 2g "권한 모델" 참조) |
| Tier 2g /budget: 입력 sanity | 음수/NaN/0/단위 누락/빈 값/decimal 변환 실패로 cap 우회 또는 즉시 종료 트랩 | 파서가 음수·NaN 거부 + `cost=$0`/`tokens=0` 거부(config 일관) + 단위(`$`/`tokens`) 명시 필수 + decimal microusd 변환 실패 시 `InvalidValue` + doctor 시작 시 sane 검사 (§3 Tier 2g sec-3 5항 참조) |
| Tier 2g /budget: 감사 위변조 | audit log 직접 편집/삭제/truncate로 raising 흔적 은폐 — 외부 LLM CLI(claude/codex)는 NvPatch 블랙리스트를 우회 | `.nerve/session-meta/budget-audit.json` append-only + `prev_hash` SHA-256 hash chain + doctor chain integrity 검사(깨졌으면 RED 경고) (§3 Tier 2g sec-3 6항 참조) |
| Tier 2g /budget: 동시성 | concurrent `/budget` race (lost update + audit/한도 drift + partial JSON) | `.nerve/session-meta/budget.lock` advisory lock 직렬화 + atomic write(`store.rs::write_json_atomic`) + lock 5초 timeout 시 경고, Mode C Mayor/patrol 공유 lock (§3 Tier 2g sec-3 7항 참조) |
| Tier 3g ratatui: 의존성 | 의존성 트리 증가 | feature gate (`--features tui`)로 옵션화 |
| Tier 3h fork/branch: 인덱스 충돌 | session fork 시 patch 인덱스 키 충돌 | `.nerve/sessions/<parent>/<child>.json` 트리 + patch index에 `session_id` 컬럼 추가 |
| Tier 3i MCP: 외부 도구 노출 | 외부 MCP 서버가 dangerous tool(`shell`/`fs`)을 reviewer에게 노출 | reviewer adapter는 read-only MCP whitelist만 통과 + `tool.allowed` 설정 |
| Tier 3j Mayor/Patrol: 토큰 폭주 | 다중 인증 토큰 폭주 | `/budget` global cap 필수 선결 + patrol당 sub-budget |

---

## 6. 관련 문서

- 아키텍처 — `nerve-architecture.md`
- 구현 계획 — `nerve-implementation-plan.md`
- 사용자 가이드 — `nerve-101.md`
- README — `README.md`

본 제안서는 위 네 문서와 별개로, **v0.2.0 UX 개선 한 사이클**에 집중한다.
