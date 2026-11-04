//! Authenticated ranged reads against the two things they replace: reading and hashing the whole
//! object for every access, and a plain positional read, which checks nothing and is the floor.
//!
//! Own harness, because what matters is more than time: each row also counts the bytes read and
//! hashed, the read calls and the bytes allocated. Prints one CSV row per case on stdout. Results
//! and how to reproduce them: `benches/fs-authenticated-results.md`.
//!
//! Modes: `full` opens, reads and hashes the whole object per sample, then slices; `plain` is
//! positional reads through a retained handle; `warm` is a retained authenticated reader; `cold`
//! reads the manifest from its file, verifies it, opens the object and only then reads, per sample.
//! A `pagecold` pattern evicts the object's pages first, where the OS allows it without root.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use std::cell::Cell;
use std::fs::File;
use std::hint::black_box;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use l3i::fs::authenticated::{AuthenticatedReader, Commitment, RangeSource, VerifiedManifest, build_manifest};
use sha2::{Digest as _, Sha256};

use crate::counting_alloc::{ALLOCATED, ALLOCATIONS};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// A file, counting what is asked of it.
struct Counted {
    file: File,
    calls: Cell<u64>,
    bytes: Cell<u64>,
}

impl Counted {
    fn new(file: File) -> Counted {
        Counted { file, calls: Cell::new(0), bytes: Cell::new(0) }
    }
}

impl RangeSource for Counted {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read_at(offset, dst)?;
        self.calls.set(self.calls.get() + 1);
        self.bytes.set(self.bytes.get() + count as u64);
        Ok(count)
    }

    fn len(&self) -> io::Result<u64> {
        RangeSource::len(&self.file)
    }
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn offset(&mut self, size: u64, length: u64) -> u64 {
        self.next() % (size - length + 1)
    }
}

/// What one sample asks for.
#[derive(Clone, Copy)]
enum Pattern {
    /// One read of this length at a random offset.
    Single(u64),
    /// The same, with the object's pages evicted first.
    PageCold(u64),
    /// Reads of this length at random offsets, this many of them.
    Sparse(u64, u64),
    /// The first bytes of the object read in order, at most this many, this long at a time.
    Sequential(u64, u64),
    /// Reads of this length inside one chunk-sized area, this many times.
    Repeated(u64, u64),
    /// One read of this percentage of the object.
    Share(u64),
}

impl Pattern {
    fn name(self) -> String {
        match self {
            Pattern::Single(length) => format!("single_{length}"),
            Pattern::PageCold(length) => format!("pagecold_{length}"),
            Pattern::Sparse(length, count) => format!("sparse_{count}x{length}"),
            Pattern::Sequential(length, span) => format!("sequential_{length}_over_{span}"),
            Pattern::Repeated(length, count) => format!("repeated_{count}x{length}"),
            Pattern::Share(percent) => format!("share_{percent}pct"),
        }
    }

    /// The reads of one sample of an object of `size` bytes.
    fn ranges(self, size: u64, random: &mut Random) -> Vec<(u64, usize)> {
        let fit = |length: u64| length.min(size);
        match self {
            Pattern::Single(length) | Pattern::PageCold(length) => {
                let length = fit(length);
                vec![(random.offset(size, length), length as usize)]
            }
            Pattern::Sparse(length, count) => {
                let length = fit(length);
                (0..count).map(|_| (random.offset(size, length), length as usize)).collect()
            }
            Pattern::Sequential(length, span) => {
                let length = fit(length);
                (0..fit(span) / length).map(|index| (index * length, length as usize)).collect()
            }
            Pattern::Repeated(length, count) => {
                let length = fit(length);
                let area = random.offset(size, fit(4 * KIB));
                (0..count).map(|index| (area + (index * 8) % (fit(4 * KIB) - length + 1), length as usize)).collect()
            }
            Pattern::Share(percent) => {
                let length = (size * percent / 100).max(1);
                vec![(random.offset(size, length), length as usize)]
            }
        }
    }
}

/// Asks the OS to forget the file's clean pages; a no-op where it cannot.
fn evict(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: an advisory call on an open descriptor; nothing is read or written through it.
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// Puts the pages back after a page-cold pattern, so the next case does not pay for its eviction.
fn rewarm(cold_pages: bool, object: &ObjectFile) {
    if cold_pages {
        black_box(std::fs::read(&object.path).unwrap());
    }
}

/// User plus system CPU time so far, where the platform says.
fn cpu_time() -> Duration {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero rusage is valid, and getrusage only writes through the pointer.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
        let part = |time: libc::timeval| {
            Duration::new(u64::try_from(time.tv_sec).unwrap_or(0), u32::try_from(time.tv_usec).unwrap_or(0) * 1000)
        };
        part(usage.ru_utime) + part(usage.ru_stime)
    }
    #[cfg(not(unix))]
    Duration::ZERO
}

