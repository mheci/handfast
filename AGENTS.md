# AGENTS.md — handfast

> Inherits global workspace policy from C:\Users\me\oc\AGENTS.md (%USERPROFILE%\oc\AGENTS.md) — primary workspace %USERPROFILE%\oc + %USERPROFILE%\Projects. Never push without explicit permission. Never commit without showing diff + approval.

## What this repo is
Wayland-first KDE Connect daemon, 9-crate Rust workspace, crates/ipc/codec.rs, wayland portals

## pstack Skills (merged 2026-08-29)

Audit: C:\Users\me\oc\pstack-audit-mheci-2026-08-29.md (45 skills) + map C:\Users\me\oc\skill-map.json. Local copies at .opencode/skills/<name>/SKILL.md and .cursor/skills/<name>/SKILL.md. Global unslop+bro at C:\Users\me\oc\.opencode\skills\.

**Always:** how -> architect (if shape unclear) -> blast-radius -> implement -> prove-it-works

| Skill | When to use here |
|---|---|
| architect | Sketch types/signatures/module structure before code |
| arena | Fan out N parallel candidates, pick base, graft best parts |
| blast-radius | Find what change could break beyond diff, prove safety by running code |
| how | Code walkthrough before change - architecture, ownership, placement |
| tdd | Failing test first |
| swarm | Fan out N parallel workers for coverage |
| interrogate | Adversarial multi-model review |
| principle-boundary-discipline | Validate at boundaries, trust types inside |
| principle-build-the-lever | Build codemod/script/generator |
| principle-encode-lessons-in-structure | Lint/flag > doc note |
| principle-foundational-thinking | Data structures first, scaffold before features |
| principle-fix-root-causes | Reproduce, why, fix root |
| principle-guard-the-context-window | Route bulk to subagents |
| principle-make-operations-idempotent | Converge on rerun/crash halfway |
| principle-migrate-callers-then-delete-legacy-apis | Migrate all callers + delete in same wave |
| principle-model-the-domain | State machine / discriminated union, not scattered if |
| principle-sequence-verifiable-units | Verify each red-green unit before next |
| principle-type-system-discipline | Illegal states unrepresentable |
| unslop | Cut 31 AI tells, add soul |
| bro | Plain human, no jargon |

---
Host safety: No mods outside %USERPROFILE%\oc or %USERPROFILE%\Projects unless Windows/external tool requires. No push without permission; no commit without diff+approval.

