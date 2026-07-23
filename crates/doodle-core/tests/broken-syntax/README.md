# Broken-syntax message corpus (M1.13)

Hand-written broken Doodle programs — **one mistake each, kid-plausible** — whose
rendered diagnostics are snapshotted (`tests/broken_syntax.rs`, via the M1.1
`diag::render`) so the messages a beginner actually sees are pinned and reviewable
against the **error-message rubric** (`discussions/plan/error-message-rubric.md`).

Per plan-m1 M1.13 the implementing agent does **not** self-certify: the table
below is the agent's rubric-pass; **the user approves** each message (the
`Sign-off` column, `ok` / a note). This review + the M1.1 rubric sign-off are
blocking for M1.15.

Review message changes with `cargo insta review`; see a program's message with
`cargo test -p doodle-core --test broken_syntax` then read its `.snap`.

## Status

**41 programs; agent rubric-pass complete; USER SIGN-OFF COMPLETE
(2026-07-19, approved as tabled + additions).** A sign-off on a
NEEDS-WORK/FAIL row endorses its verdict *and* the "Gap → intended fix"
direction. The approval included spot-check verification of 05/06/19/25/37
against snapshots (all matched) and four **corpus additions** (rows 42–45,
pending below). Scope as decided: **catalog + fix-spans-only** — the review
+ span fixes are M1.13; the message-quality fixes below (cascade
suppression, `=`/`==` and comma suggestions, de-jargon) are **spun off as
follow-up items** (`claude-todo.md`), not gated on M1.13. Cascades are
cataloged as-is.

**M1.13 GATE MET (span fixes + additions landed).** The in-scope span fixes
landed (systemic finding 1 below — unclosed-construct carets now point at the
opening token; the duplicate-declaration note points at the first declaration),
moving `01`/`08`/`16`/`32`/`35` to PASS; rows 42–45 joined the table (**45
programs**). Every diagnostic now points at the correct span.

Distribution: **30 PASS · 7 NEEDS-WORK · 8 FAIL** (45 programs, post-span-fix).
The recently-written resolver diagnostics are largely rubric-quality; the
remaining gaps/failures cluster in the parser's token-level error paths (cascades,
jargon — spun off). Two adversarial read-only reviews hardened this table (see
"Review folds").

## Rubric-pass table

Rubric elements — **(a)** name the value/operation · **(b)** point at it (correct
span) · **(c)** suggest the fix · **(d)** kid-readable (no jargon; `to`=procedure,
`fn`=function). `#` = diagnostics emitted (>1 = a cascade). Verdict is the agent's.

