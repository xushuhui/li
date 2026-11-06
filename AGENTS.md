# li

High-performance AI coding agent CLI, written in Rust. Design decisions live in `specs/design.md`; deferred work lives in `specs/v2.md`.

**Hard constraint: the code never introduces `unsafe`.**

## Agent skills

### Issue tracker

Local markdown under `specs/scratch/<feature-slug>/`; no remote, no GitHub issues. See `specs/agents/issue-tracker.md`.

### Triage labels

The five canonical labels, unchanged from defaults. See `specs/agents/triage-labels.md`.

### Domain docs

Single-context: root `GLOSSARY.md` plus `specs/adr/`. See `specs/agents/domain.md`.
