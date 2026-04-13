# AGENTS.md — RollBall.AI

This is a **design and research repository** for RollBall.AI — an AI agent platform architecture. The implementation is not ready yet, because the design documents are still being discussed. 'zeroclaw' is a reference implementation only, and is not the source of truth for RollBall.AI design.

## Repository Contents

- `docs/` — RollBall.AI architecture design documents (in Chinese)
- `agent-as-app-design-doc.md` — design doc index
- `ref-doc/` — reference documentation (zeroclaw learning materials, agent memory research)
- `zeroclaw/` — **reference implementation only** (do not edit; it is a separate project)

## Core Architecture

RollBall.AI is an agent platform modeled after Android:

| Android | RollBall | Role |
|---------|----------|------|
| APK | `.agent` package | Declarative agent bundle (ZIP: manifest, prompts, skills, no binary) |
| ART | Agent Runtime | Universal binary that loads and executes `.agent` packages |
| AMS | Gateway | Lifecycle manager (install, start, stop, budget, rate limit) |
| Key Vault | Key Vault | Secure API key storage and one-time分发 |
| ContentProvider | System Agent | System-level data services (identity, preferences) |

## Key Documents

- `docs/01-overview.md` — platform overview, Android comparison, core principles
- `docs/02-agent-package.md` — `.agent` package format, signing mechanism
- `docs/03-agent-runtime.md` — agent runtime internals and main loop
- `docs/04-gateway.md` — gateway components (PackageManager, Lifecycle, IntentRouter, Vault, Budget, Rate)
- `docs/05-memory.md` — layered memory: private Grafeo + system Agent + cloud sync
- `docs/06-communication.md` — Gateway Service API + Intent mechanism
- `docs/07-system-agent.md` — system Agent design
- `docs/08-security.md` — security design (isolation, signing, permissions)
- `docs/09-roadmap-and-scenarios.md` — 6-phase implementation roadmap
- `docs/10-dev-framework.md` — DevMode, Debug Protocol, recording/playback, Agent cloning/publishing

## Important Conventions

- All design docs are in **Chinese** (中文)
- Agent packages are **declarative only** — no executable code in `.agent` bundles
- ZeroClaw is a **reference implementation**, not the source of truth for RollBall design
- Documents use version v3.x — do not mix terminology from older versions
- Code reviews should follow [.opencode/style-guide.md](./.opencode/style-guide.md) — Rust security best practices, memory safety, and ZeroClaw project standards
