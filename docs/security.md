# Security and dependency work made visible

Operating model R47: what we do for a project (advisories checked, fixes, updates) is shown to its
users on every forge. ccid renders it from data committed in the repository, at `render_schema = 3`:

| Where | What | How |
|---|---|---|
| `README.md` | a short status block | between marker lines, after the first `# ` title |
| `SECURITY.md` | the policy and the advisory table | between marker lines, at the end (a new file is created) |
| the release notes | a "Security and dependencies" section | appended by `ccid release` to the notes of the draft and of the public release |
| `ccid security plan` | the requests that would publish the project's own advisories | JSON on stdout; nothing is sent |

A repository gets all of it with one `ccid render`. A repository that lists upstream CI files in
`[surface] upstream` (a fork, a contributed project) gets nothing (R10, R11), and `render_schema = 2`
and below render exactly as before.

## Two kinds of advisory, kept apart

* **Advisories in dependencies.** Found by the daily fleet check (R49) against the OSV database and
  recorded in the ledger `.ci/security.toml`. They are shown (README, SECURITY.md, release notes) and
  **never published** through a forge's advisory feature: they are not vulnerabilities in our project.
* **Advisories for the project's own code.** A person writes them in `.ci/own-advisories.toml` after a fix
  has shipped. Only these appear in `ccid security plan`.

A dependency advisory that shipped inside one of our releases is a judgment call, not an automatic
mapping: if a person decides it deserves a published advisory of its own, they write an own advisory for
it by hand.

## The ledger `.ci/security.toml`

Written by the daily fleet check, **only when an advisory appears or changes state** (R49: at most one
update change per repository per day, and only where something changed). It never holds a "last scanned"
date; a status that moved every day would be a commit every day. The update change that writes it also runs
`ccid render`, in the same commit, or `ccid render --check` is red after the merge.

```toml
schema = 1

[[advisory]]
id = "GHSA-xxxx-xxxx-xxxx"        # the OSV id
aliases = ["CVE-2026-0001"]       # optional
package = "hyper"
ecosystem = "crates.io"           # the OSV ecosystem name
affected = "0.14.10"              # the locked version the advisory was found against
fixed = "0.14.11"                 # the first fixed version; required for state = "fixed"
severity = "high"                 # unknown | low | moderate | high | critical
found = "2026-11-10"              # YYYY-MM-DD, the day the daily check first saw it
resolved = "2026-11-11"           # YYYY-MM-DD; required for "fixed", refused otherwise
state = "fixed"                   # open | fixed | accepted
reason = "..."                    # required for "accepted", refused otherwise
```

* One entry per `(id, package, affected)`; a duplicate is refused.
* A `fixed` entry stays: it is the history the status counts. Removing it makes the fix invisible.
* `accepted` is a reviewed decision to keep an affected version (for example a `cargo deny` exception
  that is still open). `reason` is one plain line.
* Unknown keys are refused (a typo must not silently drop a field), and so is any other `schema`.

### What the writer must honour

* **Plain values only.** Every value that reaches a Markdown file is checked against a strict character
  set and a value outside it is refused with exit 2, never escaped: ids `[A-Za-z0-9._:-]`, package names
  `[A-Za-z0-9._@/:+~-]`, versions `[A-Za-z0-9._+~:-]`, `reason` letters, digits, spaces and `.,;:'()/+_-`
  (200 characters). The OSV `summary` and `details` are free text from outside and have no field: the
  rendered pages link to `https://osv.dev/vulnerability/<id>` instead.
* **Dates** are real calendar days, `resolved` is not before `found`.
* **Order does not matter**; the renderer sorts. Write the file with a stable order anyway, so that a
  re-run that changes nothing changes no byte.
* **A repository whose channel ccid predates this one** reads `.ci/security.toml` as an unknown file under
  `.ci/` and refuses its render. Write the ledger only where the channel's ccid knows it.

## What is rendered

* **README block.** Two short paragraphs, no headings: the process in one sentence, a link to the policy
  file the repository really has, and the counts (`1 open (...), 1 accepted after review, 14 fixed`, or
  `none`). A repository without a `README.md` gets no block and no README.