/// What a sample touched, beyond its time.
#[derive(Clone, Copy, Default)]
struct Touched {
    read: u64,
    hashed: u64,
    calls: u64,
}

/// The configuration of a run: the full table, or one quick pass for the CI gate.
struct Budget {
    min_samples: usize,
    min_time: Duration,
}

struct Case<'a> {
    size: u64,
    chunk: Option<u64>,
    mode: &'a str,
    pattern: Pattern,
    budget: &'a Budget,
}

/// Runs `sample` until the budget is spent and prints the row. A percentile is only claimed when
/// there are enough samples to mean something.
fn measure(case: &Case<'_>, mut sample: impl FnMut(&mut Random) -> Touched) {
    let mut random = Random(0x5EED_0000 ^ case.size);
    let mut times = Vec::new();
    let mut touched = Touched::default();
    let mut add = |sampled: Touched| {
        touched.read += sampled.read;
        touched.hashed += sampled.hashed;
        touched.calls += sampled.calls;
    };
    let allocated = ALLOCATED.load(Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let cpu = cpu_time();
    let started = Instant::now();
    while times.len() < case.budget.min_samples || started.elapsed() < case.budget.min_time {
        let before = Instant::now();
        let sampled = sample(&mut random);
        times.push(before.elapsed());
        add(sampled);
    }
    let cpu = cpu_time().saturating_sub(cpu);
    let count = times.len() as u64;
    times.sort_unstable();
    let median = times[times.len() / 2];
    let p95 = (times.len() >= 20).then(|| times[times.len() * 95 / 100]);
    println!(
        "fs_authenticated,{size},{chunk},{mode},{pattern},{samples},{median:.1},{p95},{cpu:.1},{read},{hashed},{calls},{bytes},{allocs}",
        size = case.size,
        chunk = case.chunk.map_or_else(|| "-".to_owned(), |chunk| chunk.to_string()),
        mode = case.mode,
        pattern = case.pattern.name(),
        samples = times.len(),
        median = median.as_secs_f64() * 1e6,
        p95 = p95.map_or_else(|| "-".to_owned(), |p95| format!("{:.1}", p95.as_secs_f64() * 1e6)),
        cpu = cpu.as_secs_f64() * 1e6 / count as f64,
        read = touched.read / count,
        hashed = touched.hashed / count,
        calls = touched.calls / count,
        bytes = (ALLOCATED.load(Ordering::Relaxed) - allocated) / count,
        allocs = (ALLOCATIONS.load(Ordering::Relaxed) - allocations) / count,
    );
}

/// The whole object, read and hashed, and the ranges copied from it. This is the most favorable
/// version of the comparison: a sample of many reads verifies only once.
fn full_sample(path: &Path, trusted: &[u8; 32], ranges: &[(u64, usize)]) -> Touched {
    let file = File::open(path).unwrap();
    let size = RangeSource::len(&file).unwrap();
    let mut whole = vec![0; size as usize];
    let count = file.read_at(0, &mut whole).unwrap();
    assert_eq!(count, whole.len());
    let digest: [u8; 32] = Sha256::digest(&whole).into();
    assert_eq!(&digest, trusted);
    for (offset, length) in ranges {
        let offset = *offset as usize;
        black_box(whole[offset..offset + length].to_vec());
    }
    Touched { read: size, hashed: size, calls: 1 }
}

fn plain_sample(source: &Counted, ranges: &[(u64, usize)]) -> Touched {
    source.calls.set(0);
    source.bytes.set(0);
    for (offset, length) in ranges {
        let mut bytes = vec![0; *length];
        let count = source.read_at(*offset, &mut bytes).unwrap();
        assert_eq!(count, *length);
        black_box(&bytes);
    }
    Touched { read: source.bytes.get(), hashed: 0, calls: source.calls.get() }
}

fn warm_sample(source: &Counted, reader: &mut AuthenticatedReader<&Counted>, ranges: &[(u64, usize)]) -> Touched {
    source.calls.set(0);
    source.bytes.set(0);
    for (offset, length) in ranges {
        black_box(reader.read_at(*offset, *length).unwrap());
    }
    // A warm reader hashes each byte it reads exactly once: nothing else reaches a digest.
    Touched { read: source.bytes.get(), hashed: source.bytes.get(), calls: source.calls.get() }
}

fn cold_sample(path: &Path, manifest_path: &Path, commitment: &Commitment, ranges: &[(u64, usize)]) -> Touched {
    let manifest = std::fs::read(manifest_path).unwrap();
    let verified = VerifiedManifest::verify(&manifest, commitment).unwrap();
    let source = Counted::new(File::open(path).unwrap());
    let mut reader = AuthenticatedReader::open(&source, verified).unwrap().hold_bytes(0);
    for (offset, length) in ranges {
        black_box(reader.read_at(*offset, *length).unwrap());
    }
    let manifest_len = manifest.len() as u64;
    Touched {
        read: source.bytes.get() + manifest_len,
        hashed: source.bytes.get() + manifest_len,
        calls: source.calls.get() + 1,
    }
}

fn patterns(quick: bool) -> Vec<Pattern> {
    if quick {
        return vec![Pattern::Single(4 * KIB), Pattern::Share(90)];
    }
    vec![
        Pattern::Single(64),
        Pattern::Single(4 * KIB),
        Pattern::Single(16 * KIB),
        Pattern::PageCold(16 * KIB),
        Pattern::Sparse(4 * KIB, 32),
        Pattern::Sequential(64, 256 * KIB),
        Pattern::Sequential(16 * KIB, u64::MAX),
        Pattern::Repeated(64, 64),
        Pattern::Share(25),
        Pattern::Share(90),
        Pattern::Share(100),
    ]
}

/// One object on disk with its page cache settled, deleted when dropped.
struct ObjectFile {
    path: PathBuf,
    manifest_path: PathBuf,
}

impl ObjectFile {
    fn new(directory: &Path, size: u64) -> ObjectFile {
        let path = directory.join(format!("object-{size}"));
        let mut writer = io::BufWriter::new(File::create(&path).unwrap());
        // Incompressible, in case the filesystem would otherwise store less than it is told.
        let mut random = Random(0xABCD_EF01 ^ size);
        let mut left = size;
        while left > 0 {
            let word = random.next().to_le_bytes();
            let count = left.min(8) as usize;
            writer.write_all(&word[..count]).unwrap();
            left -= count as u64;
        }
        let file = writer.into_inner().unwrap();
        file.sync_all().unwrap();
        ObjectFile { manifest_path: directory.join(format!("manifest-{size}")), path }
    }
}

impl Drop for ObjectFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.manifest_path);
    }
}