| Program | # | Code | Verdict | Gap → intended fix | Sign-off |
|---|---|---|---|---|---|
| `01-missing-end` | 1 | `syntax-error` | PASS | (b) fixed — caret now on the opening `to` | ok |
| `02-equals-in-if` | 5 | `syntax-error` | FAIL | (a)(c)(d) 5-error cascade + jargon; never diagnoses `=` vs `==` — want `did you mean x == 3?` | ok |
| `03-unclosed-string` | 2 | `unterminated-string` | NEEDS-WORK | primary is great; suppress the cascaded `expected )` | ok |
| `04-missing-comma` | 4 | `syntax-error` | FAIL | (a)(c)(d) 4-error cascade + jargon; want `arguments are separated by commas — did you mean show(1, 2)?` | ok |
| `05-stray-do` | 1 | `syntax-error` | FAIL | (a)(b) misdiagnosis: EOF span, blames the `to`; point at the `do` (opens an unclosed block) | ok |
| `06-chained-comparison` | 1 | `chained-comparison` | PASS | — the bar | ok |
| `07-missing-then` | 1 | `syntax-error` | PASS | span at the insertion point (after `if x`), not EOF — ok | ok |
| `08-missing-fn-end` | 1 | `syntax-error` | PASS | (b) fixed — caret now on the opening `fn` | ok |
| `09-unclosed-triple-string` | 1 | `unterminated-string` | PASS |  | ok |
| `10-unclosed-bytes` | 1 | `unterminated-string` | PASS |  | ok |
| `11-missing-comma-list` | 4 | `syntax-error` | FAIL | 4-error cascade; want a single `commas separate list items` with a fix | ok |
| `12-bad-escape` | 1 | `unknown-escape` | PASS | — names it + suggests `\\` | ok |
| `13-keyword-as-name` | 6 | `syntax-error` | FAIL | 6-error cascade for `let end = 3`; want `end is a keyword, not a name` | ok |
| `14-double-underscore-number` | 1 | `malformed-number` | PASS |  | ok |
| `15-else-without-if` | 2 | `syntax-error` | FAIL | (a)(d) jargon `expected an expression`; want `this else has no matching if` | ok |
| `16-unbalanced-paren` | 1 | `syntax-error` | PASS | (b) fixed — caret now on the call `(` | ok |
| `17-let-missing-value` | 1 | `syntax-error` | NEEDS-WORK | (d) `expected an expression` is jargon; want `this let needs a value after =` | ok |
| `18-missing-while-do` | 1 | `syntax-error` | PASS | span at the insertion point (after `while x`) — ok | ok |
| `19-keyword-as-param` | 9 | `syntax-error` | FAIL | 9-error cascade for `to f(fn)`; want `fn is a keyword and can't be a parameter name` | ok |
| `20-chained-equality` | 1 | `chained-comparison` | PASS | `==`-chaining is a distinct kid intuition from `<`-chaining; both kept deliberately | ok |
| `21-return-outside-callable` | 1 | `misplaced-exit` | PASS |  | ok |
| `22-break-outside-loop` | 1 | `misplaced-exit` | PASS |  | ok |
| `23-continue-outside-block` | 1 | `misplaced-exit` | PASS |  | ok |
| `24-const-reassignment` | 1 | `const-reassignment` | PASS | — rubric example (msg is right; the rubric's own good-example `var` is the outdated bit) | ok |
| `25-assign-undeclared` | 1 | `undeclared-assignment` | NEEDS-WORK | (d) verbose: the imports/`with` clauses are noise for the common `forgot let` case | ok |
| `26-duplicate-declaration` | 1 | `duplicate-declaration` | NEEDS-WORK | (b) fixed — note points at the first `let a = 1`; (c) rename/drop suggestion spun off | ok |
| `27-procedure-in-expression` | 1 | `procedure-in-expression` | PASS | — rubric example | ok |
| `28-if-expression-missing-else` | 1 | `if-expression-missing-else` | PASS |  | ok |
| `29-non-producing-branch` | 1 | `non-producing-branch` | PASS | span at the branch insertion point — ok | ok |
| `30-fn-falls-off-end` | 1 | `function-falls-off-end` | PASS |  | ok |
| `31-shadowing-warning` | 1 | `shadowing` | PASS | — rubric example (warning) | ok |
| `32-missing-index-bracket` | 1 | `syntax-error` | PASS | (b) fixed — caret now on the `[` | ok |
| `33-with-missing-do` | 1 | `syntax-error` | PASS | span at the insertion point (after the `with` header) — ok | ok |
| `34-positional-after-keyword` | 1 | `syntax-error` | NEEDS-WORK | (c)(d) two terms of art in one sentence + no fix; want `did you mean move(5, steps: 10)?` | ok |
| `35-record-missing-end` | 1 | `syntax-error` | PASS | (b) fixed — caret now on the opening `record` | ok |
| `36-stray-close-paren` | 2 | `syntax-error` | FAIL | (d) jargon `expected a statement separator` for a stray `)`; want `this ) has no opening (` | ok |
| `37-extra-decimal-point` | 1 | `syntax-error` | NEEDS-WORK | (a) `1.2.3` is read as field access → misleading `expected a field name after .`; want `a number can have only one . point` | ok |
| `38-margin-under-indent` | 1 | `margin-mismatch` | PASS |  | ok |
| `39-tab-margin-mix` | 1 | `margin-mismatch` | PASS | — names the tab/space margin mismatch | ok |
| `40-non-ascii-bytes` | 1 | `non-ascii-bytes` | PASS | — names it + suggests `\xHH` | ok |
| `41-unicode-escape-in-bytes` | 1 | `malformed-escape` | PASS | — names it + suggests `\xHH` | ok |
| `42-empty-interpolation` | 1 | `empty-interpolation` | PASS | — names it + suggests `{{` (S-48) | ok |
| `43-comment-in-interpolation` | 1 | `comment-in-interpolation` | PASS | — names it + suggests moving it out (S-50) | ok |
| `44-newline-in-interpolation` | 4 | `unterminated-interpolation` | NEEDS-WORK | primary is good (S-47); the open interpolation cascades to 4 diagnostics | ok |
| `45-short-hex-escape-string` | 1 | `malformed-escape` | PASS | — names it + suggests `\x1B` (S-49) | ok |

## Systemic findings

1. **EOF spans for unclosed constructs — RESOLVED.** Every `expected \`end\`/
   \`)\`/\`]\`` error for an unclosed callable/record/call/list/index used to point
   at a blank line past the construct (`01`, `05`, `08`, `16`, `32`, `35`, the `19`
   cascade tail) instead of the opening token — one systematic gap (the
   unterminated-*string* family already pointed at the opening). **Fixed:**
   `expect_end_span`/`expect_close` now take the opening token's span and report
   there, so the caret lands on the `to`/`fn`/`record`/`(`/`[` that needs closing.
   (`05`'s span is now on its `to`, but its deeper *misdiagnosis* — it should point
   at the stray `do` — folds into the M1.9b `stray_do` enrichment, spun off.)
2. **Cascades.** One mistake yields 2–9 diagnostics (`to f(fn)`: 9; `let end = 3`:
   6; `=`/`==`: 5). For a beginner this is a wall of noise. Parser error-recovery/
   suppression — a design-level change, spun off.
3. **Jargon + no diagnosis.** `expected a statement separator` / `expected an
   expression` break the no-jargon rule; classic kid mistakes (`=`→`==`, missing
   comma, stray `do`, keyword-as-name, extra `.`) get a generic `expected …`
   instead of a "did you mean …?" with a concrete fix.

## Review folds (two read-only adversarial passes)

- **Fidelity:** `37` was mislabeled `malformed-float` (`1.2.3` lexes as `1.2` +
  field access `.3`) → renamed `extra-decimal-point`; the named **tab/space
  margin-mix** category was absent → added `39`; `11` had two missing commas →
  reduced to `[1 2]`; `19` "reserved-word" framing (the spec has one reserved set)
  → renamed `keyword-as-param`; added bytes-literal cases `40`/`41`.
- **Verdicts:** five PASS rows shared `01`'s EOF-span defect (`08`, `16`, `32`,
  `35`) or leaned on jargon/omitted a fix (`34`) → all moved to NEEDS-WORK; `26`
  moved to NEEDS-WORK (no "the original is here" note). Distribution corrected
  25/5/8 → **22/11/8**.
