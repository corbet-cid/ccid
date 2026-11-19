# Portable file names (`ccid portable-paths`)

Owner decision (2026-11-19): enforce the limits of every system on every project, so a tracked name never causes a
problem on any of them. `ccid portable-paths` judges `git ls-files` (and `lstat` for symlinks) cheaply, with no file
contents read.

```sh
ccid portable-paths                         # report mode: prints the violations, always exits 0
ccid portable-paths --mode enforce          # exits 1 when a violation is not covered by the allowlist
ccid portable-paths --summary               # only the per-rule counts
```

## In every repository

A repository on the central channel (`render_schema` 2 or 3) runs the check without declaring it: ccid adds the check
`portable-paths` to the repository's `verify` job (when that job runs the declared checks), in report mode, so it never
fails CI. A repository switches to enforce with one line, once its report is empty:

```toml
[portable_paths]
mode = "enforce"      # default "report"
```

A repository that declares its own `[checks.portable-paths]` keeps it (it wins); a repository that pins its own ccid
(no `render_schema`) declares the command check itself: `commands = [["ccid", "portable-paths", "--mode", "report"]]`.
Adding the check changes the rendered adapters, so the release that brings it lists the previous releases in the
channel's `previous`.

## The rules

| rule | refused |
|---|---|
| `chars` | any component byte outside `[A-Za-z0-9._-]`: space, non-ASCII (which also ends NFC/NFD questions), `/ \ : * ? " < > \|`, control characters, invalid UTF-8 |
| `case` | two tracked paths, or two of their directories, equal when case is ignored |
| `reserved` | a Windows device stem with or without an extension: CON PRN AUX NUL COM0-9 LPT0-9 (and the superscript forms) |
| `trailing` | a component ending with a dot or a space |
| `leading-dash` | a component beginning with `-` |
| `component-length` | a component longer than 255 bytes |
| `path-length` | a repo-relative path longer than 200 characters |
| `symlink` | a tracked symlink |

Submodule entries (gitlinks) are pointers and are not judged.

## The allowlist

The only exception is the committed file `.ci/portable-paths.toml` (a data file of the CI surface). Every entry has a
`pattern` (a repo-relative glob: `*` and `?` inside one segment, `**` as a whole segment) and a `reason`, and may name
the `rules` it waives (default: all):

```toml
[[allow]]
pattern = "src/routes/**/+page.svelte"
reason = "SvelteKit routing convention"
rules = ["chars"]
```

Built-in presets switch on a whole convention with one line, `presets = ["sveltekit-routes"]`: it allows `+`, `[`, `]`,
`(` and `)` (and no other character) in names below a `routes` directory (`+page.svelte`, `[slug]`, `(group)`), and
nothing outside one.

An entry that matches nothing is noted on stderr so the list shrinks with the repository. An empty reason, an unknown
rule or an unknown key is an operational error.

## Exit status

`0` in report mode whatever is found, and in enforce mode when every violation is allowlisted; `1` in enforce mode
with an open violation; `2` for an operational error (not a git repository, unreadable or malformed allowlist), in
both modes.

The rollout starts in report mode for every repository; a repository switches to enforce when its report is empty.
