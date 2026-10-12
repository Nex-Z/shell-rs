# Personal integrated desktop baseline

`personal/integrated-desktop` preserves the combined preview implementation and its tested local fixes for continued maintenance. It starts at `preview/files-gpu-package`, commit `049db8e7735750110abe761e4f5098c290ff1536` (version `0.1.10-preview.2`). It retains preview isolation and packaging configuration; it is not a new release.

## Upstream relationship

The initial maintenance snapshot was `58b72ff172d6e62e2f09fbae56e466fed19c2757`. The source preview commit was 10 commits ahead and 5 behind upstream `since2006/shell-rs:main` at `0dfa3fdf77388aae1e82147c040058f09c9d032f`.

The subsequent selective sync imports:

- `e3397dabc792684ac4f20ce3c14dc624c09a9e80`: changed SSH host-key warning, old/new fingerprints, explicit update or cancel, non-interactive refusal, and concurrent trust-update checks.
- The final Windows resource effect of `959e0b1d9de8ac2519008fcc9ca9a2f69b2b5664` and `ff1efaf15172b31485275fd3e17167740cb2b724`: `OriginalFilename=ShellRS-Xshell-WinSCP.exe`.

This is a selective source sync, not an ancestry merge. Git can still report all five upstream commits as not merged. The two release/version-only commits `bd7b8c1` and `0dfa3fd` are intentionally excluded. `0.1.10-preview.2`, preview data/credential/known_hosts isolation, disabled preview updater, packaging, and workflows are retained.

### Sync validation

On Windows x64 with Rust 1.98.1 MSVC and the `preview` feature, formatting and Clippy (`--all-targets -- -D warnings`) passed. Targeted tests passed: SSH 75, CLI 61, forwarding 46, data paths 2, updater 53, terminal engine 36, connection UI 19, file-sidebar UI 22, CLI binary integration 4 (318 total).

Added local-fixture regressions cover a concurrent known_hosts update, closing a terminal with a pending trust question, changed-key refusal without hanging in CLI and automatic forward reconnect, changed-key jump-host identification/cancellation, and UI Cancel/Escape. Existing tests cover accepting/rejecting a changed key and replacing an existing temporary known_hosts file on Windows. The normal production path remains `app::data_dir()/known_hosts`; preview data selection and credential namespaces are unchanged.

The test-built Windows executable reports `ShellRS-Xshell-WinSCP.exe` and version `0.1.10-preview.2`. No installer or release package was rebuilt, no real remote endpoint was used, and no real trust file was changed. These are targeted Windows results, not a full cross-platform pass; Unix-only tests and real bastion-launcher acceptance remain unverified. CI status must be checked separately for the exact pushed commit.

The branch combines the existing automatic host naming, file sidebar, GPU monitoring, connection status/Enter reconnect, terminal compose pane, and optional Bash directory following. It also records the path-input blur fix, focus-independent reconnect routing, Bash setting help text, and their regression tests.

## Focused upstream submissions

These are extracted, independently maintained submissions, not commits merged into this branch. Their implementations and test layout can differ where needed to fit upstream main. Do not cherry-pick all of them into this branch: the combined features already exist here.

| Submission | Fork branch | Submitted commit |
| --- | --- | --- |
| [#18 File sidebar](https://github.com/since2006/shell-rs/pull/18) | `review/single-file-sidebar` | `7128f59086b2effd568db8b03e0994c3bf2342dc` |
| [#19 GPU monitor](https://github.com/since2006/shell-rs/pull/19) | `review/linux-gpu-monitor` | `7cbfcdfb46ed4535a1e7dd49f125f66e11ad5a3f` |
| [#20 Connection state and reconnect](https://github.com/since2006/shell-rs/pull/20) | `review/terminal-connection-state` | `4d6752d1af3c0a7ce93732a7b772fc36c6cb4a7f` |
| [#21 Compose pane](https://github.com/since2006/shell-rs/pull/21) | `review/terminal-compose` | `05a83750658e678ab39f2486dcf75f6a87ba5a7f` |
| [#22 Bash directory following](https://github.com/since2006/shell-rs/pull/22) | `review/bash-directory-follow` | `419510ae1ed5293ceebdf48d2a50e074ddcac3a1` |

#22 includes the #18 sidebar dependency. The earlier automatic host naming submission is [#16](https://github.com/since2006/shell-rs/pull/16), from `feat/host-name-from-address` at `ccf61bf0d02350ea54400d8613bbcde689cb8fbd`.

## Maintenance

- Make future integrated changes on this branch and push ordinary commits. Keep focused PR branches separate.
- Review upstream updates on a separate integration branch/worktree before bringing them into this baseline. Reconcile features already accepted upstream to avoid duplicate implementations.
- Keep build outputs, caches, logs, local data, and generated test captures outside commits. Existing preview artifacts are not rebuilt or revalidated by this source-backup commit.
- Run the repository's applicable checks after future code changes. This baseline backup itself does not claim a fresh build or a full cross-platform test pass.
