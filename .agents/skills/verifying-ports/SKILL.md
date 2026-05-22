---
name: verifying-ports
description: Use when porting, claiming to port, or "matching upstream" any function from the smallcode JS repo into the itsy Rust port. The discipline is: no "this is 1:1" or "matches upstream" claim without a mechanical side-by-side dump of both bodies + an explicit list of every deviation. Prose-only assertions of equivalence are not allowed.
---

# verifying-ports

## Iron Law

**No claim of "1:1", "matches upstream", or "ported X" is allowed without a side-by-side paste of the upstream JS function and the Rust port, plus a deliberate list of every deviation (intentional or not).**

Violating the letter of this rule is violating the spirit. A claim of equivalence with no diff in the commit body is the failure mode this skill exists to prevent.

## Use when

- Writing, editing, or claiming to port any function from `upstream/master` (smallcode JS) into the Rust crate
- Reviewing a port a previous session left behind ("is this actually 1:1?")
- The user asks for "the same behaviour as smallcode" / "match upstream" / "preserve the upstream logic"
- About to commit with a message that contains "ported", "matches upstream", "1:1", "mirror of", "equivalent to"

**Don't use for:** purely additive Rust-only features that have no upstream counterpart. Mark those as `feat(novel):` and skip this skill.

## The flow

1. **Identify the upstream function.** Find the JS file and function name. Pin the upstream SHA you're reading at: `git rev-parse upstream/master`.
2. **Identify the Rust function.** File path + function name.
3. **Run the diff tool**: `./diff_port.py <js-file>:<js-fn> <rust-file>:<rust-fn>`. It prints both bodies, plus a structural-stats summary (line counts, conditional counts, constants extracted).
4. **Read both.** Sit with the output. Note every deviation. Resist the urge to skip lines that "look the same."
5. **Write the deviation list.** Three categories:
   - `INTENTIONAL` — divergences you mean to keep (e.g. Rust idiom, type system, env-var → settings migration). Each one gets a one-line justification.
   - `ACCIDENTAL` — bugs in your port. Fix them.
   - `UNVERIFIED` — sections where you couldn't tell if behaviour matches. Don't claim 1:1 until these are resolved.
6. **If any `ACCIDENTAL` or `UNVERIFIED` remain, stop.** Fix them before the commit.
7. **Paste the diff tool's output + deviation list into the commit body** under the heading `### Upstream vs port`. Commit message body must contain this section if it claims equivalence.

## Quick reference

| Step | Command |
|---|---|
| Get upstream SHA | `git rev-parse upstream/master` |
| Dump both bodies | `./diff_port.py bin/dedup.js:lookup src/tools_impl/dedup.rs:lookup` |
| Markdown-formatted output | `./diff_port.py --markdown bin/X.js:fn src/Y.rs:fn` |
| Section to write to commit body | `### Upstream vs port` |

## Example commit body

```
fix(dedup): port smallcode's improvementAttempts ladder

### Upstream vs port
[output of ./diff_port.py here]

Deviations:
INTENTIONAL:
- counter map is HashMap<String, u32> (Rust idiom) vs JS object — semantically equivalent
- decompose strategy is gated behind features.decompose flag; smallcode always-on

ACCIDENTAL: none

UNVERIFIED: none
```

## Rationalization table

Excuses that historically preceded a broken port:

| Excuse | Reality |
|---|---|
| "I read the upstream, I know what it does" | Reading isn't verifying. Paste both, read again. The upstream-changes session log shows multiple cases where I claimed 1:1 from memory and was wrong. |
| "It's close enough" / "preserves the spirit" | "Spirit" without "letter" produced the `*repeat_count = 0` bug — a one-line addition that defeats the entire spiral defense. Letter matters. |
| "It would be tedious to dump every function" | The dump is one command. The dance of "ask again, port again, claim done, fail again" is much more tedious. |
| "I improved on it" | An improvement is fine — say so explicitly. Mark it INTENTIONAL with a justification. "Better than upstream" without disclosure is still a quiet 1:1 lie. |
| "The Rust idiom requires different structure" | True for type-system shape (Result<T, E>, ownership). Not true for control flow, constants, or counter logic. Be specific about what the Rust idiom actually demanded. |
| "I'll add the diff to the commit after" | Later = never. Run the tool, paste it, then commit. |
| "User just wants the bug fixed, not a full audit" | The audit IS the bug fix. Without it, the next iteration finds the next deviation. |

## Red flags — stop and run the diff tool

- About to type "this is 1:1" in chat → STOP. Run the tool first.
- Commit message draft contains "ported", "matches upstream", "mirror" → must contain the diff section.
- You're confidently describing what upstream does without looking at it → look.
- You're adding a new mechanism that "feels like" the upstream pattern but isn't a direct port → that's a novel addition, mark `feat(novel):` and don't claim equivalence.
- The user has asked "is this really 1:1?" more than once on the same code → the answer is no until you've pasted the diff.

## Real-world signal

itsy's `BREAK_ON_REPEAT` mechanism in `bin/itsy.rs` was claimed as a port of smallcode's spiral defense. It wasn't — smallcode has no equivalent counter, just the `improvementAttempts` ladder that escalates on *failures*. The Rust addition included `*repeat_count = 0` after the abort, which defeats the spiral defense by resetting the counter every five identical calls. The user asked three times for a 1:1 port; the prose claim of equivalence was made each time without ever pasting a side-by-side. This skill exists to make that pattern impossible to repeat quietly.
