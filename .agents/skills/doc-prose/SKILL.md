---
name: doc-prose
description: >-
  Prose norms for writing or revising any felis documentation page: paragraph-level argument structure, rigor rules (no
  unearned hedging or unearned certainty), reader-load management, rhetoric restraint, and the filler judgments `just
  prose-check` cannot make. Use whenever drafting, rewriting, or reviewing sentences under docs/ (doc-cascade owns the
  structural checklist: quadrant, reference/explanation separation, sweep, and the decision-record gate; this skill owns
  the sentences). Also owns the code-comment norms (§9): load it when writing or reviewing Rust comments and doc
  comments. Default to no comment; only why-nots with a non-obvious failure and invariants the code cannot show survive,
  at most five lines, never history or what the code already shows. Also use when asked to "polish", "tighten", or "make
  this read less like an LLM wrote it".
allowed-tools: Read Grep Edit Write
---

# felis prose norms

`doc-cascade` decides where content goes; this skill governs how the sentences read. Existing pages may violate these
norms: treat them as cleanup targets, not precedent.

`just prose-check` (also the pre-commit hook) decides the mechanical rules on the added lines of a diff: dashes, history
phrases, the unambiguous filler phrases, and comment length (Markdown formatting and line wrapping are handled
automatically by `oxfmt` via `nix fmt`). Its output states the fix; this page does not repeat those lists. A line
carrying `prose-check: ignore` is skipped, and `<!-- prose-check: off -->` brackets a region up to
`<!-- prose-check: on -->`. Every rule below is a human judgment.

## 0. Voice by quadrant

- **tutorials / how-to**: second person, imperative, task-first. "Run a session on a remote box" is correct here.
- **reference**: declarative facts and tables, zero argument. Only §5 (filler) and §6 (redundancy) apply.
- **explanation**: the argument quadrant; everything below applies at full strength. No "you" outside scene-setting
  ("suppose you…"); refer to roles (the user, the client, a producer).

Bold a term at its defining first use; code, escape sequences, config keys, and CLI invocations in backticks or fenced
blocks, never paraphrased.

## 1. Paragraphs as argument steps

- One topic per paragraph; split narration that mixes investigation, finding, and evaluation.
- The first sentence says what the paragraph is about, and its opening words state the relation to the previous
  paragraph (so, but, in practice, the same holds for…).
- Argue in one direction: clear the objections, then state the conclusion once. No conclusion → objection → conclusion
  loops.
