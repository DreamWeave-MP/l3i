//! `@dream/intl`, cold and warm, counted in CPU instructions (`perf_event_open`, user space,
//! this process) and timed, with the allocations each operation makes.
//!
//! - Rust rows call `l3i::intl` directly, no VM: what ICU4X and the wrappers cost.
//! - Luau rows run the same operation from an interpreted loop through the binding; the
//!   `loop only` row is the loop itself, already subtracted from every Luau row, and
//!   `PluralRules:type()` is a bound method that does nothing, the floor any method call pays.
//! - Native allocations are Rust heap calls (this bench's counting allocator); VM bytes are what
//!   Luau's heap grew by with the collector stopped.
//! - Retention: what holding a thousand handles costs, VM and native, per handle.
//!
//! `cargo bench --features intl --bench intl`. Linux, `perf_event_paranoid` at 2 or lower;
//! elsewhere only the timings print.

#![allow(clippy::cast_precision_loss, clippy::too_many_lines, clippy::missing_panics_doc)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::intl::{
    DecimalFormatter, DecimalOptions, Grouping, IntlExtension, Locale, Operand, PluralKind, PluralRules, canonicalize,
    with_canonical,
};
use l3i::memory::GcControl;

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;

/// The system allocator, counting calls and bytes.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System` unchanged; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const ROUNDS: usize = 5;

/// Instructions, when perf counters are available.
struct Instructions {
    #[cfg(target_os = "linux")]
    counter: Option<counter::Counter>,
}

impl Instructions {
    fn open() -> Instructions {
        Instructions {
            #[cfg(target_os = "linux")]
            counter: counter::Counter::open(counter::PERF_TYPE_HARDWARE, counter::PERF_COUNT_HW_INSTRUCTIONS),
        }
    }

    fn count(&self, body: &mut dyn FnMut()) -> Option<u64> {
        #[cfg(target_os = "linux")]
        if let Some(counter) = &self.counter {
            return Some(counter.measure(body));
        }
        body();
        None
    }
}

/// One operation, run `n` times per round: instructions, nanoseconds, native allocations and
/// bytes, each per operation and the minimum over the rounds.
#[derive(Clone, Copy)]
struct Cost {
    instructions: Option<f64>,
    nanos: f64,
    allocations: f64,
    bytes: f64,
}

fn measure(instructions: &Instructions, n: u64, mut body: impl FnMut()) -> Cost {
    let mut best = Cost { instructions: None, nanos: f64::MAX, allocations: f64::MAX, bytes: f64::MAX };
    for _ in 0..ROUNDS {
        let counted = instructions.count(&mut body);
        let (allocations, bytes) = (ALLOCATIONS.load(Ordering::Relaxed), ALLOCATED.load(Ordering::Relaxed));
        let start = Instant::now();
        body();
        let nanos = start.elapsed().as_nanos() as f64;
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
        let bytes = ALLOCATED.load(Ordering::Relaxed) - bytes;
        let n = n as f64;
        best.instructions = match (best.instructions, counted) {
            (Some(old), Some(new)) => Some(old.min(new as f64 / n)),
            (_, new) => new.map(|new| new as f64 / n),
        };
        best.nanos = best.nanos.min(nanos / n);
        best.allocations = best.allocations.min(allocations as f64 / n);
        best.bytes = best.bytes.min(bytes as f64 / n);
    }
    best
}

fn row(label: &str, cost: Cost, extra: &str) {
    let instructions = cost.instructions.map_or_else(|| "-".to_owned(), |count| format!("{count:.0}"));
    println!(
        "| {label} | {instructions} | {:.1} | {:.2} | {:.0} | {extra} |",
        cost.nanos, cost.allocations, cost.bytes
    );
}

fn header(title: &str, extra: &str) {
    println!("\n{title}\n");
    println!("| Operation | instr/op | ns/op | native allocs/op | native bytes/op | {extra} |");
    println!("|---|---:|---:|---:|---:|---|");
}

fn locale(tag: &str) -> Locale {
    Locale::parse(tag).unwrap()
}

fn options(min: Option<u8>, max: Option<u8>) -> DecimalOptions {
    DecimalOptions { grouping: Grouping::Auto, min_fraction_digits: min, max_fraction_digits: max }
}

