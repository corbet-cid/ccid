# `ccid lock-refresh`

The one mover of a repository's committed `Cargo.lock` files. It runs when something happened,
never on a schedule:

| Event | Invocation |
|---|---|
| one of the fleet's own crates published a release | `ccid lock-refresh --package <crate>` in each repository that depends on it |
| a release run's preflight | `ccid lock-refresh --third-party` |
| a person asks for it | either form, by manual dispatch |

The command changes the lock files in the checkout and nothing else. Whatever runs it commits the
result as one lock-only commit and lets it go through the repository's normal flow; that commit's
own verify run is the proof. Advisories stay a check (`ccid advisories`, `cargo deny`), so a
vulnerable lock still turns a commit red before any refresh moves it.

```
ccid lock-refresh [--repo .] [--package NAME]... [--third-party]
```

## What moves

- **Always**: every branch-tracking git package that the release channel's fleet data classifies as
  first-party (the same selection as the float, `float.md`/`verdicts.md`), to the head of its branch:
  `cargo update -p <git+url#name@version> …`, one invocation per lock. Cargo moves a third-party
  package only where a moved first-party package requires it; every such change is reported.
- **`--package NAME`** (repeatable): a registry package that a release event names. Every locked
  registry version of it is moved to the newest version its requirements allow
  (`cargo update -p NAME@<version>`). A name that no lock holds moves nothing and is listed under
  `absent_packages`: an event can reach a repository that does not depend on the crate.
- **`--third-party`**: one plain `cargo update` per lock, every package to the newest version its
  requirements allow.

Without `--third-party` no third-party version moves on its own, and no `-p` names a third-party
package except one a release event named.

Every committed lock moves: the root lock and the nested ones (the committed tree's `Cargo.lock`
files, as the float finds them). A manifest is never edited: a release outside a dependent's
version requirement needs a manifest change, which the refresh does not make.

## How

The resolution runs in a copy of the checkout under ccid's scratch directory, exactly like the
float. Then the lock files whose bytes changed are written back into the checkout (a lock that is a
symbolic link is refused). The command prints one JSON line:

```json
{"mode":"first-party","requested":{"Cargo.lock":["update","-p","git+…#dep@0.1.0"]},
 "changed":["Cargo.lock"],"absent_packages":[],
 "first_party":[{"lock":"Cargo.lock","name":"dep","version":"0.1.0","source":"git+…#<new>","change":"added"}, …],
 "third_party":[]}
```

`first_party` and `third_party` list every lock entry that was added, removed or changed, by the
fleet's classification of its source (a registry package is third-party by source even when a
release event named it).

## Refusals

- Inside a ccid check (`CCID_TARGET_LOCK_HELD` or `CCID_CACHE_CHILD` set): a check never moves the
  locks its verdict is keyed on.
- Without fleet data (`CCID_FLEET_FILE`): the report classifies every moved entry.
- A `--package` value that is not a crate name.
- A failed `cargo update` (exit 2, its message included); nothing is written then.
