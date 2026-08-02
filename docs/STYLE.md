# How this codebase is written

Binding on every change, human or agent. The test for all of it: **a
competent engineer who has never seen this repo should be able to read a
file, understand why it exists, and safely change it.** If they can't, the
code is wrong even when the tests pass.

## Code

**Write for the next reader, not the compiler.** They have no context. They
are tired. They are trying to fix one bug.

- **Logic lives where it belongs.** State machines in one place, not spread
  across three files that each know a third of the rule. If you have to
  read four files to answer "when does this happen", the design is wrong.
- **Number the steps in any function with a real sequence.** A gate that
  checks eight conditions in a required order gets 1..8 in comments, in
  order, saying what each step decides. Ordering that matters must look
  like it matters.
- **One name per concept, everywhere.** If the ledger calls it `target`,
  the UI does not call it `agent` and the daemon does not call it `who`.
  Renaming across a boundary is where bugs hide.
- **Guards where failure is real, nowhere else.** A check earning its
  keep prevents a specific, nameable failure: typing into the wrong pane,
  a forged receipt, a torn ledger line. A check defending against nothing
  is noise that trains readers to skim past the checks that matter.
- **Delete rather than abstract.** Two call sites do not need a trait.
  Wait for the third, and for a reason beyond symmetry.
- **No cleverness without a comment that earns it.** If a line needs a
  paragraph to explain, first try to make it a line that doesn't.

## Comments

Explain what the code cannot: **why**, and what breaks otherwise.

- Say the constraint, the measurement, or the failure being prevented.
  "tmux 3.6a splits multi-byte glyphs across pty reads (F22), so this
  reads bytes" is worth more than any restatement of the syntax.
- Do not narrate the next line. Do not explain the change to a reviewer;
  that belongs in the commit message and is noise the moment it merges.
- If you cannot state a function's job in one sentence, the function is
  doing more than one thing. Split it before you document it.

## Documentation

Thorough, succinct, comprehensible. Those are compatible; jargon is what
breaks them.

- **One page answers one question**, and its title is that question.
- **Lead with the thing the reader does.** The command, the file, the
  config key. Explanation comes after, for whoever wants it.
- **Show real output.** Copied from a real run, not invented. If output
  changes, the page changes in the same commit.
- **Plain words.** "The daemon holds one connection per session" beats
  "the daemon maintains a per-session control-plane affinity". Every term
  a newcomer would have to look up is either defined on the page or cut.
- **Truth rule (absolute).** Never document what is not built and tested.
  A page describing behavior that no longer exists is a bug: fix it or
  delete it in the commit that changed the behavior.

## Visuals

A diagram is documentation, not decoration. Use one wherever a reader
would otherwise hold four things in their head at once.

- Mermaid in fenced blocks; GitHub renders it and it stays diffable text.
- Earn their keep: the delivery state machine, the gate order, the
  sensor-fusion tiers, how a message becomes a receipt. Those are the
  places prose is genuinely worse.
- Label edges with the condition that fires them. An unlabeled arrow is
  a shape, not an explanation.
- Keep them next to the code they describe, and update them together.

## The anti-goal

Do not build a system that is technically correct and practically
untouchable. Complexity that cannot be taken apart cannot be fixed, and
code nobody dares change is dead whether or not it runs. When a design
gets hard to explain, that is the signal to simplify it, not to write a
longer explanation.
