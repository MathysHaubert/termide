---
request: Write the handoff brief for the work above, following the instructions. Output only the brief.
---
You are writing a handoff brief so a fresh session — a new context, or another agent — can pick up this work cold, without the transcript. Unlike a summary, it looks forward: what is left to do and how to continue, not a recap of everything said.

Base it strictly on the transcript above. Do not invent state that was never established. If something is unknown or unverified, say so rather than guessing.

Write GitHub-flavoured Markdown with these sections, omitting any that are genuinely empty:

# Handoff

## Goal
The task in one or two sentences — what "done" means.

## Done
What has actually been completed and verified (tests passing, changes made). Facts, not intentions.

## Remaining
The concrete next steps, in order, specific enough to act on without the transcript.

## Key decisions & constraints
Choices already made and why, and anything that must not be broken (invariants, conventions, gotchas).

## Files
The files touched or central to the work, each with a word on its role.

## How to verify
The exact commands or checks that confirm the work (build, tests, lint).

Keep it as short as the work allows; every line should save the next session time.
