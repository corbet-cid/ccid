# Fleet data: first-party owners on the release channel

Some checks need to know which source owners belong to the fleet: which dependency URLs are
first-party, which host names spell the fleet's one forge, and which legacy spellings of an owner
moved where. That is **fleet data**. It exists once, as the `fleet` member of the release channel
(`channel.json` on ccid's main branch), which every runner already fetches to pick its ccid
release. It is never compiled into ccid and never copied into a repository. **An org move is an
edit of `fleet` in `channel.json`**: no ccid rebuild, no repository change. Retiring owners (and
every alias) is the same kind of edit; a smaller fleet is still valid data. The next run of every
`fleet = true` check misses its cached result once (the digest changed); nothing else is
invalidated. The code is `src/fleet.rs`.

## Schema

```json
"fleet": {
  "schema": 1,
  "forge_hosts": ["forge.example", "forge-alt.example"],
  "first_party": ["https://forge.example/org/", "https://forge.example/other-org/"],
  "aliases": {"https://mirror.example/org/": "https://forge.example/org/"}
}
```

| Field | Meaning |
|---|---|
| `schema` | `1`. Any other value is refused; unknown fields are refused. |
| `forge_hosts` | Optional. Every public host name of the one forge that serves `first_party`; the first is canonical. |
| `first_party` | Canonical owner prefixes, strictly ascending (byte order), no duplicates, at least one. |
| `aliases` | Optional. Legacy owner prefix (on another forge) → the canonical prefix it moved to. |

Every prefix, alias key and alias value is a **prefix**: a URL that parses, uses `https`, has no
user name, password, port, query or fragment, has a non-empty lowercase host, ends with `/`, and
whose path is exactly `/<owner>/` with `<owner>` of `[A-Za-z0-9._-]` (not `.` or `..`). The
string must be the URL's canonical spelling (no default port, no surrounding whitespace, no case
or escape variants), because matching compares bytes. Every alias value is a `first_party`
entry; no alias key is one. Every forge host is a lowercase DNS name (`[a-z0-9.-]`, no scheme,
port or path), listed once; when `forge_hosts` is present, every `first_party` prefix is on
`forge_hosts[0]` and no alias key is on any forge host. Errors name the offending entry.

A URL is classified by byte prefix after removing one leading `git+` and reading any forge host
spelling (`https://<forge host>/`) as the canonical one: **canonical** when it then starts with a
`first_party` prefix, **alias** when it starts with an alias key, otherwise outside the fleet. The
trailing `/` of every prefix keeps `https://forge.example/org-other/` out of
`https://forge.example/org/`; a URL with a user name, a port or another scheme matches nothing.
`same_forge(a, b)` is true for equal hosts and for two spellings of the forge.

The **digest** is the lowercase SHA-256 of the canonical bytes: the validated data serialized in
field order (`schema`, `forge_hosts`, `first_party`, `aliases` ordered by key). Two channel files
that differ only in key order or whitespace have the same digest.

### Host spellings that are not public: `CCID_FORGE_HOSTS`

A runner may reach the forge under a name that must not appear in the public channel (an
in-cluster service name). The runner names such spellings in `CCID_FORGE_HOSTS`
(comma-separated lowercase DNS names, surrounding blanks ignored); `fleet::load` validates them
like `forge_hosts`, appends them after the channel's list (a known spelling is skipped) and
refuses them when the channel has no `forge_hosts`. They are not fleet data: they are never part
of the canonical bytes or the digest, and never keyed. A host spelling is a route to the forge,
not a different forge. ccid forwards the variable to cached work as plumbing (cmnp).

## Where the data comes from

`CCID_FLEET_FILE` names a local copy of the **whole** release channel document: ccid's
`channel.json` from the main branch, exactly the document `CCID_CHANNEL_URL` names on runners (a
JSON object; ccid reads its `fleet` member). It must be a regular file (a symbolic link is
refused). Its location is never part of a key; its content is, through the digest.

