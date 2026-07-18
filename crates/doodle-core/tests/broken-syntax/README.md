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

**41 programs; agent rubric-pass complete; user sign-off pending.** Scope decided
with the user: **catalog + fix-spans-only** — the review + span fixes are M1.13;
the message-quality fixes below (cascade suppression, `=`/`==` and comma
suggestions, de-jargon) are **spun off as follow-up items** (`claude-todo.md`),
not gated on M1.13. Cascades are cataloged as-is. Parser fixes are held until this
sign-off (the review informs the fixes, not the other way round).

Distribution: **22 PASS · 11 NEEDS-WORK · 8 FAIL.** The recently-written resolver
diagnostics are largely rubric-quality; the failures/gaps cluster in the parser's
token-level error paths. Two adversarial read-only reviews hardened this table
(see "Review folds").

## Rubric-pass table

Rubric elements — **(a)** name the value/operation · **(b)** point at it (correct
span) · **(c)** suggest the fix · **(d)** kid-readable (no jargon; `to`=procedure,
`fn`=function). `#` = diagnostics emitted (>1 = a cascade). Verdict is the agent's.

| Program | # | Code | Verdict | Gap → intended fix | Sign-off |
|---|---|---|---|---|---|
| `01-missing-end` | 1 | `syntax-error` | NEEDS-WORK | (b) EOF span; point at / note the unclosed `to greet()` | |
| `02-equals-in-if` | 5 | `syntax-error` | FAIL | (a)(c)(d) 5-error cascade + jargon; never diagnoses `=` vs `==` — want `did you mean x == 3?` | |
| `03-unclosed-string` | 2 | `unterminated-string` | NEEDS-WORK | primary is great; suppress the cascaded `expected )` | |
| `04-missing-comma` | 4 | `syntax-error` | FAIL | (a)(c)(d) 4-error cascade + jargon; want `arguments are separated by commas — did you mean show(1, 2)?` | |
| `05-stray-do` | 1 | `syntax-error` | FAIL | (a)(b) misdiagnosis: EOF span, blames the `to`; point at the `do` (opens an unclosed block) | |
| `06-chained-comparison` | 1 | `chained-comparison` | PASS | — the bar | |
| `07-missing-then` | 1 | `syntax-error` | PASS | span at the insertion point (after `if x`), not EOF — ok | |
| `08-missing-fn-end` | 1 | `syntax-error` | NEEDS-WORK | (b) EOF span (identical defect to 01); point at the opening `fn` | |
| `09-unclosed-triple-string` | 1 | `unterminated-string` | PASS |  | |
| `10-unclosed-bytes` | 1 | `unterminated-string` | PASS |  | |
| `11-missing-comma-list` | 4 | `syntax-error` | FAIL | 4-error cascade; want a single `commas separate list items` with a fix | |
| `12-bad-escape` | 1 | `unknown-escape` | PASS | — names it + suggests `\\` | |
| `13-keyword-as-name` | 6 | `syntax-error` | FAIL | 6-error cascade for `let end = 3`; want `end is a keyword, not a name` | |
| `14-double-underscore-number` | 1 | `malformed-number` | PASS |  | |
| `15-else-without-if` | 2 | `syntax-error` | FAIL | (a)(d) jargon `expected an expression`; want `this else has no matching if` | |
| `16-unbalanced-paren` | 1 | `syntax-error` | NEEDS-WORK | (b) EOF span; the missing `)` belongs at end of line 1, not the blank line | |
| `17-let-missing-value` | 1 | `syntax-error` | NEEDS-WORK | (d) `expected an expression` is jargon; want `this let needs a value after =` | |
| `18-missing-while-do` | 1 | `syntax-error` | PASS | span at the insertion point (after `while x`) — ok | |
| `19-keyword-as-param` | 9 | `syntax-error` | FAIL | 9-error cascade for `to f(fn)`; want `fn is a keyword and can't be a parameter name` | |
| `20-chained-equality` | 1 | `chained-comparison` | PASS | `==`-chaining is a distinct kid intuition from `<`-chaining; both kept deliberately | |
| `21-return-outside-callable` | 1 | `misplaced-exit` | PASS |  | |
| `22-break-outside-loop` | 1 | `misplaced-exit` | PASS |  | |
| `23-continue-outside-block` | 1 | `misplaced-exit` | PASS |  | |
| `24-const-reassignment` | 1 | `const-reassignment` | PASS | — rubric example (msg is right; the rubric's own good-example `var` is the outdated bit) | |
| `25-assign-undeclared` | 1 | `undeclared-assignment` | NEEDS-WORK | (d) verbose: the imports/`with` clauses are noise for the common `forgot let` case | |
| `26-duplicate-declaration` | 1 | `duplicate-declaration` | NEEDS-WORK | (b)(c) add a note at the first `let a = 1`; suggest rename/drop the second | |
| `27-procedure-in-expression` | 1 | `procedure-in-expression` | PASS | — rubric example | |
| `28-if-expression-missing-else` | 1 | `if-expression-missing-else` | PASS |  | |
| `29-non-producing-branch` | 1 | `non-producing-branch` | PASS | span at the branch insertion point — ok | |
| `30-fn-falls-off-end` | 1 | `function-falls-off-end` | PASS |  | |
| `31-shadowing-warning` | 1 | `shadowing` | PASS | — rubric example (warning) | |
| `32-missing-index-bracket` | 1 | `syntax-error` | NEEDS-WORK | (b) EOF span; point after `0`, not the blank next line | |
| `33-with-missing-do` | 1 | `syntax-error` | PASS | span at the insertion point (after the `with` header) — ok | |
| `34-positional-after-keyword` | 1 | `syntax-error` | NEEDS-WORK | (c)(d) two terms of art in one sentence + no fix; want `did you mean move(5, steps: 10)?` | |
| `35-record-missing-end` | 1 | `syntax-error` | NEEDS-WORK | (b) EOF span; point at the opening `record` | |
| `36-stray-close-paren` | 2 | `syntax-error` | FAIL | (d) jargon `expected a statement separator` for a stray `)`; want `this ) has no opening (` | |
| `37-extra-decimal-point` | 1 | `syntax-error` | NEEDS-WORK | (a) `1.2.3` is read as field access → misleading `expected a field name after .`; want `a number can have only one . point` | |
| `38-margin-under-indent` | 1 | `margin-mismatch` | PASS |  | |
| `39-tab-margin-mix` | 1 | `margin-mismatch` | PASS | — names the tab/space margin mismatch | |
| `40-non-ascii-bytes` | 1 | `non-ascii-bytes` | PASS | — names it + suggests `\xHH` | |
| `41-unicode-escape-in-bytes` | 1 | `malformed-escape` | PASS | — names it + suggests `\xHH` | |

## Systemic findings

1. **EOF spans for unclosed constructs (the biggest systematic (b) gap).** Every
   `expected \`end\`/\`)\`/\`]\`` error for an unclosed callable/record/call/list/
   index points at a blank line past the construct (`01`, `05`, `08`, `16`, `32`,
   `35`, and the `19` cascade tail) instead of the opening token. The tell: the
   unterminated-*string* family (`09`, `10`) correctly points at the opening — so
   this is one systematic parser fix, not per-program noise. It is **in M1.13
   scope** (span-correctness).
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

## Follow-up items (spun off; see `claude-todo.md`)

- **Parser error-recovery / cascade suppression** — one mistake → one message.
- **Pattern diagnostics + de-jargon** — `=`/`==`, missing comma, stray `do`,
  misplaced `else`, keyword-as-name, extra `.`; replace `expected a statement
  separator`/`expected an expression`.
- **Span fixes (M1.13 scope)** — point unclosed-construct errors at the opening
  token, not EOF (systemic finding 1); add the "original is here" note for `26`.
- **Rubric Appendix-A reconciliation** — update the catalog to the shipped codes.

## Sign-off

Fill the `Sign-off` column (`ok`, or a note) per program. The corpus passes M1.13
review when every row is signed and every diagnostic points at the correct span.
