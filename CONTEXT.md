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
something else; a gated one needs the repository changed.

The gate runs twice, over the same action at two stages of completeness. Once
on the template, because a repository that will be refused should not be asked
a **ref question** — the first pass is what keeps the cold reads down to the
repositories that could actually run. Once more on the finished action, because
a rule like sync's cannot be judged until resolving has named the trunk.

Both passes are the same function, judged against the same snapshot, the same
clock and the same policy, so the second changes an answer only where the first
was missing a fact. That is the point: a rule that needs a resolved fact is
enforced by the gate like every other rule, rather than by whichever step of
resolving remembered to ask.

## The network verdict

Whether this machine can reach anything at all.

A fact about the *machine*, and that is the whole reason it exists separately
from a repository's fetch health. Backing off and quarantine are verdicts on a
**repository**: it keeps refusing, so stop asking and tell the user. An outage
is nothing of the kind — every repository in the working set fails at once, for
a reason none of them had any part in, and quarantining forty of them leaves
forty things for the user to restart by hand once the wifi comes back.

So while the verdict is down, a failed fetch records the attempt and nothing
else: the schedule does not move and the failure count does not rise.

Two independent facts put it down, and the verdict is down while either holds.
Each is set and lifted only by its own evidence.

**No route off the machine** is asked before anything runs, of the kernel's
routing table, in about a fifth of a millisecond. Nothing is spawned, so
nothing fails, so there is nothing to attribute to a repository afterwards —
and it answers for a whole plan at once, in front of the user, rather than in
forty transcripts of the same sentence. A route coming back lifts it, with no
fetch needed to prove anything.

**Nothing reachable** is learned from fetches that already ran. It is the only
evidence that can catch a network with a route and no service — dead DNS, a
captive portal, a VPN half up — and only a fetch that reaches something can
lift it, because only a fetch could have found it. A returning route must not
lift this one: a captive portal has a perfectly good route. Fetches that fail
while there is no route are not counted towards it — the missing route already
explains them.

Only a fetch from a remote *host* is evidence either way. A path remote
succeeds with the wifi off, and letting it vote would lift a verdict it knows
nothing about.

The route question is asked only in the negative. A route that exists is not a
promise that anything answers, and everything stronger has a way of being wrong
about this machine — an ssh alias is not a hostname, an `insteadOf` rewrite is
not the URL we read, a proxy or a VPN answers for names that resolve to nothing
here. All of those still need a route, so "no route" is the one negative that
cannot be a false one. Anything else is left to git, which is the only thing
that actually knows.

A repository whose remotes are all paths is never held, whichever fact is
down. It never needed a network, and `Remote::host` is `None` for exactly
those.

## The recheck

The one fetch that keeps running while the verdict is down *and* a route
exists.

Holding every repository would be a stop rather than a pause: nothing tells the
app that DNS came back, so something has to keep asking. One hosted repository,
at the recheck interval counted from when the verdict went down, chosen as the
one asked longest ago — the question is about
the machine, so any repository can answer it, and rotating is what stops a
repository with a genuinely broken remote from answering "still down" for
everyone else for ever.

With no route there is no recheck at all. The routing table is asked instead,
and it costs nothing.

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

The same rule decides *which* directory has to be readable: the one the refs
are actually read from. A linked worktree keeps `refs/` in the main
repository's git directory, so a worktree whose main repository is unreachable
is a repository whose tags and trunk are unknown — not one with no tags and no
trunk. An empty answer read from the wrong directory is the failure this
distinction exists to prevent, and it is indistinguishable from a true one
once it has been returned.

## The engine's I/O seam

`Arc<dyn Probe>` — and it is the only one. The engine reaches the filesystem
through that trait or not at all.

This is a claim the code makes about itself and has to keep. It stopped being
true once resolution read `refs/` through free functions, and the cost was not
theoretical: a planning path could not be tested without building real
repositories with real `git`, and every one of those reads ran synchronously on
the actor task that also serves every command.
