# [시스템 설계서] Nerve: 시냅틱 AI 오케스트레이터

## 1. 프로젝트 개요
- **프로그램명:** Nerve (너브)
- **슬로건:** "The Reflexive Execution Layer"
- **목적:** 단일 지능의 편향성을 극복하기 위해 Claude Code와 Codex를 병렬로 운영하고, 상호 비판적 검토를 통해 고도화된 코드를 자동으로 산출하는 실행 중심의 AI 에이전트 시스템.

## 2. 핵심 철학: Collaborative Friction (협력적 마찰)
Nerve는 단순한 병렬 처리를 넘어, 두 모델 사이에 '의도적인 마찰(Review & Critique)'을 발생시킵니다. 감각 신호가 신경계(Nerve)를 통해 근육을 조율하듯, 사용자의 발화 하나로 여러 모델을 제어하여 최종 합의안에 도달하는 것을 목표로 합니다.

## 3. 시스템 아키텍처 (Core Architecture)
Nerve는 사용자 입력을 분산 처리하고 하나로 통합하는 'Synaptic Loop' 구조를 가집니다.

| 컴포넌트 | 설명 |
| :--- | :--- |
| **Nerve-Core** | Rust 기반의 고속 오케스트레이터. 입력을 분석하고 작업을 분배함. |
| **The Synapse** | 작업 진행 중 발생하는 중간 코드, 리뷰 의견, 상태를 저장하는 공유 메모리 버퍼. |
| **Lead Agent** | 실제 코드 구현 및 리팩토링을 담당하는 주 모델 (예: Claude Code). |
| **Reviewer Agent** | Lead의 결과물을 비판하고 엣지 케이스를 찾아내는 검증 모델 (예: Codex). |
| **Fusion Module** | 상호 리뷰 결과를 결합하여 최종 산출물을 확정하고 파일에 적용함. |

## 4. 데이터 명세: nerve.config.json
수동 설정 없이 자동화를 실현하기 위한 Nerve의 핵심 설정 파일입니다.

```json
{
  "orchestration": {
    "default_strategy": "consensus",
    "max_refinement_rounds": 2,
    "conflict_policy": "lead_priority"
  },
  "roles": {
    "architect": "claude-code",
    "reviewer": "codex"
  },
  "profiles": [
    {
      "id": "blockchain_dev",
      "match_rules": ["*.rs", "*.sol", "contract"],
      "lead": "claude-code",
      "reviewer": "codex",
      "review_strictness": "high"
    },
    {
      "id": "rapid_fix",
      "match_rules": ["fix", "ui"],
      "lead": "codex",
      "reviewer": "claude-code"
    }
  ]
}
```

5. 운영 워크플로우 (Operational Flow)
	1	Signal Dispatch: 사용자가 CLI에서 명령을 입력합니다. (예: nv "auth 모듈 개선")
	2	Parallel Execution: Lead 모델은 구현을 시작하고, Reviewer 모델은 실시간으로 작성되는 코드를 감시하며 보안 및 성능 이슈를 계산합니다.
	3	Cross-Firing: Reviewer가 발견한 피드백을 Lead에게 전송하며 수정을 요구합니다.
	4	Synaptic Fusion: 합의된 최종 코드가 파일 시스템에 적용되고, 결과가 사용자에게 보고됩니다.

6. CLI 및 UX 설계 (Ghostty + cmux)
Nerve는 고성능 CLI 환경인 Ghostty와 cmux를 기반으로 가시성을 극대화합니다.
	•	cmux 자동 레이아웃: nv 실행 시 패널을 3분할하여 Lead의 사고 과정, Reviewer의 피드백, 전체 상태 로그를 실시간 스트리밍합니다.
	•	Atomic Patch: 모든 결과물은 nv-patch 단위로 관리되어 언제든 안전하게 롤백이 가능합니다.

7. 기술 구현 가이드라인
	•	개발 언어: 시스템의 속도와 안정성을 위해 Rust를 주 언어로 사용합니다.
	•	비동기 처리: tokio를 활용하여 여러 AI 모델의 API 호출 및 파일 감시(fswatch)를 병렬로 처리합니다.
	•	모델 인터페이스: async-trait를 통해 모델을 추상화하여, 새로운 모델이 출시되어도 설정 파일 수정만으로 교체 가능하게 설계합니다.


---

### 💡 팁: 터미널에서 바로 파일 만들기
고스티(Ghostty) 터미널을 사용 중이시라면, 아래 명령어를 복사해서 붙여넣으시면 바로 `nerve_design.md` 파일이 생성됩니다.

```bash
cat <<EOF > nerve_design.md
# 위에 제공된 마크다운 내용을 여기에 붙여넣으세요.
EOF
