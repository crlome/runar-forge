---
name: icebox
description: Capture a deferred idea, backlog item or piece of future work into the project icebox, and list or promote what is already filed. Use when the user says "file this", "add to the icebox", "let's do this later", "park that", "what's on the backlog", or asks what to work on next.
---

<!-- managed by runar setup; edits will be overwritten. Remove this line to take ownership of the file. -->

# /icebox — capture now, decide later

An idea mentioned mid-task and not written down is gone. This skill exists
to make filing one cost two exchanges, not twenty.

## Capturing

Be fast. Ask at most one clarifying question, and only if the item would be
meaningless without it.

1. **Title** — one line, stating the thing itself. Prefer the finding over
   the feature: "the code graph answers confidently from a stale index"
   beats "add graph auto-refresh".
2. **Why it matters** — a sentence or two, plus anything already known that
   the future reader would otherwise have to rediscover: the measurement,
   the file, the constraint that rules out the obvious fix.

Then ask where it goes — multiple answers are normal:

1. **runar**, via `muninn_icebox_add`. Listable later, syncs to the team.
2. **A local `ICEBOX.md`** in the repo. Reviewable in a diff, editable by
   hand.
3. **Both.**

Before filing, run `muninn_icebox_list` and check for a near-duplicate. If
one exists, add to it rather than filing a second — a backlog with two
entries for one problem is a backlog nobody trusts.

## Listing

`muninn_icebox_list` with no status shows everything; `status: "open"` shows
what is still live. When the user asks what to work on next, list the open
items and say what you would pick and why — do not just print the list.

Items are excluded from automatic recall on purpose: a parked idea is not a
fact about the code, and injecting it would make intentions look like
decisions. These tools are the only way the backlog is seen, so use them
when the subject comes up.

## Promoting

When the user decides to work on an item:

1. Invoke the `prd` skill to turn it into a real plan — the interview is the
   point. A one-line item is not a specification.
2. Then close the loop:
   `muninn_icebox_set_status(slug, status: "promoted", promotedTo: <plan slug>)`.

When an item is decided against, record that too:
`muninn_icebox_set_status(slug, status: "dropped")`. A dropped item with a
reason stops the same idea being re-filed every few months. Deleting it does
not.
