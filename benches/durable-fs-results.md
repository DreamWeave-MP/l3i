# Filesystem synchronization benchmark

Run `cargo test --all-features --test integration fs::durability_benchmark -- --ignored --nocapture`
with clang, or run `durable-fs.jsl` in a Jess built with this branch on a target filesystem.
On Linux, `strace -c -e fsync,fdatasync,write ...` can collect observed system call counts.
The 64 transaction run invokes `syncData` 64 times for batch 1 and 4 times for batch 16;
the container run below measured the actual system calls.

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

A second run used a disposable CentOS Stream 9 container with a host-built debug test binary
and host libraries mounted read-only. The container scratch directory was on overlayfs. With
`strace -f -c -e trace=write,fsync,fdatasync`, all six cases together made 204 `fdatasync`
syscalls and 363 `write` syscalls (including test harness output). The traced transaction
rates for batch 1 / batch 16 were 27,634 / 83,659 at 4 KiB, 22,906 / 38,870 at 64 KiB,
and 8,205 / 9,571 at 1 MiB. Batch 1 sync p50/p95/p99 in milliseconds were
0.028/0.061/0.073, 0.014/0.018/0.066, and 0.014/0.029/0.043 respectively. Batch 16
had four sync samples per size: 0.029/0.030/0.030, 0.014/0.021/0.021, and
0.016/0.034/0.034 milliseconds. Tracing affects timings; overlayfs and the underlying
storage are not a power-cut test.

The Linux ignored tests `syscall_sync_failure_reaches_script` and `injected_write_is_retried`
are intended for `strace` fault injection. The former passed under
`strace -f -e inject=fdatasync:error=EIO:when=1`; the latter passed with an existing path
in `L3I_DURABILITY_FAULT_PATH`, using `strace -f -P "$L3I_DURABILITY_FAULT_PATH" -e
inject=write:error=EINTR:when=1`. The interrupted write was observed to retry and the
complete 16 KiB record was read back.
A separate run compiled `tests/support/fs_short_write.c` with clang as a shared library and
preloaded it into the test executable. It limited the first matching write to seven actual
bytes; `strace` observed writes of 7 and 16,377 bytes, and the test reopened the complete
16 KiB record. The shim is test-only; it does not alter the filesystem implementation.