| Context | `CCID_FLEET_FILE` |
|---|---|
| Forgejo job (`ccid job-run`), Crow/Argo job (`ccid execute-job`) | Set by the job runner when neither the runner nor the job names it. The source is the runner's `CCID_CHANNEL_FILE` when set, else the shim's copy `${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/ccid-channel/last-good.json` (a regular file with valid fleet data; a copy that does not parse yet is re-read a few times, since another job's shim may be rewriting it). The job then reads an immutable snapshot of that data, `ccid-channel/fleet-<digest>.json` beside the shim's copy (`{"fleet": <canonical bytes>}`, written once through a temporary file and a rename), so a later channel fetch never changes what a running job was keyed with; without a writable state directory it reads the source itself. The runner prints `fleet data <digest> from <source> (CCID_FLEET_FILE=<path>)`, or that the channel carries no fleet data, and continues. It never sets `CCID_CHANNEL_FILE`. A Crow/Argo host with neither file gets no fleet data: its `fleet = true` checks fail closed (see the requirement below). |
| Land gate and other lanes | The caller exports a fetched copy of `channel.json`. |
| Anything else | Whatever the caller names. Without it, `fleet = true` checks fail closed. |

### Requirement for any Crow or Argo job with fleet checks

Crow and Argo jobs run `ccid execute-job` with a pinned tool and no shim, so nothing writes a
channel copy on their hosts. Before a job on such an engine runs a `fleet = true` check, the
engine must give it `CCID_FLEET_FILE`: the absolute path of a regular file outside the source
tree holding a copy of the release channel, fetched at job start from the same document
`CCID_CHANNEL_URL` names (never a timer or a second, hand-maintained copy, so an org move stays
one edit of `channel.json`). A runner's `CCID_CHANNEL_FILE` also works as the source, but it
governs which verdict records the runner accepts as well, so set it only when that is intended.
Without either, those checks fail closed. Today no Crow or Argo job runs fleet checks.

## The `fleet = true` contract

A check that reads fleet data declares it in `.ci/ccid.toml`:

```toml
[checks.first-party]
kind = "commands"
fleet = true
commands = [["ccid", "fleet", "show"]]
```

| Step | What ccid does |
|---|---|
| Manifest load | Adds `CCID_FLEET_DIGEST` to the check's `cache_env` (once). That one entry keys the verdict store and cmnp's moon tasks alike. `CCID_FLEET_DIGEST` in the `cache_env` of a check without `fleet = true` is refused. |
| Before a selection is keyed or run (`ccid check`, `ccid cached`) | If any selected check declares `fleet = true`: load `CCID_FLEET_FILE` (or fail with exit 2, `Check <name> declares fleet = true but no fleet data is available: ...`) and set `CCID_FLEET_DIGEST`. An inherited digest must name the same data; a cached moon task must have inherited one. A selection without `fleet = true` is untouched. |
| Per check | `CCID_FLEET_KEYED=1` for a `fleet = true` check, removed for every other check. |
| Verdict key | A `fleet = true` check is never keyed without `CCID_FLEET_DIGEST`. Its declaration carries `"fleet": true`; a check without the field serializes exactly as before. |
| Moon tasks | cmnp keys the declared `CCID_FLEET_DIGEST` and forwards `CCID_FLEET_FILE` and `CCID_FORGE_HOSTS` unkeyed. |
| Consumers | Every reader (`ccid fleet show` and later consumers) loads `CCID_FLEET_FILE` and, inside a ccid check, requires `CCID_FLEET_KEYED=1` and `CCID_FLEET_DIGEST` equal to the digest of what it read: `this check reads fleet data; declare fleet = true on it in .ci/ccid.toml` or `fleet data changed after the check was keyed` otherwise (exit 2). |

A check script that reads `CCID_FLEET_FILE` itself bypasses that last guard; consumers go through
ccid.

## Commands

| Command | Result |
|---|---|
| `ccid fleet show` | One JSON line `{"fleet": <canonical bytes>, "digest": "<hex>", "source": "<path>", "runtime_forge_hosts": [...]}`; exit 0. Unavailable or invalid data, or a check without `fleet = true`: exit 2. |
| `ccid fleet validate --channel <file>` | `fleet data valid: <digest>`; exit 0. Invalid or missing fleet data: exit 2. |

`channel.json`'s own fleet data is validated by ccid's test suite.
