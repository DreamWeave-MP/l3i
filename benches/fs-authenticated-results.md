# Authenticated ranged reads: measurements

Reproduce with `cargo bench --features fs-authenticated -- fs_authenticated`, which prints CSV.
The default is two object sizes (2 MiB, 64 MiB) and takes a few minutes.
`L3I_AUTH_BENCH_FULL=1` adds 256 KiB, 16 MiB and 128 MiB with a longer budget; **that table has not
been run**, so nothing below speaks for those sizes. These tables hold one chunk (`hold_bytes(0)`), so
they measure the hash and not the cache. The filter `fs_authenticated_cache` runs only the comparison
under "Holding more chunks", about three minutes on this machine.

## What this is and isn't

Two runs of the default table on one machine: Linux 7.2, i7-10870H, btrfs over LUKS on an SSD,
release build. The two numbers in a cell are the two runs' medians. Other work was running (load
average 5 to 8), which is why a few 4 KiB rows disagree badly, such as 474 and 308 ms; read those as
noise, not as a property of the chunk size. Samples are few where a sample is slow (five for
anything hashing the whole 64 MiB), so these are orders of magnitude and rough ratios, not
statistically established differences. p95 is only printed from 20 samples up. Windows and macOS
have not been measured, and neither has the cost of the Luau binding, only the Rust reader.

This CPU has no SHA-NI, so SHA-256 runs at about 240 MB/s and hashing dominates everything here.
A CPU with SHA extensions should narrow the gap to plain reads considerably; that is untested.

- `full`: open, read and hash the whole object, compare, then slice. A sample of many reads
  verifies once, which favors it.
- `plain`: positional reads through a retained handle. Checks nothing; it is the floor.
- `warm`: a retained authenticated reader.
- `cold`: per sample, read the manifest's file, verify it, open the object, then read.
- `pagecold`: the object's pages evicted with `posix_fadvise` first, so the disk is involved.

## 64 MiB object, warm reader, median per sample

Times are microseconds except where marked. A sample is one read unless the pattern says otherwise.

| Pattern | full | plain | 4 KiB chunks | 16 KiB chunks | 64 KiB chunks |
| --- | ---: | ---: | ---: | ---: | ---: |
| one 64-byte read | 293.9, 289.8 ms | 0.5, 0.4 | 17.1, 17.0 | 67.0, 64.6 | 248, 267 |
| one 4 KiB read | 300.5, 289.6 ms | 0.9, 0.8 | 33.5, 34.0 | 69.1, 65.6 | 259, 269 |
| one 16 KiB read | 304.3, 296.7 ms | 1.9, 1.8 | 83.1, 86.2 | 138, 132 | 273, 270 |
| one 16 KiB read, pages evicted | 314.9, 314.8 ms | 125, 126 | 1004, 643 | 450, 455 | 484, 495 |
| 32 random 4 KiB reads | 293.1, 296.8 ms | 23.4, 22.5 | 1235, 1168 | 4794, 3018 | 9805, 9311 |
| 64 reads of 64 bytes in one area | 294.5, 287.4 ms | 19.0, 18.1 | 31.1, 31.9 | 65.4, 71.2 | 262, 259 |
| 256 KiB in order, 64 bytes at a time | 292.7, 300.8 ms | 1165, 1126 | 1790, 1299 | 1297, 1312 | 1290, 1237 |
| the whole object in order, 16 KiB at a time | 298.0, 292.4 ms | 6.1, 5.9 ms | 321.9, 295.3 ms | 285.2, 286.6 ms | 284.6, 271.5 ms |
| one read of 25% | 297.2, 287.3 ms | 2.4, 2.3 ms | 130.5, 74.4 ms | 67.9, 72.5 ms | 73.3, 71.1 ms |
| one read of 90% | 313.5, 310.3 ms | 27.7, 28.1 ms | 474, 308 ms | 270, 286 ms | 282, 267 ms |
| one read of 100% | 318.7, 310.2 ms | 34.5, 34.7 ms | 448, 325 ms | 307, 305 ms | 317, 295 ms |

## Bytes touched, 64 MiB object

Every byte a warm reader reads is hashed exactly once; a byte served from the held chunk is neither.

| Pattern | full, read and hashed | 4 KiB chunks | 16 KiB chunks | 64 KiB chunks |
| --- | ---: | ---: | ---: | ---: |
| one 64-byte read | 67,108,864 | 4,172 | 16,384 | 65,536 |
| one 16 KiB read | 67,108,864 | 20,480 | 32,768 | about 81,000 |
| 32 random 4 KiB reads | 67,108,864 | about 262,000 | about 670,000 | about 2,300,000 |

Those are per-sample means. A single 64-byte read at the 16 KiB default reads and hashes 4,096 times
fewer bytes than verifying the object. CPU time tracked wall time closely except where pages were
evicted, as it should when the work is hashing rather than waiting.

## Opening a reader

`cold` adds reading and verifying the manifest to the read, so it favors larger chunks.

| | 4 KiB chunks | 16 KiB chunks | 64 KiB chunks |
| --- | ---: | ---: | ---: |
| manifest for 64 MiB | 524,320 bytes | 131,104 bytes | 32,800 bytes |
| manifest for 2 MiB | 16,416 bytes | 4,128 bytes | 1,056 bytes |
| verifying the 64 MiB manifest | 2230, 2199 µs | 516, 521 µs | 122, 132 µs |
| building, 64 MiB | 288, 316 ms | 281, 288 ms | 269, 273 ms |
| building, 2 MiB | 8.7, 8.7 ms | 8.2, 8.5 ms | 8.3, 8.5 ms |
| 64 MiB, cold, one 64-byte read | 2230, 2257 µs | 624, 627 µs | 398, 409 µs |
| 64 MiB, cold, one 16 KiB read | 2585, 2354 µs | 682, 668 µs | 422, 416 µs |

