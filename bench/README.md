# bench

Timing harness for copse. Kept separate from `cli/tests/`: the tests check correctness, these scripts measure time. Written in Rust so the same harness runs on Windows, macOS and Linux.

## M0: baseline (no copse yet)

Times each phase of making a usable workspace with plain Git:

1. `git worktree add` (detached, at a pinned commit) with parallel checkout (`-c checkout.workers=0`, one worker per CPU). This is the fastest plain Git can do, and copse only matters if it beats it.
2. dependency install (`pnpm install`, `cargo fetch`, ...)
3. build
4. `git status` on the result

It also records the file count and size after the build, which is what copse will have to clone.

```sh
cargo m0 --repo ripgrep --runs 1        # smoke test first
cargo m0                                # all repos, 3 runs each
cargo m0 --workdir D:/copse-bench       # measure a different volume
cargo m0 -v                             # stream command output
cargo m0 --help
```

`cargo m0` is an alias (`.cargo/config.toml`) for `cargo run -p copse-bench --release --bin m0 --`. The default workdir is `~/copse-bench`.

Results go to `results/<date>-<host>.csv` (one row per repo × run × phase) and a `.md` summary with medians. Commit both.

### Repos
Defined in `repos.toml`: URL, **pinned commit**, install command and build command. Install and build run through `cmd /C` on Windows and `sh -c` elsewhere.

| name | why |
|---|---|
| ripgrep | small Rust repo, smoke test |
| vite | JS/TS pnpm monorepo, heavy `node_modules` |
| nushell | Rust workspace, heavy `target/` |
| linux | checkout-only stress test (~90k tracked files). No install/build, shallow fetch of the pinned commit |

`install` and `build` are optional. Leave them out to time only `worktree add` + `git status`. Set `shallow = true` to fetch just the pinned commit instead of the full history. If a fresh checkout isn't clean (e.g. case-colliding or reserved names on Windows), `m0` prints a warning, because the timings are then suspect.

### Notes for reading results
- **Run 1 vs later runs:** the source clone happens once and isn't timed. Package caches (pnpm store, cargo registry) are warm after run 1, so later installs show the realistic "developer machine" case. Run 1 also includes downloads.
- **Filesystem:** recorded per run (best effort). The baseline doesn't need copy-on-write. M1 (block cloning) needs a ReFS **Dev Drive** on Windows, with the workdir on it.
- **Size** is apparent size and doesn't follow symlinks or junctions. Hardlinked files (e.g. from the pnpm store) are counted at full size.
- **Antivirus / indexing** affect install and build heavily on Windows. Note the Defender settings in the results commit message.
- Requirements: `git` 2.38+, `cargo`, `pnpm` (for vite).

## Later
- A `copse new` column using the same repos, for the "X minutes → Y seconds" headline (docs/12-cli-v1.md §8).