/// Publication's cost at each chunk size: building, the manifest's size, verifying it.
fn publication(object: &ObjectFile, size: u64, chunk: u32, budget: &Budget) -> (Vec<u8>, Commitment) {
    let file = File::open(&object.path).unwrap();
    let mut times = Vec::new();
    let mut last = None;
    let samples = budget.min_samples.min(3);
    for _ in 0..samples {
        let before = Instant::now();
        last = Some(build_manifest(&file, chunk).unwrap());
        times.push(before.elapsed());
    }
    times.sort_unstable();
    let built = last.unwrap();
    let mut verifies = Vec::new();
    for _ in 0..budget.min_samples.max(9) {
        let before = Instant::now();
        black_box(VerifiedManifest::verify(&built.bytes, &built.commitment).unwrap());
        verifies.push(before.elapsed());
    }
    verifies.sort_unstable();
    println!(
        "fs_authenticated_publication,{size},{chunk},{manifest},{build:.1},{verify:.1}",
        manifest = built.bytes.len(),
        build = times[times.len() / 2].as_secs_f64() * 1e6,
        verify = verifies[verifies.len() / 2].as_secs_f64() * 1e6,
    );
    (built.bytes, built.commitment)
}

/// A workload for the held-chunk comparison: the offsets of one sample's small reads.
fn workload(name: &str, size: u64, count: usize, random: &mut Random, chunk: u32) -> Vec<u64> {
    const READ: u64 = 64;
    let last = size - READ;
    let mut offsets = Vec::with_capacity(count + 32);
    match name {
        "uniform" => offsets.extend((0..count).map(|_| random.next() % last)),
        "hot" => offsets.extend((0..count).map(|_| (random.next() % 16) * u64::from(chunk))),
        "sequential" => offsets.extend((0..count).map(|i| (i as u64 * u64::from(chunk)) % last)),
        "thrash" => offsets.extend((0..count).map(|i| ((i as u64 % 1025) * u64::from(chunk)) % last)),
        // Popularity falling as 1/rank over 4096 records scattered across the object, an 80/20
        // shaped load; ranks come from inverting the harmonic sum.
        "skewed" => {
            let records = 4096.0_f64;
            for _ in 0..count {
                let unit = (random.next() >> 11) as f64 / (1_u64 << 53) as f64;
                let rank = records.powf(unit).floor() as u64;
                let scattered = rank.wrapping_mul(0x9E37_79B9_7F4A_7C15) % last;
                offsets.push(scattered);
            }
        }
        // Binary search of sorted fixed records: every search walks the same top levels.
        "binary_search" => {
            while offsets.len() < count {
                let key = random.next() % last;
                let (mut low, mut high) = (0, last);
                while high - low > READ {
                    let middle = low + (high - low) / 2;
                    offsets.push(middle);
                    if key < middle {
                        high = middle;
                    } else {
                        low = middle;
                    }
                }
            }
        }
        // A burst of reads in one small area, then somewhere else.
        "clustered" => {
            while offsets.len() < count {
                let area = random.next() % (last - 256 * KIB);
                offsets.extend((0..64).map(|_| area + random.next() % (256 * KIB)));
            }
        }
        // A table near the start, then the record it names: one read each, turn about.
        "table_then_record" => {
            while offsets.len() < count {
                offsets.push(random.next() % (32 * KIB));
                offsets.push(64 * KIB + random.next() % (last - 64 * KIB));
            }
        }
        _ => unreachable!(),
    }
    offsets
}