## Small objects

The saving scales with the object, and it is smaller here, but it is still there at 2 MiB. One 4 KiB
read costs 8304 and 8156 µs through `full`, 0.5 µs through `plain`, and at 16 KiB chunks 65.4 and
64.1 µs warm, 89.7 and 89.2 µs cold. At 64 KiB chunks a cold read is 278 and 267 µs, against the
smaller chunks' 90, so a large chunk gains nothing on an object this small. Objects under about one
chunk gain nothing at all: a read hashes the whole object anyway, and only the manifest differs.

## The default: 16 KiB

Against 64 KiB, a warm sparse read at 16 KiB hashes a quarter as much and takes about a quarter as
long (65 µs against about 250), at the price of a manifest four times larger and a cold open about
1.5 times slower at 64 MiB. Against 4 KiB, its manifest is a quarter the size and a cold open about
3.5 times faster, while a warm 64-byte read is about 4 times slower. A script that opens readers
often and reads little should choose 64 KiB; one that holds a reader and reads tiny records should
choose 4 KiB.

## Holding the last verified chunk

The first version of the reader had no held chunk. It re-read and re-hashed a chunk for every partial
read, which this table found:

| 256 KiB in order, 64 bytes at a time | bytes hashed before | ms before (one run) | ms after | bytes hashed after |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB chunks | 16,777,216 | 66.4 | 1.8, 1.3 | 262,144 |
| 16 KiB chunks | 67,108,864 | 265.4 | 1.3, 1.3 | 262,144 |
| 64 KiB chunks | 268,435,456 | 1057.0 | 1.3, 1.2 | 262,144 |

Afterwards that scan is close to plain reads (1165 and 1126 µs), since 256 KiB hashes only once and
the 4096 small reads are what remains. Reading a 64 MiB object 16 KiB at a time through 64 KiB chunks
went from about 1154 ms to 285 and 272 ms for the same reason.

## Holding more chunks

One held chunk thrashes as soon as reads turn between two places. Same 64 MiB object, reads of 64
bytes, on a reader warmed by one unmeasured pass, two runs, median microseconds per read. In brackets
is the bytes hashed per read, which is the same in both runs because the seeds are fixed, and is the
better number: other work on the machine moved the times, by nearly half in a few 64 KiB rows.

16 KiB chunks:

| Workload | 1 chunk (16 KiB) | 4 (64 KiB) | 16 (256 KiB, the default) | 64 (1 MiB) | 256 (4 MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Uniform random | 63, 68 (16,458) | 64, 67 (16,438) | 64, 66 (16,384) | 63, 67 (16,187) | 61, 64 (15,399) |
| Skewed, popularity 1/rank | 62, 65 (16,115) | 59, 62 (15,434) | 52, 55 (13,504) | 39, 40 (9,898) | 20, 21 (4,932) |
| Binary search | 80, 79 (19,579) | 75, 76 (18,352) | 73, 72 (17,965) | 53, 52 (12,761) | 38, 37 (8,793) |
| Bursts within 256 KiB | 65, 62 (15,506) | 53, 50 (12,626) | 17, 17 (4,216) | 17, 17 (4,123) | 17, 17 (3,926) |
| A table, then a record, in turn | 68, 67 (16,443) | 44, 43 (10,250) | 57, 33 (8,208) | 36, 33 (8,130) | 36, 31 (7,666) |

64 KiB chunks:

| Workload | 1 chunk (64 KiB) | 4 (256 KiB, the default) | 16 (1 MiB) | 64 (4 MiB) |
| --- | ---: | ---: | ---: | ---: |
| Uniform random | 339, 261 (65,449) | 294, 259 (65,186) | 288, 266 (64,662) | 263, 256 (61,822) |
| Skewed, popularity 1/rank | 274, 261 (64,750) | 401, 245 (61,997) | 229, 214 (53,434) | 149, 142 (35,826) |
| Binary search | 269, 262 (65,055) | 250, 245 (59,813) | 241, 235 (57,978) | 142, 140 (35,608) |
| Bursts within 256 KiB | 200, 200 (49,963) | 45, 40 (9,941) | 23, 21 (5,035) | 30, 20 (5,035) |
| A table, then a record, in turn | 401, 261 (65,623) | 137, 132 (32,593) | 135, 130 (32,156) | 129, 125 (31,020) |

Holding more never measurably cost anything on the load it cannot help: uniform random reads stayed
at the hash's price up to 256 slots, whose search is small next to one chunk's hash. The default is a
budget in bytes, 256 KiB, because a count would mean 128 MiB at the largest chunk size. It takes the
two loads with locality from about 65 µs down to 17 and 33 µs at 16 KiB chunks, and takes memory only
as chunks are read. Binary search and skewed loads need far more: their gains start around 1 MiB, and
a host that knows it has such a load should say so with `heldBytes`. These are synthetic loads, so
they show what each shape gains, not what any real program's mix will.

## What remains expensive

Hashing. A read of most of the object costs about what full verification does (90%: 270 to 286 ms
against 310 to 314), so it stays competitive but saves little, and it is roughly ten times a plain
read on this CPU. A read that happens to straddle a boundary pays for two chunks. Uniform random small
reads almost never hit a held chunk, whatever the budget.