- Deny anticipated misreadings explicitly, then give the real reason: "The reason is not X. It is Y, because…".
- When denying or limiting, quote the exact proposition being denied ("this does not mean 'a spec makes review
  unnecessary'"), never a vague "this doesn't solve everything".
- Forward references sit at the end of a paragraph or section, and must actually be redeemed at the target.

## 2. Rigor

- Keep uncertainty that is real; cut hedging that is not. "may" and "likely" survive only when the text genuinely cannot
  confirm the claim; once the evidence settles it, state it flatly. Never mechanically upgrade a supposition to an
  assertion.
- Do not fuse distinct things. Separate decisions, causes, and kinds of problems keep separate names; if they interact,
  say so ("separate decisions that depend on each other").
- A causal claim carries its mechanism: not "splitting by stage makes changes ripple" but "each stage then shares a data
  representation, and changing it ripples through all of them".
- State detection, guarantees, and fixes with their conditions ("catches X when Y holds", "only if"), never as
  unconditional.
- Check that an example carries the full claim; if it supports only part, narrow the claim to match.
- A concession ("granted, …") must be followed by the argument moving forward; never end a section on it.
- Define a term before leaning on it, then use it everywhere; do not drift back to "the tool" or "the context".
  `docs/reference/glossary.md` is the term authority.

## 3. Reader load

- Don't name what the reader never needs again: an identifier mentioned once is "the config struct", "a session id".
- If an abstract phrase could point at two things, pin it with a parenthetical rather than making the reader scroll
  back.
- Each new example costs held context; buy it with one sentence of motivation (what it shows that the previous one
  couldn't).
- Trim ornamental precision (timestamps, status codes, percentages no later sentence uses); keep the specifics the
  argument consumes.

## 4. Restraint

Rhetoric is rationed, not banned: spend it where the argument peaks.

- Dramatic setup, rhetorical questions, and punch-sentences as their own paragraph: once per page at a genuine turning
  point, at most.
- Bold in running text: one or two per section, only to block a misreading or land a conclusion. Emphasize by sentence
  order, not typography.
- State turning points as plain fact; don't stack consequences to alarm ("data loss, corruption, and worse").
- Don't preface claims ("What matters here is that…"); write the claim. Announcing the register is fine ("as a slogan:
  …").
- No twisted idioms or metaphors whose referent isn't unique; use the plain verb.

## 5. Filler

The checker catches the fixed phrases. What it cannot see:

- **Empty intensity** ("robust", "seamless") standing in for the claim's content: say what the thing does.
- **Structural tics**: the rule of three where fewer items would do; negative parallelism; trailing participial analysis
  ("…, ensuring that…"). If the observation matters, give it its own sentence.
- **Vague attribution** ("it is widely considered"): cite the source or drop the claim.
- **History narration** in any phrasing the checker misses. State the current contract as if it had always held; the
  past belongs to `git log` and, for user-affecting surface changes, `CHANGELOG.md` (placement rule: doc-cascade
  "History is not doc content"). A rejected alternative is not history: argue it in the present tense ("a sibling-field
  pair would…").
- **Wrap-ups**: a page ends when its content does.

Test: delete the phrase. If the sentence loses nothing, it was filler; if it loses the claim, rewrite so the claim is
the sentence.

## 6. Cut redundancy

- One claim, stated once. Merge adjacent sections that say the same thing from two angles.
- After a scene or example, don't re-summarize; add only the one sentence that says what it means.
- Parallel facts with the same logical role share one sentence.
- Skip derivation steps the reader can supply; if an argument compresses to one sentence, keep only that sentence.
- No connective-only or evaluation-only sentences ("That is a good thing in itself.").
- No staged Q&A with an imagined reader. A real anticipated question may stand as a plain question; the answer follows
  without theater.
- No authorial throat-clearing or self-defense ("this document does not claim otherwise").
- No hand-maintained index of a page's own links (`Cross-references`, `External sources cited`): the inline citation is
  the record.
- Don't foreshadow terms or documents not yet introduced.

## 7. Headings

- A heading names the question the section answers or the object it covers: "Survive the disconnect", not "More
  details".
- Not the section's punchline; the reader should not learn the conclusion from the table of contents.
- Question form or noun phrase, whichever fits the page.

## 8. Honesty

- If an example could look contrived, say so, and ground its plausibility in common experience ("this failure mode is a
  familiar one"), not authorial assertion.
- Never write around an unverified claim so smoothly that it reads as verified: cite the upstream source or mark the
  status honestly.

## 9. Code comments

Default to no comment: every naming, factoring, and structuring decision has a reason, and almost none of them get one.
A comment survives in exactly two cases, never to restate what the code does:

- a **why-not**: an alternative a competent reader would plausibly reach for, and its non-obvious failure;
- an **invariant the code cannot show**: a cross-boundary agreement, an environment quirk, an ordering constraint whose
  violation breaks something non-locally. When the constraint is recorded in a doc, the comment cites it
  (`docs/reference/cli.md`, …), following the same sourcing rule as the decision records.

<!-- prose-check: off -->

No history ("was removed", "renamed from"): the present-tense contract or nothing; `git log` owns the past. Justifying
the current change ("this is safe because…") belongs in the commit body, not the source. Test code is the one WHAT
surface: the test name and its comment state the contract being pinned, nothing else.

Length is the tell, and the checker enforces it: a comment block over five lines is an argument. The site keeps the one
sentence a reader needs (the invariant, or the alternative and its failure); the argument goes to the owning explanation
doc (cited from the comment) when it is a design decision, or to the commit body when it only justifies this change. A
module-level `//!` essay is the usual offender. `// SAFETY:` blocks are exempt: the workspace policy keeps the audit
record in the source.

<!-- prose-check: on -->

## 10. Review pass

Revise in this order; each pass changes what the next one sees:

1. **Argument** (§1–2): paragraph topics, direction, unquoted denials, unearned certainty or hedging.
2. **Cut** (§3, §6): filler, re-summaries, ornamental detail.
3. **Surface** (§0, §4–5, §7): rhetoric budget, register, headings; then `just prose-check` for the mechanical rules.

Then run doc-cascade's sweep; prose polish that renames a term is still a cascade.
