# Launch Timing

`axis-bench launch` times real `axis run` launches of one or more AXIS builds
from the outside, and asks each build to record where its time went from the
inside. It prints a table of median and p90 times per step and, for two or
more builds, the difference from the first build with a 95% interval. Use it
to find slow parts of a launch and to show that a change made one faster.

`runtime-metrics` ([runtime-performance-benchmarks.md](runtime-performance-benchmarks.md))
measures the sandbox library in-process; `axis-bench` measures the whole CLI
process, including its own start-up and exit, on any platform AXIS supports.

## Quick Start

Build the workspace, which puts `axis`, its helpers, `axis-bench`, and
`axis-bench-child` in `target/release`:

```bash
cargo build --locked --release
```

The MXC backend also needs its executor: on Linux, copy an `lxc-exec` next to
`axis` as [DEVELOPMENT.md](../DEVELOPMENT.md) describes; on Windows, AXIS finds
`wxc-exec.exe` next to `axis.exe` or where it is installed.

Time the build in the folder holding `axis-bench`:

```bash
target/release/axis-bench launch
```

Compare a build of the base commit with your change. Build both checkouts with
the same command, then pass both build folders; the first one is the baseline:

```bash
target/release/axis-bench launch --build ../axis-main/target/release --build target/release
```

On Windows:

```powershell
target\release\axis-bench.exe launch --build C:\src\axis-main\target\release --build target\release
```

`--build` takes a folder holding `axis` and its helpers, or the `axis` binary
itself. Each build is tagged with the name of the checkout holding it, here
`axis-main` and `axis-fork`; the tag heads its column and names its trace
folder. Choose tags with one `--tag` per `--build`, in the same order:

```bash
target/release/axis-bench launch --build ../axis-main/target/release --tag main --build target/release --tag mine
```

Pass the same folder twice to see how much two identical builds differ on
your machine; the second one is tagged with `-2` added.

## How Launches Run

- Each build is copied with the helpers beside it (`lxc-exec`,
  `axis-seccomp-launcher`, and `axis-netns-helper` on Linux; `wxc-exec.exe`,
  `wxc.exe`, and `mxc-exec.exe` on Windows) into a private directory, because
  AXIS on Linux refuses helpers under group- or world-writable directories.
  The directory defaults to `~/.cache/axis-bench` or
  `%LOCALAPPDATA%\axis-bench`; change it with `--stage-dir`. Helpers that AXIS
  finds elsewhere, such as an installed executor, are used as usual, and the
  executor each build used is printed.
- Every launch runs `axis --socket <unreachable> run --policy minimal -- <command>`
  in a fresh, empty directory with stdin closed. The unreachable socket makes
  AXIS skip the daemon at once; pass `--daemon-probe` to let it look as usual.
- The command is `axis-bench-child`, copied into that directory. It prints when
  its `main` started and when it was about to exit, so the table can tell the
  sandbox's start-up and teardown apart from the command. Run a real command
  instead with `-- <command> [args]`.
- Each build first runs `--warmup` unmeasured launches (default 2); the run
  stops if one fails. Then come `--runs` rounds (default 30). Each round
  launches every build once in a shuffled order, with `--pause-ms` (default
  500) before each launch. The shuffle seed is printed and saved; `--seed`
  repeats an order.
- A launch that fails or runs past `--timeout` seconds (default 60) is killed,
  recorded with its error, and left out of the statistics.

## Reading The Table

Two copies of the same build, tagged `a` and `b`, on a 32-CPU Linux host with
the MXC backend:

```text
                                    a                  b                           b - a
ms                    median      p90    median      p90    median          95% interval
axis start              1.69     2.14      1.67     1.86     -0.01        [-0.23, +0.10]
cli                     2.19     2.38      2.20     2.46     -0.00        [-0.13, +0.16]
sandbox                 0.21     0.24      0.20     0.23     -0.01        [-0.03, +0.01]
front_door              0.00     0.00      0.00     0.00     +0.00        [+0.00, +0.00]
support_files           0.05     0.06      0.05     0.06     -0.00        [-0.00, +0.00]
backend                 0.28     0.31      0.28     0.28     -0.01        [-0.01, -0.00] *
child_setup             0.10     0.11      0.10     0.10     -0.00        [-0.01, +0.00]
resource_setup          0.00     0.00      0.00     0.00     +0.00        [+0.00, +0.00]
spawn                   0.93     1.04      0.94     1.07     +0.01        [-0.03, +0.08]
post_spawn_handoff      0.27     0.34      0.27     0.31     -0.00        [-0.02, +0.03]
before command         11.87    12.45     12.09    13.02     +0.69        [-0.41, +1.12]
command                 0.00     0.00      0.00     0.00     +0.00        [+0.00, +0.00]
after command           5.91     6.79      5.79     7.96     +0.16        [-1.06, +0.88]
cleanup                10.26    10.54     10.26    10.55     +0.02        [-0.04, +0.12]
axis exit               0.94     1.13      0.94     1.28     -0.02        [-0.12, +0.06]
untraced                0.09     0.12      0.08     0.10     -0.02        [-0.02, +0.00]
----------------------------------------------------------------------------------------
total                  34.88    36.12     35.02    36.01     -0.07        [-0.56, +0.48]
```

