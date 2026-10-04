---
name: fixer
description: Resolve every itemized review finding on a GitHub PR — accept or reject each with a threaded reply on the original comment, fix the accepted ones, keep CI green, and post a resolution report on the PR. The closing half of the code-review loop.
---

# Fixer (resolve PR review findings)

Take a PR that carries itemized review findings (the code-review format: one
comment per finding with a stable ID and a priority) and drive every finding
to an explicit, posted disposition. Nothing stays silent: every item ends
with an accept or a reject on the record, accepted items end in a fixing
commit, and the PR ends green.

## The loop

1. **Read everything first.** The PR description, the diff, and every
   itemized review comment. Understand each finding before dispositioning
   any — findings can interact.
2. **Disposition every item as a threaded reply on the original review
   comment**, via
   `gh api repos/{owner}/{repo}/pulls/<pr>/comments/<comment_id>/replies -f body=…`.
   The reply body is accept or reject plus one-sentence reasoning. A reply
   anywhere else does not count. For a PR-level (issue-comment) finding,
   post the disposition as a standalone issue comment referencing the
   finding ID — GitHub has no threaded-reply route for issue comments.
3. **Apply fixes for every accepted item.** Do not weaken an assertion or
   restructure code just to silence a finding without addressing its root
   cause.
4. **Run the full local CI gate set after fixes** — `make test` (both
   profiles) and `make ci` for Rust, `npm run build` for the viewer, and the
   real-browser pass for anything the player sees or hears (AGENTS.md
   step 4).
5. **Push fixes to the same branch**, then post the resolution report as a
   PR comment listing each item ID, its disposition, the threaded reply
   comment ID, and the fixing commit SHA (for accepted items). Post it
   before watching CI — the report is the loop's last artifact and therefore
   the one an interrupted session most often loses — then update it in place
   (`gh api -X PATCH repos/{owner}/{repo}/issues/comments/<comment_id>
   --input <json-payload-file>`) once checks settle, with the final head SHA
   and check status. Normalise the body of every `--input` payload this step
   sends per the pr-creation skill's body-normalisation rule (canonical
   there; not restated here).
6. **Keep CI green; iterate on red** until `gh pr checks <pr>` is green for
   the head commit.

## Non-negotiable rules

- **Reject with evidence.** If you reject a High-priority item, your reply
  must cite a concrete file:line mechanism. If you need fact-finding,
  escalate to the manager with the specific evidence you need verified.
  "I disagree" is not enough.
- **Never hide rejected items.** Post the rejection as a threaded reply on
  the original comment, just like acceptances.

## Where the work happens

- Work in the same worktree as the original implementation; if that worktree
  no longer exists, recreate one per AGENTS.md (`git fetch origin && git
  worktree add ../gunbatte-<task-slug> -b <branch> origin/main`) and check
  out the implementation branch into it — fixes build on the original work,
  never on a fresh start.
- If the implementation branch has no open PR yet, create one before
  starting the loop: dispositions and the resolution report are PR
  artifacts.
- Do not merge. Merging is the user's call; the loop ends at a green,
  merge-ready PR.
