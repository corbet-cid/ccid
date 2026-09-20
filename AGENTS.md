# Working on ccid

ccid is public, generic CI tooling. Keep operator hostnames, paths, credentials,
topology and account configuration outside this repository. Preserve exact
source/tool identities, selected-check semantics and native coverage boundaries.
Do not install tools or add services as part of running a check. Keep manifests
thin and declarative; reuse authoritative repository scripts for special gates.
Changes to common runners require their meaningful standard-library tests on a
build worker. Never turn absent checks or missing tools into a passing result.
