---
name: code-review
description: Security-expert deep review of a GitHub PR — bugs, regressions, vulnerabilities, code quality. Posts one inline review comment per finding plus a consolidated summary comment.
---

# Code review (GitHub PR)

Perform a comprehensive, adversarial review of a PR as a security expert and
quality auditor. Be EXTREMELY thorough, rigorous, careful, and attentive —
NOTHING slips through. Nothing can slip through.

## What to audit

1. **Correctness** — real bugs: logic errors, off-by-one, race conditions,
   unhandled error paths, wrong state transitions, integer/overflow issues.
2. **Regressions** — changes that break existing features/functionality,
   including behavior changes hidden inside refactors and contract changes
   tests silently absorb.
3. **Security vulnerabilities** — injection, auth bypass, identity confusion,
   DoS (per-request or unbounded memory/growth), information disclosure,
   trust-boundary violations. For this repo also: the matchmaker ≠ game-server
   seam rules in AGENTS.md, database-as-identity-arbiter, determinism claims.
4. **Code quality** — restructure/implement meaningfully better without
   changing behavior: abstractions, modularity, duplication, spaghetti,
   succinctness, legibility. Be ambitious when there is a clear path; measure
   twice, cut once.

## Method

- Read the full diff (`gh pr diff <n>`) and the PR description; understand the
  intent before judging.
- For every changed hunk, read the surrounding code in the checked-out branch
  (worktree or primary checkout) — hunks lie without context.
- Trace behavior end-to-end for anything user-visible or protocol-visible:
  who can trigger it, what state changes, what the failure path does.
- Facts only: every finding cites concrete code (file:line or a behavior
  trace). No speculation; verify each claim against the actual code.
- Check the tests: does a new test actually pin the fix? Is any changed
  behavior now untested or test-weakened?

## Deliverables (all findings go to GitHub)

Post **one GitHub review comment per item** on the PR (inline where a changed
line anchors it). Every item has:

- **[ID]** — stable identifier (R1, R2, …) so users can instruct fixes by ID
  without ambiguity.
- **Priority** — High / Medium / Low.
- **file:line anchor** — when the changed line is identifiable; otherwise
  file + region.
- **Evidence** — concrete code reference or behavior trace, no speculation.
- **Recommended fix** — concrete and actionable.

Then post **one summary comment** consolidating all findings, containing an
item index table for every posted item:

| ID | Priority | File | Short title |
|----|----------|------|-------------|

Order findings by priority, then file. Link each table row to its inline
comment. State at the top what was reviewed (PR, base/head SHAs, scope) and
the overall verdict.

## Posting mechanics

- Fetch head commit SHA: `gh pr view <n> --json headRefOid`.
- Inline comments: `POST /repos/{owner}/{repo}/pulls/{n}/comments` (or a
  single `POST …/pulls/{n}/reviews` with `comments[]`, event `COMMENT`) with
  `commit_id`, `path`, `line` (and `start_line`/`start_side` for ranges) —
  anchor lines must be part of the diff.
- Summary: `POST /repos/{owner}/{repo}/issues/{n}/comments` after the inline
  comments exist, so it can link them.
- If there are no findings, say so explicitly in the summary comment instead
  of posting empty items.
