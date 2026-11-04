# Filesystem synchronization benchmark

Run `cargo test --all-features --test integration fs::durability_benchmark -- --ignored --nocapture`
with clang, or run `durable-fs.jsl` in a Jess built with this branch on a target filesystem.
On Linux, `strace -c -e fsync,fdatasync,write ...` can collect observed system call counts.
The 64 transaction run invokes `syncData` 64 times for batch 1 and 4 times for batch 16;
actual system call counts have not been measured here (`strace` was unavailable).

Example run on Linux 7.2.9, tmpfs, debug build, 64 transactions per case. This measures
memory-backed filesystem overhead, **not persistent storage throughput or power-loss durability**.
Batch 16 has only four latency samples, so its percentiles are illustrative only.

| Bytes | Batch | Transactions/s | Sync p50 ms | p95 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4096 | 1 | 156356 | 0.003 | 0.007 | 0.030 |
| 4096 | 16 | 218125 | 0.004 | 0.006 | 0.006 |
| 65536 | 1 | 47276 | 0.002 | 0.003 | 0.004 |
| 65536 | 16 | 72270 | 0.002 | 0.003 | 0.003 |
| 1048576 | 1 | 4110 | 0.002 | 0.007 | 0.008 |
| 1048576 | 16 | 4739 | 0.005 | 0.007 | 0.007 |
