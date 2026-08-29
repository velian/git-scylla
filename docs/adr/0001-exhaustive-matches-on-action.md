# Matches on `Action` stay exhaustive, even where that restates a fact

`Action` is the one enum in this project whose exhaustiveness is load-bearing:
adding a variant should fail to compile until someone has decided what it means
for argv expansion, undo, eligibility and resolution. Where two matches state
the same fact about the same variants — `Action::is_resolved` and the two
`Vec::new()` arms in `Action::steps` that refuse to expand an unresolved action
— we keep both matches rather than routing one through the other.

## Considered options

Rewriting `steps` as an early return on `!self.is_resolved()` states the fact
once. It also means a future variant carrying an unresolved `Option` gets a
silently correct answer from `steps` instead of a compile error, which is the
trade this enum exists to refuse. Deduplication here buys one edit site and
sells the diagnostic that catches the edit you forgot.

Expect this to be re-suggested: two matches over the same variants look like
duplication, and from inside either one, they are.