- **Deferred (spun off):** the rubric's Appendix-A code catalog has drifted from
  the shipped slugs (`assign-to-undeclared`→`undeclared-assignment`,
  `bad-escape`→`unknown-escape`, the three `*-outside-*`→`misplaced-exit`,
  `function-missing-value`→`function-falls-off-end`,
  `under-indented-line`→`margin-mismatch`) — reconcile Appendix A with the code.

## Corpus additions (approved with the 2026-07-19 sign-off) — LANDED as rows 42–45

The individually-ruled interpolation/escape diagnostics — exactly what this corpus
exists to review — were absent; now added (programs + snapshots + table rows;
the diagnostics already existed from M1.4):

- `42-empty-interpolation` — `{}` in a string (S-48's dual-intent message). PASS.
- `43-comment-in-interpolation` — `#` inside `{…}` (S-50's targeted error). PASS.
- `44-newline-in-interpolation` — a `{expr` left open across a line (S-47).
  NEEDS-WORK: the primary message is good but the open interpolation cascades.
- `45-short-hex-escape-string` — string-side `\x` with one digit (S-49's
  malformed-escape; `41` covers only the bytes-literal side). PASS.

## Follow-up items (spun off; see `claude-todo.md`)

- **Parser error-recovery / cascade suppression** — one mistake → one message.
- **Pattern diagnostics + de-jargon** — `=`/`==`, missing comma, misplaced
  `else`, keyword-as-name, extra `.`; replace `expected a statement
  separator`/`expected an expression`. The **stray-`do`** case (`05`) folds
  into the queued M1.9b `stray_do` enrichment (same diagnostic site), not a
  separate item.
- **`25` conditional hedge (sharpened at sign-off)** — the fix is not "make it
  shorter": emit the imported-name/`with` hedge **only when the module
  lexically contains a wildcard import** (the resolver knows for free); a
  no-wildcard module gets the plain "write `let total = …`" message. This is
  the S-39 ruling's specific-where-lexically-known principle applied one
  level deeper.
- **Span fixes (M1.13 scope)** — point unclosed-construct errors at the opening
  token, not EOF (systemic finding 1); add the "original is here" note for `26`.
- **Rubric Appendix-A reconciliation** — update the catalog to the shipped codes.

## Sign-off

**Complete (user, 2026-07-19): all 41 rows signed `ok` — approved as tabled,
plus the additions above.** M1.13's review gate required the span fixes (systemic
finding 1, in scope) and rows 42–45. **Both landed** — the gate is **MET**: 45
programs, every diagnostic points at the correct span, and the message-quality
follow-ups are tracked in `claude-todo.md`.
