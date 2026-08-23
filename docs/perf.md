# Performance benchmarks

Handfast's daemon (`handfastd`) runs continuously in the background on a user's
desktop. Its idle resource footprint is therefore a hard product requirement,
not just an optimization goal: the daemon should be effectively invisible next
to the compositor and other desktop services.

## Idle resource targets

| Metric              | Target        | Measured by                                    |
| ------------------- | ------------- | ---------------------------------------------- |
| Peak RSS (idle)     | < 40 MB       | `VmRSS` in `/proc/<pid>/status`, 30 s window   |
| Average CPU (idle)  | < 1 %         | `utime + stime` delta in `/proc/<pid>/stat`    |

Notes:

- "Idle" means the daemon is running with no paired devices connected — the
  steady state for most of a session.
- RSS is sampled every 500 ms for 30 seconds; peak is the maximum sample and
  average is the mean of all samples.
- CPU% is normalized to wall-clock time of the sampling window, so values are
  comparable across machines regardless of core count.

## Running the benchmark

From the repository root, on Linux (the script reads `/proc`):

```sh
./scripts/perf.sh
```

The script:

1. Builds `target/release/handfastd` with `cargo build --release -p handfastd`
   if the binary is not already present.
2. Starts `handfastd` in the background using a throwaway temp directory for
   its IPC socket and state DB, so your real profile is never touched.
3. Samples RSS every 500 ms for 30 seconds.
4. Prints peak RSS, average RSS, and average CPU%.
5. Stops the daemon.
6. Compares results against the targets above.

Exit code is `0` when both targets are met and `1` when either target is
missed (or setup fails), so it can gate CI or release checks directly.

## Baseline results

Fill this table after the first measurement run. One row per commit whose
resource profile changes materially.

| Date       | Commit | Peak RSS (MB) | Avg RSS (MB) | Avg CPU (%) | Result |
| ---------- | ------ | ------------- | ------------ | ----------- | ------ |
| _pending_  |        |               |              |             |        |
