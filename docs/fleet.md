# Fleet data: first-party owners on the release channel

Some checks need to know which source owners belong to the fleet: which dependency URLs are
first-party, and which legacy spellings of an owner moved where. That list is **fleet data**. It
exists once, as the `fleet` member of the release channel (`channel.json` on ccid's main branch),
which every runner already fetches to pick its ccid release. It is never compiled into ccid and
never copied into a repository. **An org move is an edit of `fleet` in `channel.json`**: no ccid
rebuild, no repository change. The next run of every `fleet = true` check misses its cached
result once (the digest changed); nothing else is invalidated. The code is `src/fleet.rs`.

## Schema

```json
"fleet": {
  "schema": 1,
  "first_party": ["https://forge.example/org/", "https://forge.example/other-org/"],
  "aliases": {"https://mirror.example/org/": "https://forge.example/org/"}
}
```

| Field | Meaning |
|---|---|
| `schema` | `1`. Any other value is refused; unknown fields are refused. |
| `first_party` | Canonical owner prefixes, strictly ascending (byte order), no duplicates, at least one. |
| `aliases` | Optional. Legacy owner prefix → the canonical prefix it moved to. |

Every prefix, alias key and alias value is a **prefix**: a URL that parses, uses `https`, has no
user name, password, port, query or fragment, has a non-empty lowercase host, ends with `/`, and
whose path is exactly `/<owner>/` with `<owner>` of `[A-Za-z0-9._-]` (not `.` or `..`). The
string must be the URL's canonical spelling (no default port, no surrounding whitespace, no case
or escape variants), because matching compares bytes. Every alias value is a `first_party`
entry; no alias key is one. Errors name the offending entry.

A URL is classified by byte prefix after removing one leading `git+`: **canonical** when it
starts with a `first_party` prefix, **alias** when it starts with an alias key, otherwise outside
the fleet. The trailing `/` of every prefix keeps `https://forge.example/org-other/` out of
`https://forge.example/org/`; a URL with a user name or another scheme matches nothing.

The **digest** is the lowercase SHA-256 of the canonical bytes: the validated data serialized in
field order (`schema`, `first_party`, `aliases` ordered by key). Two channel files that differ
only in key order or whitespace have the same digest.

## Where the data comes from

| Context | `CCID_FLEET_FILE` |
|---|---|
| Forgejo job (`ccid job-run`) | Set by `job-run` when neither the runner nor the job names it: the runner's `CCID_CHANNEL_FILE` when set, else the shim's copy `${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/ccid-channel/last-good.json`. It must be a regular file whose fleet data is valid; `job-run` prints `fleet data <digest> from <path>`, or that the channel carries no fleet data and continues. It never sets `CCID_CHANNEL_FILE`. |
| Land gate and other lanes | The caller exports a fetched copy of `channel.json`. |
| Anything else | Whatever the caller names. Without it, `fleet = true` checks fail closed. |

`CCID_FLEET_FILE` must name a regular file (a symbolic link is refused). Its location is never
part of a key; its content is, through the digest.

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
| Consumers | Every reader (`ccid fleet show`, the first-party and alias consumers) loads `CCID_FLEET_FILE` and, inside a ccid check, requires `CCID_FLEET_KEYED=1` and `CCID_FLEET_DIGEST` equal to the digest of what it read: `this check reads fleet data; declare fleet = true on it in .ci/ccid.toml` or `fleet data changed after the check was keyed` otherwise (exit 2). |

A check script that reads `CCID_FLEET_FILE` itself bypasses that last guard; consumers go through
ccid.

## Commands

| Command | Result |
|---|---|
| `ccid fleet show` | One JSON line `{"fleet": <canonical bytes>, "digest": "<hex>", "source": "<path>"}`; exit 0. Unavailable or invalid data, or a check without `fleet = true`: exit 2. |
| `ccid fleet validate --channel <file>` | `fleet data valid: <digest>`; exit 0. Invalid or missing fleet data: exit 2. |

`channel.json`'s own fleet data is validated by ccid's test suite.