fn rust_rows(instructions: &Instructions) {
    const COLD: u64 = 2_000;
    const WARM: u64 = 200_000;
    header("Rust, no VM: cold construction", "note");
    for tag in ["pt_br", "zh-Hant-TW", "de-DE-u-ca-gregory-nu-latn"] {
        row(
            &format!("`Locale::parse(\"{tag}\")`"),
            measure(instructions, COLD, || {
                for _ in 0..COLD {
                    black_box(Locale::parse(black_box(tag)).unwrap());
                }
            }),
            "",
        );
        row(
            &format!("`with_canonical(\"{tag}\")`"),
            measure(instructions, COLD, || {
                for _ in 0..COLD {
                    black_box(with_canonical(black_box(tag), str::len).unwrap());
                }
            }),
            "no locale kept",
        );
    }
    row(
        "`canonicalize(\"pt_br\")`",
        measure(instructions, COLD, || {
            for _ in 0..COLD {
                black_box(canonicalize(black_box("pt_br")).unwrap());
            }
        }),
        "owned `String`",
    );
    for (tag, kind) in [
        ("en", PluralKind::Cardinal),
        ("pl", PluralKind::Cardinal),
        ("ar", PluralKind::Cardinal),
        ("en", PluralKind::Ordinal),
    ] {
        let locale = locale(tag);
        row(
            &format!("`PluralRules::new({tag}, {})`", kind.name()),
            measure(instructions, COLD, || {
                for _ in 0..COLD {
                    black_box(PluralRules::new(&locale, kind).unwrap());
                }
            }),
            "",
        );
    }
    for (tag, min, max) in [("en", None, None), ("fr", Some(2), Some(2)), ("ar-EG", None, None)] {
        let locale = locale(tag);
        row(
            &format!("`DecimalFormatter::new({tag})`"),
            measure(instructions, COLD, || {
                for _ in 0..COLD {
                    black_box(DecimalFormatter::new(&locale, options(min, max)).unwrap());
                }
            }),
            "",
        );
    }

    header("Rust, no VM: warm calls on a built handle", "note");
    let pl = PluralRules::new(&locale("pl"), PluralKind::Cardinal).unwrap();
    let ordinal = PluralRules::new(&locale("en"), PluralKind::Ordinal).unwrap();
    for (label, operand) in [
        ("cardinal pl, `Integer(5)`", Operand::Integer(5)),
        ("cardinal pl, `Number(22.0)`", Operand::Number(22.0)),
        ("cardinal pl, `Number(1.5)`", Operand::Number(1.5)),
        ("cardinal pl, `Decimal(\"1.00\")`", Operand::Decimal("1.00")),
        ("cardinal pl, `Decimal(\"12345678901234567892\")`", Operand::Decimal("12345678901234567892")),
    ] {
        row(
            &format!("`PluralRules::category`, {label}"),
            measure(instructions, WARM, || {
                for _ in 0..WARM {
                    black_box(pl.category(black_box(operand)).unwrap());
                }
            }),
            "",
        );
    }
    row(
        "`PluralRules::category`, ordinal en, `Integer(23)`",
        measure(instructions, WARM, || {
            for _ in 0..WARM {
                black_box(ordinal.category(black_box(Operand::Integer(23))).unwrap());
            }
        }),
        "",
    );
    let en = DecimalFormatter::new(&locale("en"), options(None, None)).unwrap();
    let fr = DecimalFormatter::new(&locale("fr"), options(Some(2), Some(2))).unwrap();
    let mut out = String::with_capacity(64);
    for (label, formatter, operand) in [
        ("en, `Integer(1234567)`", &en, Operand::Integer(1_234_567)),
        ("en, `Integer(42)`", &en, Operand::Integer(42)),
        ("en, `Number(1234567.891)`", &en, Operand::Number(1_234_567.891)),
        ("en, `Number(0.5)`", &en, Operand::Number(0.5)),
        ("fr 2..2, `Decimal(\"1234567.895\")`", &fr, Operand::Decimal("1234567.895")),
        ("fr 2..2, `Integer(2)`", &fr, Operand::Integer(2)),
    ] {
        row(
            &format!("`DecimalFormatter::format_to`, {label}"),
            measure(instructions, WARM, || {
                for _ in 0..WARM {
                    out.clear();
                    formatter.format_to(black_box(operand), &mut out).unwrap();
                    black_box(out.len());
                }
            }),
            "reused `String`",
        );
    }
}

const CHUNK: &str = r"
local intl = require('@dream/intl')
local en = intl.pluralRules('en')
local ordinal = intl.pluralRules('en', 'ordinal')
local pl = intl.pluralRules('pl')
local enFormat = intl.decimalFormatter('en')
local frFormat = intl.decimalFormatter('fr', { minFractionDigits = 2, maxFractionDigits = 2 })
local frOptions = { minFractionDigits = 2, maxFractionDigits = 2 }
local kept = {}
local cases = {}
function cases.loop(n) for i = 1, n do end end
function cases.type(n) for i = 1, n do pl:type() end end
function cases.cardinalInteger(n) for i = 1, n do pl:category(5) end end
function cases.cardinalVaried(n) for i = 1, n do pl:category(i) end end
function cases.cardinalFraction(n) for i = 1, n do pl:category(1.5) end end
function cases.cardinalString(n) for i = 1, n do pl:category('1.00') end end
function cases.ordinal(n) for i = 1, n do ordinal:category(23) end end
function cases.englishOne(n) for i = 1, n do en:category(1) end end
function cases.formatInteger(n) for i = 1, n do enFormat:format(1234567) end end
function cases.formatVaried(n) for i = 1, n do enFormat:format(i) end end
function cases.formatFraction(n) for i = 1, n do enFormat:format(1234567.891) end end
function cases.formatString(n) for i = 1, n do frFormat:format('1234567.895') end end
function cases.canonicalize(n) for i = 1, n do intl.canonicalize('pt_br') end end
function cases.locale(n) for i = 1, n do intl.locale('pt-BR') end end
function cases.newCardinal(n) for i = 1, n do intl.pluralRules('pl') end end
function cases.newOrdinal(n) for i = 1, n do intl.pluralRules('en', 'ordinal') end end
function cases.newFormatter(n) for i = 1, n do intl.decimalFormatter('fr', frOptions) end end
function cases.newFormatterPlain(n) for i = 1, n do intl.decimalFormatter('en') end end
function cases.keepLocales(n) for i = 1, n do kept[i] = intl.locale('pt-BR') end end
function cases.keepRules(n) for i = 1, n do kept[i] = intl.pluralRules('pl') end end
function cases.keepFormatters(n) for i = 1, n do kept[i] = intl.decimalFormatter('fr', frOptions) end end
function cases.drop() kept = {} end
return function(case, n) cases[case](n) end
";

