# Personal integrated desktop baseline

`personal/integrated-desktop` preserves the combined preview implementation and its tested local fixes for continued maintenance. It starts at `preview/files-gpu-package`, commit `049db8e7735750110abe761e4f5098c290ff1536` (version `0.1.10-preview.2`). It retains preview isolation and packaging configuration; it is not a new release.

## Upstream relationship

At creation, upstream `since2006/shell-rs:main` was `0dfa3fdf77388aae1e82147c040058f09c9d032f`. The source preview commit was 10 commits ahead and 5 behind that main. This maintenance commit does not merge those 5 upstream commits. In particular, the latest upstream host-key update flow and Windows Xshell/WinSCP compatibility changes are not included merely by creating this branch.

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