/// One chunk held against several, under loads where holding more could matter. Each row's reader
/// is warmed by a first sample that is not measured. Bytes hashed per read is what to compare; a count
/// of source calls would mislead, since a read that straddles two chunks makes two.
pub fn cache_main() {
    let quick = std::env::args().any(|arg| arg == "--test");
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("auth-cache-bench-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let size = if quick { 2 * MIB } else { 64 * MIB };
    let object = ObjectFile::new(&directory, size);
    let cases: &[(u32, &[usize], usize, usize)] = if quick {
        &[(16 << 10, &[1, 8], 64, 1)]
    } else {
        &[
            (512, &[1, 1024], 2050, 5),
            (4 << 10, &[1, 1024], 2050, 5),
            (16 << 10, &[1, 16], 2000, 5),
            (64 << 10, &[1, 4], 500, 5),
        ]
    };
    println!(
        "fs_authenticated_cache,chunk,workload,slots,held_bytes,median_us_per_read,p95_us_per_read,cpu_us_per_read,bytes_hashed_per_read,read_calls_per_read,allocations_per_read"
    );
    for &(chunk, slots, reads, samples) in cases {
        let file = File::open(&object.path).unwrap();
        let built = build_manifest(&file, chunk).unwrap();
        let verified = std::sync::Arc::new(VerifiedManifest::verify(&built.bytes, &built.commitment).unwrap());
        for name in ["uniform", "hot", "sequential", "thrash", "skewed", "clustered", "table_then_record"] {
            for &count in slots {
                let source = Counted::new(File::open(&object.path).unwrap());
                let budget = count as u64 * u64::from(chunk);
                let mut reader = AuthenticatedReader::open(&source, verified.clone()).unwrap().hold_bytes(budget);
                let mut random = Random(0x5EED_CAC4);
                for offset in workload(name, size, reads, &mut random, chunk) {
                    black_box(reader.read_at(offset, 64).unwrap());
                }
                let mut times = Vec::new();
                let (mut total, mut hashed, mut calls, mut allocations) = (0, 0, 0, 0);
                let mut cpu = Duration::ZERO;
                for _ in 0..samples {
                    let offsets = workload(name, size, reads, &mut random, chunk);
                    source.calls.set(0);
                    source.bytes.set(0);
                    let before = Instant::now();
                    let cpu_start = cpu_time();
                    let allocation_start = ALLOCATIONS.load(Ordering::Relaxed);
                    for offset in &offsets {
                        black_box(reader.read_at(*offset, 64).unwrap());
                    }
                    times.push(before.elapsed().as_secs_f64() * 1e6 / offsets.len() as f64);
                    cpu += cpu_time().saturating_sub(cpu_start);
                    allocations += ALLOCATIONS.load(Ordering::Relaxed).saturating_sub(allocation_start);
                    total += offsets.len();
                    hashed += source.bytes.get();
                    calls += source.calls.get();
                }
                times.sort_unstable_by(f64::total_cmp);
                println!(
                    "fs_authenticated_cache,{chunk},{name},{count},{budget},{median:.1},{p95:.1},{cpu:.1},{per:.0},{calls:.3},{allocations:.3}",
                    median = times[times.len() / 2],
                    p95 = times[(times.len() * 95).div_ceil(100) - 1],
                    cpu = cpu.as_secs_f64() * 1e6 / total as f64,
                    per = hashed as f64 / total as f64,
                    calls = calls as f64 / total as f64,
                    allocations = allocations as f64 / total as f64,
                );
            }
        }
    }
    drop(object);
    let _ = std::fs::remove_dir_all(&directory);
}

pub fn main() {
    let quick = std::env::args().any(|arg| arg == "--test");
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("auth-bench-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    // The default is two sizes and a short budget, minutes rather than an hour; the whole table is
    // opt-in.
    let full = std::env::var_os("L3I_AUTH_BENCH_FULL").is_some();
    let (sizes, chunks): (Vec<u64>, Vec<u32>) = if quick {
        (vec![256 * KIB], vec![16 << 10])
    } else if full {
        (vec![256 * KIB, 2 * MIB, 16 * MIB, 64 * MIB, 128 * MIB], vec![4 << 10, 16 << 10, 64 << 10])
    } else {
        (vec![2 * MIB, 64 * MIB], vec![4 << 10, 16 << 10, 64 << 10])
    };
    let budget = if quick {
        Budget { min_samples: 1, min_time: Duration::ZERO }
    } else if full {
        Budget { min_samples: 7, min_time: Duration::from_millis(200) }
    } else {
        Budget { min_samples: 5, min_time: Duration::from_millis(50) }
    };
    println!(
        "fs_authenticated_publication,size,chunk,manifest_bytes,build_us_median,verify_us_median\n\
         fs_authenticated,size,chunk,mode,pattern,samples,median_us_per_sample,p95_us,cpu_us_per_sample,bytes_read,bytes_hashed,read_calls,bytes_allocated,allocations"
    );
    for &size in &sizes {
        let object = ObjectFile::new(&directory, size);
        let trusted: [u8; 32] = Sha256::digest(std::fs::read(&object.path).unwrap()).into();
        let handle = File::open(&object.path).unwrap();
        for pattern in patterns(quick) {
            let cold_pages = matches!(pattern, Pattern::PageCold(_));
            let case = Case { size, chunk: None, mode: "full", pattern, budget: &budget };
            measure(&case, |random| {
                let ranges = pattern.ranges(size, random);
                if cold_pages {
                    evict(&handle);
                }
                full_sample(&object.path, &trusted, &ranges)
            });
            let source = Counted::new(File::open(&object.path).unwrap());
            let case = Case { size, chunk: None, mode: "plain", pattern, budget: &budget };
            measure(&case, |random| {
                let ranges = pattern.ranges(size, random);
                if cold_pages {
                    evict(&handle);
                }
                plain_sample(&source, &ranges)
            });
            rewarm(cold_pages, &object);
        }
        for &chunk in &chunks {
            let (manifest, commitment) = publication(&object, size, chunk, &budget);
            std::fs::write(&object.manifest_path, &manifest).unwrap();
            let verified = std::sync::Arc::new(VerifiedManifest::verify(&manifest, &commitment).unwrap());
            for pattern in patterns(quick) {
                let cold_pages = matches!(pattern, Pattern::PageCold(_));
                let source = Counted::new(File::open(&object.path).unwrap());
                // One chunk held, so these rows measure the hash; `cache_main` measures holding more.
                let mut reader = AuthenticatedReader::open(&source, verified.clone()).unwrap().hold_bytes(0);
                let case = Case { size, chunk: Some(u64::from(chunk)), mode: "warm", pattern, budget: &budget };
                measure(&case, |random| {
                    let ranges = pattern.ranges(size, random);
                    if cold_pages {
                        evict(&handle);
                    }
                    warm_sample(&source, &mut reader, &ranges)
                });
                let case = Case { size, chunk: Some(u64::from(chunk)), mode: "cold", pattern, budget: &budget };
                measure(&case, |random| {
                    let ranges = pattern.ranges(size, random);
                    if cold_pages {
                        evict(&handle);
                    }
                    cold_sample(&object.path, &object.manifest_path, &commitment, &ranges)
                });
                rewarm(cold_pages, &object);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}
