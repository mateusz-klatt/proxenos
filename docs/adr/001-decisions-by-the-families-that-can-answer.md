# 001: Decisions rest on the model families that can answer

Status: accepted on 2026-11-24 at the owner's direction.

## Problem

Proxenos exists because a second model family sees what the first one misses. On its first day
its own design did not get that treatment consistently:

- A ten-role roster reached the owner from one research agent of the same family as the session
  that presented it. The owner asked whether Codex, Antigravity or Kimi had seen it. They had
  not.
- Two consultations ran on the bridges' default models, which were not the strongest ones those
  providers offered.
- Three of the seven families had no quota left that day, so waiting for all of them would have
  stopped the work.

## Direction

On 2026-11-24 the owner directed:

> pamietaj ze z racji ze podejmujemy teraz decyzje architektoniczne uzywac tylko najmocniejszych
> modeli w trybie ultra i max

English: remember that, since we are taking architecture decisions now, only the strongest models
are to be used, at ultra and max.

> idziemy z tym co mamy teraz, pozniej robimy konsensus... a jak jakas rodzina modeli wroci
> pozniej (np grok za 2 dni) to ponownie glosujemy... najwazniejsze zeby miec architekture
> opracowana i unit testy... implementacje zawsze mozna zmienic... tak dokumentujmy wszystko
> wraz z tym kto byl autorem aby miec context

English: we go with what we have now and reach consensus later; when a model family comes back
later (Grok in two days, for example) we vote again. What matters most is a worked-out
architecture and unit tests; an implementation can always be changed. Yes, let us document
everything, with who the author was, to keep the context.

## Decision

1. A design question goes to every model family that can answer, each on its strongest model at
   its highest effort. Each answers alone. Where a proposal already exists, a family writes its
   own answer before it reads the proposal.
2. A family without quota does not hold a decision up. The decision is taken on the positions
   that exist, and the record names the families that were absent.
3. When an absent family returns, the recorded decisions are put to it. A decision it contests
   is voted on again.
4. `docs/votes.md` records each question: who proposed, who answered and on which model and
   effort, where they agreed, where they differed, and what the owner decided. A vote informs.
   The owner decides.
5. A choice of implementation may be recorded as provisional. It stays cheap to reverse: it
   lives behind one module, and tests that do not depend on it describe the behaviour.
6. The architecture and its tests come first. Code follows them.

## What this does not do

- Sessions of one family are not independent voters. Seven Claude Code sessions answered a
  question about their experience with the predecessor; that is a report from users, and it is
  recorded as one.
- A family's account of its own strengths and failures is a judgment. None of them has been
  measured here.
- Agreement between families is evidence, not proof. Two of them agreeing on something neither
  has run is still unmeasured, and the record says which claims were measured.