* **SECURITY.md block.** The process (R48: automatic updates, merged on green; an update with red tests is
  handed to the maintainers' agents and stays visible as an issue), the table of advisories in dependencies
  (open and accepted first, then the 25 most recent fixed; the rest stay in the ledger), the table of
  advisories for the project, and how to report a vulnerability. The target is the file GitHub reads:
  `.github/SECURITY.md`, else `SECURITY.md`, else `docs/SECURITY.md`; none exists: a root `SECURITY.md` is
  created with the title `# Security policy`.
* **Release notes.** The section is built from the data at the released commit alone, so the draft and the
  promotion (which may run in different jobs) produce the same text: open and accepted advisories, the
  count of fixed ones with the three most recent, and the project's own advisories.

The policy names **no contact address** (the owner's decision of 2026-10-24: a published address only
attracts spam). Reports go through the forge's own private channel: GitHub's "Report a vulnerability",
a confidential issue on GitLab (never relayed to another forge, R50). The Forgejo and Bitbucket copies have
no private channel and point to the GitHub or GitLab copy.

### The managed block

A person owns both files; ccid owns the text from the line `<!-- ccid:security:begin ... -->` to the line
`<!-- ccid:security:end -->`. A render replaces what is between them and changes nothing else. A file with
no block gets one (README: after the first `# ` title, outside front matter, comments and code fences; a
README without a title: at the top of the body; SECURITY.md: at the end). Marker lines inside a fenced code
block are not markers. One begin line and one end line, in that order, or none: anything else is refused
with a message that names the file. CRLF files keep their line endings, a BOM stays first, and a render
keeps the file's permissions.

`ccid render --check` calls a block that differs from the data **drift**. A file that has no block at all
is a declared transition (like any file a new ccid release introduces): the check passes, says so on
stderr and lists the file under `transitional`; the next render adds the block. Remove that tolerance once
the fleet has been rendered.

## `.ci/own-advisories.toml` and `ccid security plan`

```toml
schema = 1

[[advisory]]
key = "bad-parse-2026-11"        # local slug; the forge assigns its own id
summary = "One plain line, at most 200 characters"
description = "The text the forge shows. A person reads it before it is ever sent."
severity = "moderate"            # low | moderate | high | critical (never unknown)
ecosystem = "rust"               # GitHub's names: actions composer erlang go maven npm nuget other pip pub rubygems rust swift
package = "ccvl"                 # the project's own package
affected = ">= 0.2.0, < 0.3.5"
patched = "0.3.5"
found = "2026-11-03"
resolved = "2026-11-06"          # when the fixing release shipped
cve = "CVE-2026-12345"           # optional
```

`ccid security plan [--repo .]` prints, per forge, whether a free advisory feature exists and what it
would send. **It sends nothing**, reads no network, and holds no credential.

| Forge | Feature | Free | Published by this plan |
|---|---|---|---|
| GitHub | repository security advisories | yes | yes: `GET` (look for an existing one), `POST` (draft), `PATCH` (`state: published`) |
| GitLab | no published project advisory on a free plan. A maintainer of a public project can request a CVE id with a confidential issue in `gitlab-org/cves` (by hand, per vulnerability). A report goes in a confidential issue | - | no |
| Bitbucket | none (Cloud has no advisories and no issues) | - | no |
| Forgejo 17.0.1 | none (no advisory API) | - | no |

The placeholders `{owner}`, `{repo}` and `{ghsa_id}` are filled by the applier, which is not part of
ccid. Before the first live advisory, an applier must: check that the copy is a **public** repository
(a published advisory is public), use the relay identity's credential with the repository security
advisories permission only (GitHub accepts a draft only from an administrator or security manager of the
copy, so that role must exist first), send one repository at a time under the GitHub pacing policy, and stop
on the first refusal. The one-time repository setting that makes the policy's reporting route work, private
vulnerability reporting (`PUT /repos/{owner}/{repo}/private-vulnerability-reporting`), belongs to the copy
settings plan.

## Commands

```
ccid render [--check]        # keeps the blocks; reads .ci/security.toml and .ci/own-advisories.toml
ccid security plan [--repo .] # JSON; sends nothing
```

Exit codes: a malformed ledger, own-advisory file or block stops the render with exit 2 and writes nothing.
