# copse

**Faster Git worktrees.** copse makes new worktrees by copy-on-write cloning an existing one instead of checking every file out of Git. That's faster than `git worktree add` even for the checkout alone. And because the clone includes ignored files, each new worktree also comes with dependencies installed and build output in place.

```
$ copse new feature-a
✓ cloned template (38684 files, 664 MB) in 2.1s  [refs-blockclone], 878 files repointed from the template
✓ checked out new branch feature-a at ef295d54d564 (0 files changed vs template)
✓ git status clean
→ E:\dev\vite.copse\feature-a  (2.4s total)
```

Use it anywhere you'd reach for `git worktree add`, especially when you make lots of worktrees: one per agent, per PR review, per experiment. There, checkout + `pnpm install` + a build every time adds up.

> **Status: early.** Works end to end on Windows with a ReFS Dev Drive. On Linux and macOS it works, but copies files instead of cloning them, so it's slower (copy-on-write support is next). Expect rough edges and breaking changes.

## How it works

copse keeps one **template**: a worktree of your repo that `copse warm` keeps on your base branch, with setup (e.g. `pnpm install`, a build) already run. `copse new <name>` then:

1. Registers a new worktree with `git worktree add --no-checkout`. Nothing is written yet.
2. **Clones the template's files** into it, ignored ones included (`node_modules`, `target/`, build caches), using copy-on-write where the filesystem supports it. Cloned files share disk blocks with the template until either side changes them.
3. Copies the template's Git index. Files keep their timestamps, so Git trusts the index without rehashing every file.
4. Runs `git checkout -b <name> [<from>]`, which rewrites only the files that differ from the template's commit.
5. Repoints absolute template paths that tools wrote into files (pnpm's `.bin` shims and state files by default), then runs your `post_create` hooks.

The result is a normal Git worktree: it shows up in `git worktree list`, and it shares objects, branches and config with your repo.

## Install

From source (needs Rust and Git 2.38+):

```sh
git clone <this repo> && cd copse-cli
cargo install --path cli
```

## Quick start

```sh
cd my-repo
copse doctor          # check clone support, same-volume setup, Git version
copse init            # write .copse.toml (setup guessed from lockfiles) and warm the template
copse new feature-a   # a ready workspace at ../my-repo.copse/feature-a
copse list
copse rm feature-a --delete-branch
```

Run `copse warm` when your base branch moves, so new workspaces start from fresh dependencies.

## Commands

| Command                                                     | What it does                                                                                                                                |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `copse init [--setup <cmd>]... [--no-warm]`                 | Write `.copse.toml`, create the workspaces dir, check clone support, warm the template.                                                     |
| `copse warm [--no-fetch]`                                   | Fetch, move the template to `origin/<base>` (keeping installed files), run `setup`. `new` refuses a template whose last warm didn't finish. |
| `copse new <name> [--from <ref>] [-b <branch> \| --detach]` | Create a workspace. Warns if a lockfile differs between the template and `--from`.                                                          |
| `copse list`                                                | Template state, then each workspace's branch, commit and number of changes.                                                                 |
| `copse rm <name> [--force] [--delete-branch]`               | Remove a workspace. Refuses if it has uncommitted or untracked changes unless `--force`.                                                    |
| `copse doctor`                                              | Filesystem, clone support, same-volume check, file-write speed (antivirus), Git version, long paths.                                        |

## Configuration

`.copse.toml` at the root of your main worktree. Every field is optional.

```toml
# Must be on the same volume as the repo for copy-on-write. Default: ../<repo>.copse
workspaces_dir = "../my-repo.copse"
# Branch the template tracks.
base = "main"

[warm]
# Run in the template by `copse warm`. Should only write ignored files.
setup = ["pnpm install --frozen-lockfile", "pnpm run build"]
# Never cloned into workspaces.
exclude = ["**/.cache/tmp", "*.log"]

[new]
# Run in each new workspace after checkout.
post_create = []
# Extra files whose absolute template paths get rewritten (pnpm's are built in).
fix_paths = [".venv/pyvenv.cfg"]
```

## Platform support

| Platform                  | Cloning           | Notes                                                       |
| ------------------------- | ----------------- | ----------------------------------------------------------- |
| Windows, ReFS / Dev Drive | Block cloning ✓   | The main target today.                                      |
| Windows, NTFS             | Full copy         | Works, but slow and uses full disk space. `doctor` says so. |
| macOS (APFS)              | Full copy for now | `clonefile` support planned.                                |
| Linux (Btrfs, XFS)        | Full copy for now | Reflink support planned.                                    |

The template and the workspaces must be on the **same volume**: copy-on-write doesn't work across volumes.

On Linux and macOS, a cloned file always gets a new inode number and change time, which Git would otherwise compare against the copied index. So `copse new` sets `core.checkStat=minimal` and `core.trustctime=false` in each workspace's own config (via `extensions.worktreeConfig`). Your main worktree and the template keep Git's full checks.

### Windows setup

- **Use a Dev Drive** (ReFS) for the repo and workspaces. NTFS can't block-clone.
- **Defender is the biggest factor.** Real-time scanning of each new file is what limits copse's speed. With it on, `copse new` adapts by cloning every file (cloned files cost the scanner far less), but it's still about 4× slower than with an exclusion. Exclude the Dev Drive (or the workspaces folder), or turn on Dev Drive performance mode. `copse doctor` measures this and tells you.
- Turn on long paths for deep `node_modules`: `git config --global core.longpaths true`.

## Benchmarks

Windows 11, ReFS Dev Drive excluded from Defender, Git 2.56. Harness in [`bench/`](bench/).

| Repo                                                | Plain Git                                                                          | `copse new`                  |
| --------------------------------------------------- | ---------------------------------------------------------------------------------- | ---------------------------- |
| vite (38,685 files, 664 MB after install + build)   | `worktree add` + `pnpm install` + build: **~10.2s** (24.3s with a cold pnpm store) | **2.4–3.5s**, ready to build |
| linux (97,358 tracked files, 1.7 GB, checkout only) | `worktree add` + `status`: **~6.1s**                                               | **~4.0s**                    |

Vite is a fast case: pnpm hard-links from its store, so install takes seconds. The slower a repo's install and build, the bigger the gap. With Defender scanning E: (no exclusion), the Linux checkout-only case is about 2× _slower_ than Git, because copse is unsigned and Defender scans its writes far more heavily than `git.exe`'s.

## Things to know

- **Keep the template clean.** Only `copse warm` should touch it. Setup commands should only write ignored files; anything else shows up as changes in every workspace.
- **Lockfile changes:** if `--from` changes a lockfile vs. the template, copse warns you. Re-run setup in that workspace, or `copse warm` if the base moved.
- **Absolute paths in tool output** (Python venvs, some build caches) can still point at the template. Add those files to `new.fix_paths`, or rebuild them in `post_create`.
- **Case-colliding file names** (e.g. in the Linux kernel) can't coexist on Windows. They show as changes in the template and every workspace, just as with a plain checkout.

## Repository layout

| Path     | Contents                                                                                         |
| -------- | ------------------------------------------------------------------------------------------------ |
| `cli/`   | The `copse` binary: clone layer (`src/clone`), commands (`src/commands`), config, Git wrapper.   |
| `bench/` | Timing harness: `m0` (plain Git baseline) and `m1_files` (per-file write strategies on Windows). |

## Roadmap

- Copy-on-write on macOS (`clonefile`, including whole-directory clones) and Linux (reflink).
- Write each new file's inode and change time into the copied index, so Unix workspaces need no relaxed stat settings.
- `snapshot` / `diff` between workspaces, and `status` / `sync` with upstream changes and `git merge-tree` conflict prediction.
- Examples for agent workflows: one workspace per task.
