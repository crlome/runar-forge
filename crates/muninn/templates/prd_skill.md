<!-- managed by runar setup; edits will be overwritten. Remove this line to take ownership of the file. -->
---
name: prd
description: Turn a feature request into a product requirements document through a structured interview, then store it as a plan in runar and/or a markdown file. Use when the user says "write a PRD", "plan this feature", "spec this out", "let's design X before building it", or asks to turn an icebox item into real work.
---

# /prd — requirements interview, then a stored plan

Your job is **not** to write down what the user asked for. It is to find out
what they actually need, disagree where the evidence says otherwise, and end
with a plan whose phases can each be executed and verified independently.

A plan written from the first sentence of a request is the most expensive
document in the repository: it looks like agreement and encodes a
misunderstanding.

## 1. Ground yourself before asking anything

Run these first, and say what you found:

- `muninn_search` for the feature area — there may already be a decision,
  a rejected approach, or a bug that explains why the obvious design is
  wrong.
- `muninn_plan_list` — a plan for this may already exist. If one does and it
  is not `closed`, ask whether to continue it rather than start a second.
- `muninn_icebox_list` — the request may already be filed.
- Read the code the change touches. Name real files.

Never ask a question the codebase or memory already answers.

## 2. Interview

Ask in small batches — no more than four questions at a time — and stop when
you can state the requirements back and the user agrees. Cover:

**What** — the change in one sentence. The explicit non-goals. What stays
exactly as it is. Which existing surface it modifies versus adds to.

**Why** — the problem behind the request, who has it, and how you will know
it is solved. If the user describes a solution, ask what problem it solves;
if the honest answer is "none we have", say so.

**How** — constraints that already exist: data that must sync, backward
compatibility, migrations, performance budgets, security boundaries.
Existing patterns in the repo the change should follow rather than reinvent.

**What could go wrong** — the failure mode nobody would notice. Prefer the
ones this codebase has actually had: silent truncation, a guard that reads
as stricter and is not, a set-difference over a set that is not complete, a
counter that measures something other than what its name says.

If you disagree with the approach, say so once, plainly, with the reason.
If the user reaffirms it, that is their decision — build it and record the
concern in the Risks section.

## 3. Draft

Structure the plan as:

```markdown
# <Title>

<Problem statement and the outcome, in a few sentences.>

## Context
## Goals and non-goals
## Design
## Phase 1 — <name>
## Phase 2 — <name>
## Risks
## Verification
```

Rules that matter:

- **Only headings that name a phase become tracked work.** `## Phase 2 —
  storage` is executable and tracked; `## Design` is not. This is
  deliberate — nothing is silently promoted into work.
- **Split anything longer than one session into phases.** Each phase must be
  independently shippable and independently verifiable. This is what makes
  progress survive a restart.
- Every phase names the files it touches and how to tell it worked.

## 4. Ask where it goes

Ask explicitly — this is the user's call, and multiple answers are normal:

1. **A markdown file** in the repo (`plans/<slug>.md` by convention). Best
   for editing by hand and reviewing in a diff.
2. **runar** via `muninn_plan_create`. Recallable across sessions, survives
   a restart, and syncs to the team remote.
3. **Both** — the file is where it gets edited, runar is where execution is
   tracked. If the file later changes, re-run `muninn_plan_create` with the
   same slug: it replaces the sections and retires the ones that are gone.
4. **Execute it now** — still store it first, or a restart loses everything.

Pass the whole document as `markdown` to `muninn_plan_create`; it splits the
sections and detects phases. If a section is longer than the entry cap it is
split across parts, never truncated.

If this plan came from an icebox item, close the loop:
`muninn_icebox_set_status(slug, status: "promoted", promotedTo: <plan slug>)`.

## 5. Execution protocol

When executing a plan — in this session or a later one:

1. `muninn_plan_get(slug)` first. **If `closed` is true, do not execute it.**
   It is finished or abandoned; ask the user what they actually want.
2. Skip every phase whose `phaseStatus` is `done`. Never redo completed work.
3. At the start of a phase:
   `muninn_plan_set_status(slug, phase: N, phaseStatus: "in-progress")`.
4. Do the work. Verify it — run the tests, run the thing.
5. Only after it verifies:
   `muninn_plan_set_status(slug, phase: N, phaseStatus: "done")`.
6. When every phase is done, **ask the user** before setting the plan to
   `completed`. Do not close it yourself.

Marking a phase done before verifying it is the one failure this protocol
exists to prevent: it turns a resumed session into a session that skips
broken work.
