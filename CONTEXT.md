# Context

The vocabulary this project uses for itself.

Terms are here because they were ambiguous in conversation and the ambiguity
cost something — not to catalogue every noun. How a thing works belongs in its
module header; why a decision was made belongs in `docs/adr/`.

## Action template

An `Action` as the user chose it, before any repository has been consulted.
`Action::SyncDefault { plan: None }` and `Action::DevTag { name: None }` are
templates: the `Option` is the part no snapshot can fill in, and it stays `None`
until one repository has answered for itself.

The distinction matters because a template is not runnable. A plan that showed
one is claiming a uniformity the working set does not have — "create the next
dev tag" is not something a user can check before confirming it forty times.

A **plan template** is the same idea one level up: a plan whose per-repository
actions may still be templates. It is a separate type from a `Plan`, so a plan
that has not been through resolving cannot be handed to anything that runs
one.

## Resolving

Turning an action template into one runnable action **per repository**, using
facts that are not on a `RepoSnapshot`. Which branch is this repository's trunk;
what tags does it already have. A repository that cannot be resolved leaves the
plan as a named skip.

## Validating

Deciding whether an action the user *fully* specified can run here. Does the ref
they typed exist. Validation never changes the action; it either keeps the row
or skips it.

Kept apart from resolving because the two fail differently. An unresolvable
repository has to be refused — there is no action to run. An unvalidatable one
is often better let through: refusing a checkout that would have worked is worse
than a job that fails with a good message, so an unanswerable question means
*try*, not *skip*.

## Gating

Deciding whether an action can run here, against facts the *engine* derived
rather than facts the user typed. Sync's "you are already on the default branch
and your tree is dirty" is one: nothing was mis-specified, and no answer is
missing — this repository is simply in a state the action refuses.

A third thing because the remedy is a third thing. An unresolvable repository
needs a repository that can answer; an unvalidatable one needs the user to type
something else; a gated one needs the repository changed. Gating runs after
resolving, since the facts it judges are the ones resolving produced.

## Hot and cold facts

**Hot** is what a `RepoSnapshot` carries: one `git status` per repository, on a
path that has to finish in under a second for a hundred of them.

**Cold** is everything too expensive for that — a `refs/` walk, a tag list. Cold
facts are read once per plan and never per row, and keeping them off the
snapshot is what makes a hundred-repository scan fast rather than thorough.

## Ref question

One cold question asked of every repository in a plan at once: a `RefQuery` in,
one `RefAnswer` or `RefError` per repository out.

One query per call, because the question comes from the action the user chose
once. One request per repository, because the facts it needs — the git
directory, the remote names — differ.

A `RefError` is **not** an answer of "no". A git directory that cannot be read
belongs to a repository whose trunk is *unknown*, and a plan that reported it as
trunkless would put a sentence in front of the user that may be plainly false.
Unknown and no have different remedies, so they are different values.

## The engine's I/O seam

`Arc<dyn Probe>` — and it is the only one. The engine reaches the filesystem
through that trait or not at all.

This is a claim the code makes about itself and has to keep. It stopped being
true once resolution read `refs/` through free functions, and the cost was not
theoretical: a planning path could not be tested without building real
repositories with real `git`, and every one of those reads ran synchronously on
the actor task that also serves every command.