Each row is the time spent in one step on the main thread of `axis`, not
counting the steps nested inside it, so the rows of one launch add up to its
total. Rows group steps by the first part of their name
(`backend.preflight.executor` counts under `backend`); `--depth 2` or more
splits them further. The output ends with a description of each row.

Rows that need a word:

| Row | Meaning |
| --- | --- |
| `axis start` | from spawning `axis` to its `main` |
| `before command` | AXIS waiting while the sandbox starts the command, until the command's `main` |
| `command` | the command itself |
| `after command` | from the command's exit until AXIS sees the sandbox exit |
| `axis exit` | from AXIS writing its timing file to the process exiting |
| `untraced` | main-thread time outside every step; above 5% of the total, a step is missing a span |
| `total` | spawn to exit, measured by `axis-bench` |

`median` and `p90` (nearest rank) are per build. The difference column is the
median of the per-round differences (`b` minus `a` in the same round), with a
distribution-free 95% interval for it from the sign test. A `*` marks an
interval that excludes 0: the difference is larger than the noise between
rounds. Each row is a separate test, so with many rows an occasional `*` is
chance, like `backend` above. Run a build against itself first to learn how
large a difference your machine can show by chance. Fewer than 6 rounds give
no interval.

Builds without timing support, such as releases older than this tool, still
get `total` and the before command, command, and after command split from the
child's timestamps. `--no-timing` or `--timing-off TAG` measures a build
that way on purpose, for example to check that timing adds no cost. Those
coarse rows mean something different from the same rows of a build with
timing, so rows other than `total` are compared only between builds that both
have timing or both do not. On the host above, timing off showed no
difference from the base commit, and timing on added less than 1 ms (the
interval for `total` was [-0.41, +0.83] ms).

The table warns when builds used different backends, since their rows are not
comparable, and when launches failed.

## Saved Runs

Every run is saved under `--output` (default `bench-runs`) as
`<UTC time>_<name>/`:

| File | Contents |
| --- | --- |
| `manifest.json` | settings, seed, host, CPU load during the rounds, and each build's files with sizes and SHA-256 |
| `samples.jsonl` | one line per measured launch: build, round, exit code, error, timestamps |
| `summary.json` | the table at depth 1 |
| `traces/<tag>/<round>.json` | the launch as `axis-bench` saw it, the command as it reported itself, and every step AXIS recorded |

Open a trace in [Perfetto](https://ui.perfetto.dev) or `chrome://tracing` to
see one launch on a timeline, including steps on other threads.

`axis-bench show <run-dir>` prints a saved run's table again; add
`--depth 2` or more to split the rows further without launching anything.

`axis-bench compare <run-dir> <run-dir>` prints the table for one build of
each saved run (`--before-tag` and `--after-tag` pick them; by default the
first build of each). Separate runs are not paired, so it shows the difference
of the medians without an interval, and it warns when the runs used different
hosts, policies, or commands. For a trustworthy difference, measure both
builds in one `axis-bench launch`.

## Timing Inside AXIS

`axis` records timing only when `AXIS_TIMING_FILE` names a file; otherwise
every timing call returns at once. On exit it writes the file in the Chrome
Trace Event format: step names, start times and durations, process and thread
ids, and a few facts such as the backend and the executor path. It never
records arguments, environment variables, or policy contents.

Steps come from `axis_core::timing`:

```rust
let _span = axis_core::timing::span("support_files.mxc_config");
write_private_config(&path, &config)?;
```

A span ends when its guard is dropped. Every startup phase recorded through
`record_startup_result` is already a span. Name new spans `group.step`, using
an existing group where one fits, so they land in the right row. Only spans on
the main thread become rows; spans on other threads appear in the trace only.
Code that ends the process with `std::process::exit` must call
`axis_core::timing::finish()` first, because exiting skips the code that
writes the file.

## Limits

- Only `axis run` is timed; the daemon path is not.
- The `vxn` backend does not mark when it reaps the child, so its time after
  the command's exit shows under `after command` instead of `sandbox`.
- CPU load during the rounds is reported on Linux and Windows only.
