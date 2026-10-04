---
name: implement-with-grill
description: The first agent's span of the task loop — grill the design, work in a worktree, implement, prove it locally, and open a merge-ready PR, then hand off to the review-code skill. Use when the user asks to implement or fix something, or for any feature/bugfix task in this repo.
---

# Implement with grill (first agent: grill → worktree → implement → PR)

The task loop runs as three agent roles, each ending with a handoff that
names the next stage: this skill is the first span — grill → worktree →
implement → PR — and hands off the merge-ready PR to review-code;
review-code hands off the findings summary to resolve-review-findings;
resolve-review-findings hands off the resolution summary and, on the
user's go, merges, deploys, verifies, and cleans up. Run your span end
to end; don't stop halfway and don't skip stages unless the user
explicitly waves one off. AGENTS.md carries the rules every role shares
(reliability outranks policing, ask through the harness question tool,
the architecture boundaries).

1. **Grill first (grill-me / the grilling skill).** Before writing any code,
   load the grilling skill and work the design tree in rounds: fact-find in
   the repo until the frontier is real decisions only, then present each
   round — numbered questions, concrete options, recommendation first — via
   the harness question tool, and wait. Small mechanical bug fixes with an
   unambiguous cause are exempt; anything with a design choice is not.
2. **Confirm, then implement.** When the frontier is empty, restate the
   agreed plan in one block and get an explicit go — then implement.
3. **Work in a dedicated worktree off latest `main`.** Never build on
   whatever branch the primary checkout happens to sit on. Fetch, then
   create one worktree per task, branched from fresh `origin/main` —
   several agents (or one agent on several tasks) can then work at the
   same time without colliding, and the primary checkout stays untouched:

   ```sh
   git fetch origin
   git worktree add ../gunbatte-<task-slug> -b <branch> origin/main
   ```

   Every edit, build, and test run for the task happens inside that
   worktree. Its first build is cold (fresh `target/` and `node_modules`) —
   that is the price of isolation, not something to route around.

4. **Prove it locally before it leaves the machine.** `make test` (both
   profiles) and `make ci` for Rust; `npm run build` (tsc + vite) for the
   viewer; for anything the player sees or hears, drive the real client in a
   real browser against a local server (`./start-server.sh`) and verify on
   screen — screenshots as evidence, console clean of errors.
5. **PR, then make it merge-ready before handing it over.** Keep the branch
   current with `main`: `git fetch origin && git rebase origin/main` right
   before opening the PR, and again before handing it over whenever `main`
   has moved since (`git push --force-with-lease` when the PR already
   exists; re-run step 4 if the rebase changed anything). Open the PR, watch
   its checks, and hand it over only when GitHub reports it genuinely
   ready: `gh pr view --json mergeable,mergeStateStatus` says `MERGEABLE` /
   `CLEAN` and every check is green (a docs/meta-only PR runs no checks at
   all — mergeability is then the whole gate).

## Handoff: the PR

When step 5's gate passes, stop. The handoff is the PR itself: report
the PR link, what it contains, and the readiness gate
(`gh pr view --json mergeable,mergeStateStatus` says `MERGEABLE` /
`CLEAN`; checks green — or no checks for a docs/meta-only PR, where
mergeability is the whole gate). Then suggest the next step and end the
run: the review-code skill reviews this PR as a separate agent run —
fresh eyes, never this agent.
