---
name: release
description: Release a single metriken workspace crate — bump its version, update the changelog, tag, and (gated) publish to crates.io
---

metriken is a **multi-crate workspace** with **independently versioned and
tagged** crates (`metriken`, `metriken-core`, `metriken-derive`,
`metriken-exposition`, `metriken-query`). There is **no release automation**
(CI is build/test only) — releases are manual, per-crate. Tags follow
`<crate>-v<version>` (e.g. `metriken-query-v0.11.1`).

This skill releases **one crate**. To release several, run it once per crate,
lowest in the dependency graph first (see "Inter-crate deps" below).

## Arguments

`/release <crate> <level>` — e.g. `/release metriken-query patch`.
- `<crate>`: one of the workspace members.
- `<level>`: `patch` | `minor` | `major`, or an explicit version `X.Y.Z`.

If either is missing, ask.

## Steps

1. **Prerequisites**:
   - On `main`, clean working tree, up to date with `origin/main`.
   ```bash
   git fetch origin
   git branch --show-current            # must be main
   git status --porcelain               # must be empty
   git rev-parse HEAD; git rev-parse origin/main   # must match
   ```

2. **Checks** for the crate being released:
   ```bash
   cargo clippy -p <crate> --all-features -- -D warnings
   cargo test  -p <crate> --all-features
   cargo fmt   -p <crate> -- --check
   ```
   (Whole-workspace `--all-features` clippy trips a pre-existing
   `histogram::percentiles` deprecation in `metriken-exposition`; scope to
   the crate.)

3. **Determine the new version** (cargo-release computes it; strips any
   pre-release suffix):
   ```bash
   cargo release version <level> -p <crate> --dry-run
   ```

4. **Inter-crate deps.** If the crate is depended on by others in the
   workspace (e.g. `metriken-exposition` ← `metriken-query`), releasing it
   may require bumping the dependents' required version. Check:
   ```bash
   grep -rn "<crate>" */Cargo.toml
   ```
   Release dependencies **before** dependents.

5. **Create a release branch** (metriken uses PRs; don't push to main directly):
   ```bash
   git checkout -b release/<crate>-v<new-version>
   ```

6. **Bump the version**:
   ```bash
   cargo release version <level> -p <crate> --execute --no-confirm
   ```
   Sync `Cargo.lock`:
   ```bash
   cargo update -p <crate> --precise <new-version>
   ```

7. **Update CHANGELOG.md**: move the crate's `### <crate> <new-version>`
   block out of `## Unreleased` into a dated released section, and leave a
   fresh empty `## Unreleased` for that crate. Ask the user to review the
   changelog before continuing.

8. **Commit + push + PR**:
   ```bash
   git add -A
   git commit -m "release: <crate> v<new-version>"
   git push -u origin release/<crate>-v<new-version>
   gh pr create --repo iopsystems/metriken --head release/<crate>-v<new-version> \
     --title "release: <crate> v<new-version>" --body "…"
   ```
   Include the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer on the commit.

9. **After the PR merges to `main`** — tag and publish. **There is no
   automation; these steps are manual and `cargo publish` is IRREVERSIBLE.**
   - Update local `main`, then tag:
     ```bash
     git checkout main && git pull origin main
     git tag <crate>-v<new-version>
     git push origin <crate>-v<new-version>
     ```
   - **STOP. Confirm with the user before publishing** — a crates.io publish
     cannot be undone (only yanked). Then:
     ```bash
     cargo publish -p <crate>
     ```

## Notes

- Tag format is exactly `<crate>-v<version>` — match the existing tags
  (`git tag --sort=-creatordate`).
- Never force-push or amend.
- If `cargo-release` isn't installed: `cargo install cargo-release`.
- Publishing order matters: a dependent crate won't publish until the
  dependency version it requires is already on crates.io.
