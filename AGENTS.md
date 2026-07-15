## Language Requirements
- All code comments MUST be in English only
- All logging messages MUST be in English only
- All error messages MUST be in English only
- All docstrings MUST be in English only
- All variable names, function names, and class names MUST be in English only
- All git commit messages MUST be in English only
- Project documentation (README.md, files in docs/) MUST be in English only

## Git Workflow
Commit straight to `main`. Do **not** create a branch unless explicitly asked:
this project keeps a linear history with no branches, pull requests, or CI, and
releases are cut from `main` (see `docs/adr/0007`). This overrides any default
habit of branching before committing.

Committing and pushing still happen only when asked.

## Code Guidelines
Follow the behavioral rules in @CODE_GUIDELINES.md

## Architecture Decision Records
Significant architectural or technical decisions are recorded as ADRs in
`docs/adr/`. See `docs/adr/README.md` for conventions and the template. When
making a decision that affects structure, tooling, or long-lived tradeoffs, add
a new ADR instead of relying on commit messages or chat history.
