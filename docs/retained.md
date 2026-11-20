# The retained store (`ccid retained`)

Some CI captures cannot be rebuilt: a recorded proof, a measured fixture, a native worker capture. They used to live in a
per-repository namespace (`ccid-cached/<sha256 of the repository URL>/release-artifacts/...`), so every consumer pinned
another repository's namespace hash, and an organisation move or a URL change broke every reader.

The retained store is content-addressed and belongs to no repository:

```
$CI_RETAINED_ROOT/<kind>/<digest>/content        the capture (a file, or a directory tree)
$CI_RETAINED_ROOT/<kind>/.alias/<old digest>     an earlier scheme's digest naming the same capture
```

`ccid job-run` exports `CI_RETAINED_ROOT` (`$CI_CACHE_ROOT/retained` unless the runner sets it). Writers write by digest,
readers read by digest and nothing else.

```sh
ccid retained put --kind record-fixture ./out/fixture      # prints the digest; write-once, read-only
ccid retained get --kind record-fixture --digest <digest>   # prints the verified path
ccid retained get --kind record-fixture --digest <digest> --into ./fixture
ccid retained verify [--kind record-fixture]                 # every entry still digests to its name
```

* The digest of a file is its sha256. The digest of a directory is the sha256 of the sorted lines `<relative path>\0<file
  sha256>\n` of its regular files; a symlink or special file is refused.
* `put` copies into a temporary sibling and renames it into place. An entry that exists is verified and never replaced.
* `get` verifies the digest first: a damaged entry is an error, never a silent wrong answer.
* `put --alias <old digest>` lets an existing consumer keep asking for the digest it already pins; an alias names one
  entry only.
* Seeding from the old namespaces is a COPY: `put` never moves or deletes its source.