fn heap(runtime: &Runtime) -> f64 {
    f64::from(runtime.gc(GcControl::Count)) * 1024.0 + f64::from(runtime.gc(GcControl::CountRemainder))
}

fn luau_rows(instructions: &Instructions) {
    const WARM: u64 = 100_000;
    const COLD: u64 = 2_000;
    const KEPT: u64 = 1_000;
    let plan = RuntimePlan::builder().policy(RuntimePolicy::new()).extension(IntlExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let run = runtime.load_function(CHUNK).unwrap();
    let call = |case: &str, n: u64| run.invoke::<(), _>(&runtime.stack(), (case, n as f64)).unwrap();
    let vm_bytes = |case: &str, n: u64| {
        runtime.gc(GcControl::Collect);
        runtime.gc(GcControl::Stop);
        let before = heap(&runtime);
        call(case, n);
        let grown = heap(&runtime) - before;
        runtime.gc(GcControl::Restart);
        runtime.gc(GcControl::Collect);
        grown / n as f64
    };
    let floor = measure(instructions, WARM, || call("loop", WARM));
    let net = |cost: Cost| Cost {
        instructions: cost.instructions.zip(floor.instructions).map(|(cost, floor)| cost - floor),
        nanos: cost.nanos - floor.nanos,
        ..cost
    };
    header("Luau, interpreted, through the binding (loop subtracted)", "VM bytes/op");
    row("loop only (not subtracted)", floor, "0");
    for (case, label, n) in [
        ("type", "`pl:type()`: the bound-method floor", WARM),
        ("cardinalInteger", "`pl:category(5)`", WARM),
        ("cardinalVaried", "`pl:category(i)`", WARM),
        ("englishOne", "`en:category(1)`", WARM),
        ("cardinalFraction", "`pl:category(1.5)`", WARM),
        ("cardinalString", "`pl:category('1.00')`", WARM),
        ("ordinal", "`ordinal:category(23)`", WARM),
        ("formatInteger", "`en:format(1234567)`", WARM),
        ("formatVaried", "`en:format(i)`", WARM),
        ("formatFraction", "`en:format(1234567.891)`", WARM),
        ("formatString", "`fr2:format('1234567.895')`", WARM),
        ("canonicalize", "`intl.canonicalize('pt_br')`", WARM),
        ("locale", "`intl.locale('pt-BR')`: a userdata each", COLD),
        ("newCardinal", "`intl.pluralRules('pl')`", COLD),
        ("newOrdinal", "`intl.pluralRules('en', 'ordinal')`", COLD),
        ("newFormatterPlain", "`intl.decimalFormatter('en')`", COLD),
        ("newFormatter", "`intl.decimalFormatter('fr', options)`", COLD),
    ] {
        let cost = net(measure(instructions, n, || call(case, n)));
        row(label, cost, &format!("{:.0}", vm_bytes(case, n)));
        runtime.gc(GcControl::Collect);
    }

    println!("\nRetention: a thousand live handles, per handle\n");
    println!("| Handle | VM bytes | native bytes |");
    println!("|---|---:|---:|");
    for (case, label) in [
        ("keepLocales", "`dream.intl.Locale`"),
        ("keepRules", "`dream.intl.PluralRules`"),
        ("keepFormatters", "`dream.intl.DecimalFormatter`"),
    ] {
        call("drop", 0);
        runtime.gc(GcControl::Collect);
        let (vm, native) = (heap(&runtime), LIVE.load(Ordering::Relaxed));
        call(case, KEPT);
        runtime.gc(GcControl::Collect);
        let vm = (heap(&runtime) - vm) / KEPT as f64;
        let native = (LIVE.load(Ordering::Relaxed) as f64 - native as f64) / KEPT as f64;
        println!("| {label} | {vm:.0} | {native:.0} |");
    }
    call("drop", 0);
    runtime.gc(GcControl::Collect);
    println!(
        "\nRust sizes: `Locale` {} bytes, `PluralRules` {}, `DecimalFormatter` {}.",
        size_of::<Locale>(),
        size_of::<PluralRules>(),
        size_of::<DecimalFormatter>()
    );
}

fn main() {
    let instructions = Instructions::open();
    rust_rows(&instructions);
    luau_rows(&instructions);
}
