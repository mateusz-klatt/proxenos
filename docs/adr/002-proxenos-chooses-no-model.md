# 002: Proxenos chooses no model

Status: accepted by the owner on 2026-11-25: "3 x tak" (yes to all three), in answer to three
questions, of which this was one. Proposed the same day.

## Problem

D8 made the strongest model each provider's default. That was a provisional choice. Claude (Fable
5.2, max) made it in vote 4, where the two reviewers differed, and it went to the next vote.

## What vote 6 counted

Nine positions, the two of vote 4 and the seven of vote 6. [votes.md](../votes.md) names them.

- Seven opposed the strongest model as the default: GPT (Astra), Kimi, Gemini (Hydrogen),
  DeepSeek, Zhipu, Microsoft and xAI.
- Two kept it: Gemini (Neon), and Gemini (Argon) with an explicit value that passes no model.
- Of the seven, five said to pass no model unless one is named. Three of those five let the
  user's configuration name it.

## Proposal

Proxenos stops selecting the strongest model by itself. On a start it passes the model that the
call names, or else the one that the user's own overlay sets for that provider, and otherwise
none. A reply follows D19. The user of vote 5 who asked for the strongest model as the default can
request it by naming it, in the call or in the overlay.

D8 in [architecture.md](../architecture.md) is written to this decision.

## Open

The owner's acceptance did not settle this question.

What a reply does when it must name a model (D5) and its start passed none. It could repeat the
model that the provider reported for the start, where the provider reported one, or ask the caller
for one. No voter was asked.

## What this does not do

- It does not make a model run. A CLI that is given no model runs its own default or refuses to
  start, and a model that is named is a request: which one ran is what the provider reports.
- It does not hold a default still. A provider's default can change with a release of its CLI or
  with the user's configuration of it, and Proxenos does not see that happen.
