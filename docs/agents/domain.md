# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

This repo keeps no domain docs of its own. The Companion's glossary, decisions and spec live in pagle-backend.
Read them from the sibling clone at `../pagle-backend` (pull it first). Without a clone, use `gh api repos/timemir/pagle-backend/contents/<path> --jq .content | base64 -d`.

- `GLOSSARY.md`
- `docs/adr/`: read the ADRs that touch the area you're working in (for example 0005, "the Companion stays outside the game").
- `docs/spec/`: canonical. Start at `docs/spec/README.md`. The Companion's own area is `docs/spec/companion/`. Cite rule IDs (`DST-R56`, `CMP-R50`) instead of restating them.

New terms and ADRs that `/domain-modeling` resolves go into pagle-backend, not here.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0005 (the Companion stays outside the game), but worth reopening because…_
