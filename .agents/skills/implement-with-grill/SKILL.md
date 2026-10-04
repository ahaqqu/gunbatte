---
name: implement-with-grill
description: The full task loop for implementing or fixing anything in this repo — grill the design, work in a worktree, implement, prove it locally, open a PR, then continue through review-code and resolve-review-findings to merge, deploy, verify, and clean up. Use when the user asks to implement or fix something, or for any feature/bugfix task in this repo.
---

# The task loop: grill → worktree → implement → PR → review code → resolve review code → merge & deploy → verify → clean up

AGENTS.md carries the rules every role shares (reliability outranks
policing, ask through the harness question tool, the architecture
boundaries); this skill owns the loop that turns a request into a merged,
deployed, verified change. Run the full loop for your span; don't stop
halfway and don't skip stages unless the user explicitly waves one off.
The loop is split across three agents, each owning its stages end to end,
with the PR and its review comments as the handoff between them:

- **First agent** — grill → worktree → implement → PR (steps 1–5).
- **Second agent** — review code (step 6).
- **Third agent** — resolve review code → merge & deploy → verify → clean
  up (steps 7–10).

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
   its checks, and hand it to the review agent only when GitHub reports it
   genuinely ready: `gh pr view --json mergeable,mergeStateStatus` says
   `MERGEABLE` / `CLEAN` and every check is green (a docs/meta-only PR runs
   no checks at all — mergeability is then the whole gate). That is the
   first agent's last stage.
6. **Review the code (review-code skill).** A second agent — fresh eyes,
   never the implementation agent — runs the review-code skill on the PR:
   an adversarial security-expert audit for bugs, regressions,
   vulnerabilities, and code quality (here that includes the seam rules,
   database-as-identity-arbiter, and determinism claims). Every finding
   goes to GitHub as one inline review comment — stable ID, priority,
   evidence, recommended fix — followed by one consolidated summary comment
   indexing them. The reviewer posts findings only; it does not fix or
   push. Its stage ends when the findings are on the record.
7. **Resolve the review findings (resolve-review-findings skill).** A third
   agent picks up from the review comments: disposition every finding with
   a threaded accept/reject reply on the original comment, fix the accepted
   ones in the same worktree, re-run the step-4 gates for anything the
   fixes touch, push to the same branch, post the resolution report on the
   PR, and iterate until CI is green. Rejections cite evidence; nothing
   stays silent.
8. **Merge, then deploy is part of the task.** With every finding
   dispositioned and GitHub reporting the PR genuinely ready (the same gate
   as step 5), merge, confirm CI is green on main, then watch the `deploy`
   workflow (it fires on CI success via `workflow_run`) until it succeeds —
   it runs `provision/vps/deploy.sh`: build, rsync, restart
   `gunbatte.service`. Docs/meta-only merges (the `paths-ignore` list in
   ci.yml) start no CI run and so fire no deploy — verify by absence in the
   Actions tab instead.
9. **Verify the work on the live URL** ([play.gunbatte.ahaqqu.com]). CI
   rebuilds on its own toolchain, so asset hashes differ from any local
   build — verify by content (does the served page/bundle contain the
   change?) and by playing the actual flow the PR was about. Report what was
   verified and what couldn't be (e.g. audibility in a headless browser).
10. **After merge: clean up the worktree.** The third agent owns the
    teardown; the user never does it. Once the PR is merged (or closed
    without merging), remove the worktree and the branch, then prune:

    ```sh
    git worktree remove ../gunbatte-<task-slug>
    git branch -d <branch>
    git fetch --prune
    ```
