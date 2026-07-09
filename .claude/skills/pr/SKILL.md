---
name: pr
description: Create a feature branch, commit changes, push, and open a PR against iopsystems/metriken
---

Create a PR from the current uncommitted changes. Handles branching, committing, pushing, and opening the PR against `iopsystems/metriken`.

metriken is a **multi-crate Cargo workspace** (`metriken`, `metriken-core`, `metriken-derive`, `metriken-exposition`, `metriken-query`). Feature PRs do **not** bump versions — versions are bumped per-crate at release time (see the `release` skill). Add changelog entries under `## Unreleased` instead.

## Arguments

Optional branch name:
- If provided, use it (e.g. `/pr fix-quantile-interp`).
- If not, generate a descriptive kebab-case branch name from the changes.

## Steps

1. **Verify prerequisites**:
   ```bash
   git status
   git diff
   git diff --staged
   ```
   If there are no changes, stop and tell the user. Don't reuse a feature branch that already has unpushed commits for a *different* change.

2. **Analyze the changes** and match existing conventions:
   ```bash
   git log --oneline -10
   ```
   Commit style is conventional: `type(scope): description` — e.g. `feat(query): …`, `fix(query): …`, `chore: …`. The scope is usually the crate area (`query`, `tsdb`, `exposition`).

3. **Create a feature branch** (if on `main`):
   ```bash
   git checkout -b <branch-name>
   ```
   If already on a suitable feature branch, use it.

4. **Update CHANGELOG.md** if the change is user-facing:
   - Add a bullet under `## Unreleased`, inside the affected crate's
     `### <crate> <next-version>` sub-section (create the sub-section if
     absent). Match the existing per-crate grouping.

5. **Run checks before committing** (if not already run this session):
   ```bash
   cargo clippy -p <affected-crate> --all-features -- -D warnings
   cargo test -p <affected-crate> --all-features
   cargo fmt -p <affected-crate> -- --check
   ```
   Note: `cargo clippy --all-features` across the whole workspace currently trips a pre-existing deprecated `histogram::percentiles` warning in `metriken-exposition` — scope clippy to the crate you changed.

6. **Stage and commit**:
   - Stage changed files by name (avoid `git add -A`).
   - Conventional-commit message via HEREDOC.
   - Trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

7. **Push to origin**:
   ```bash
   git push -u origin <branch-name>
   ```

8. **Open the PR**. `origin` is `iopsystems/metriken` (confirm with `git remote -v`), so this is a same-repo PR (`--head <branch-name>`). If `origin` is instead a personal fork, use `--head <fork-owner>:<branch-name>` and `--repo iopsystems/metriken`.
   ```bash
   gh pr create \
     --repo iopsystems/metriken \
     --head <branch-name> \
     --draft \
     --title "<conventional-commit-style title>" \
     --body "$(cat <<'EOF'
   ## Summary
   <1-3 bullets>

   ## Test plan
   <checklist of testing done or needed>

   🤖 Generated with [Claude Code](https://claude.com/claude-code)
   EOF
   )"
   ```

9. **Report the PR URL** to the user.

## Notes

- Keep PR titles under 70 chars, same conventional-commit format as the commit.
- Never force-push or amend existing commits.
- PRs are drafts by default; mark ready with `gh pr ready` or the GitHub UI.
- Do **not** bump crate versions in a feature PR — that's the `release` skill's job.
