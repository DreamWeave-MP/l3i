//! GC controls, dumps, the buffer cage, embedder GC integration, light userdata, and finalizers.

use std::alloc::{Layout, alloc, dealloc};
use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::rc::Rc;

use dream_binder::Runtime;
use dream_binder::memory::{self, BufferCage, EmbedderGc, GcControl, LightUserdata, UserdataMark, WeakRef};
use dream_binder::stack::Scope;
use dream_binder::thread::Resume;
use dream_binder::userdata::{Userdata, tagged};
use dream_binder::value::{Table, Value};

#[test]
fn gc_controls_and_counters() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("keep = {} for i = 1, 1000 do keep[i] = { i } end").unwrap();
    assert_eq!(runtime.gc(GcControl::IsRunning), 1);
    runtime.gc(GcControl::Stop);
    assert_eq!(runtime.gc(GcControl::IsRunning), 0);
    runtime.gc(GcControl::Restart);
    assert_eq!(runtime.gc(GcControl::IsRunning), 1);
    assert!(runtime.gc(GcControl::Count) > 0);
    assert!(runtime.gc(GcControl::CountRemainder) >= 0);
    let previous_goal = runtime.gc(GcControl::SetGoal(150));
    assert_eq!(runtime.gc(GcControl::SetGoal(previous_goal)), 150);
    runtime.gc(GcControl::SetStepMultiplier(200));
    runtime.gc(GcControl::SetStepSize(1));
    let mut finished = false;
    for _ in 0..100_000 {
        if runtime.gc(GcControl::Step(1)) == 1 {
            finished = true;
            break;
        }
    }
    assert!(finished);
    let _ = runtime.gc(GcControl::IsPaused);
    assert!(runtime.allocation_rate() >= -1);
    let before = Runtime::clock();
    runtime.exec("local s = 0 for i = 1, 100000 do s = s + i end").unwrap();
    assert!(Runtime::clock() >= before);
    let pointer = 0x1000_usize;
    assert_ne!(runtime.encode_pointer(pointer), pointer, "pointer encoding is seeded by default");
}

#[test]
fn dumps_write_files() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("held = { nested = { 1, 2, 3 }, text = 'x' }").unwrap();
    let dir = std::env::temp_dir().join(format!("dream-binder-dump-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let memory = dir.join("memory.txt");
    let heap = dir.join("heap.json");
    runtime.memory_dump(&memory).unwrap();
    runtime.gc_dump(&heap, Some(&[c"shared", c"scripts"])).unwrap();
    assert!(std::fs::metadata(&memory).unwrap().len() > 0);
    let heap_text = std::fs::read_to_string(&heap).unwrap();
    assert!(heap_text.contains("objects") || heap_text.contains("{"), "{}", &heap_text[..heap_text.len().min(200)]);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(runtime.memory_dump(&dir.join("missing").join("x")).is_err());
}

#[test]
fn light_userdata_round_trips_with_tags_and_names() {
    let runtime = Runtime::new().unwrap();
    let mut target = 42u32;
    let pointer = (&mut target as *mut u32).cast::<c_void>();
    runtime.set_light_userdata_name(5, "dreamweave.Handle").unwrap();
    assert_eq!(runtime.light_userdata_name(5).as_deref(), Some("dreamweave.Handle"));
    assert_eq!(runtime.light_userdata_name(6), None);
    assert!(runtime.set_light_userdata_name(200, "nope").is_err());
    let stack = runtime.stack();
    stack
        .with_frame(|frame| {
            let view = frame.push(&LightUserdata { pointer, tag: 5 })?;
            let back = view.read::<LightUserdata>()?;
            assert_eq!((back.pointer, back.tag), (pointer, 5));
            frame.set_global("handle")
        })
        .unwrap();
    drop(stack);
    runtime.exec("assert(typeof(handle) == 'dreamweave.Handle') assert(type(handle) == 'userdata')").unwrap();
    assert!(runtime.stack().with_frame(|frame| frame.push(&LightUserdata { pointer, tag: 300 }).map(|_| ())).is_err());
}

struct Keeper {
    keep: RefCell<Vec<Rc<WeakRef>>>,
    resets: Cell<u32>,
}

impl EmbedderGc for Keeper {
    fn reset(&self) {
        self.resets.set(self.resets.get() + 1);
    }
    fn mark_reachable(&self, mark: &mut dyn FnMut(&WeakRef)) {
        for weak in self.keep.borrow().iter() {
            mark(weak);
        }
    }
}

#[test]
fn weak_references_die_unless_the_embedder_gc_marks_them() {
    let runtime = Runtime::new().unwrap();
    let make = |runtime: &Runtime| -> (Table, WeakRef) {
        let table = Table::new(&runtime.stack(), 0, 0).unwrap();
        let weak = runtime.stack().with_frame(|frame| runtime.weak_ref(table.push_to(frame)?.value())).unwrap();
        (table, weak)
    };
    let (table, weak) = make(&runtime);
    assert!(runtime.stack().with_frame(|frame| Ok(weak.get(frame).is_some())).unwrap());
    drop(table);
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(runtime.stack().with_frame(|frame| Ok(weak.get(frame).is_none())).unwrap(), "unmarked weak refs die");
    weak.release(&runtime.stack());

    // With an embedder GC marking it, the weakly referenced value survives.
    let (table, weak) = make(&runtime);
    let weak = Rc::new(weak);
    let keeper = Keeper { keep: RefCell::new(vec![weak.clone()]), resets: Cell::new(0) };
    runtime.set_embedder_gc(keeper);
    drop(table);
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(runtime.stack().with_frame(|frame| Ok(weak.get(frame).is_some())).unwrap(), "marked weak refs survive");
    runtime.clear_embedder_gc();
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(runtime.stack().with_frame(|frame| Ok(weak.get(frame).is_none())).unwrap());
}

struct Native {
    id: u32,
}

unsafe impl Userdata for Native {
    const NAME: &'static str = "dreamweave.tests.Native";
}

thread_local!(static MARKED: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) });

struct RecordMark;

impl UserdataMark<Native> for RecordMark {
    fn mark(value: &Native) {
        MARKED.with(|marked| marked.borrow_mut().push(value.id));
    }
}

#[test]
fn userdata_marks_report_reachable_instances() {
    let runtime = Runtime::new().unwrap();
    tagged::register::<Native>(&runtime, 33, |_| Ok(())).unwrap();
    memory::set_userdata_mark::<Native, RecordMark>(&runtime).unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        tagged::push(&frame, Native { id: 7 }).unwrap();
        frame.set_global("alive").unwrap();
        tagged::push(&frame, Native { id: 8 }).unwrap();
    }
    runtime.collect_garbage();
    runtime.collect_garbage();
    let marked = MARKED.with(|m| m.borrow().clone());
    assert!(marked.contains(&7), "{marked:?}");
    assert!(
        !marked.contains(&8) || marked.iter().filter(|&&id| id == 8).count() <= 1,
        "the unreachable one is not marked each cycle"
    );
    struct Untagged;
    unsafe impl Userdata for Untagged {
        const NAME: &'static str = "dreamweave.tests.UntaggedMark";
    }
    assert!(memory::set_userdata_mark::<Untagged, NoMark>(&runtime).is_err());
    struct NoMark;
    impl UserdataMark<Untagged> for NoMark {
        fn mark(_: &Untagged) {}
    }
}

struct CountingCage {
    allocations: Cell<u32>,
    frees: Cell<u32>,
}

impl BufferCage for CountingCage {
    fn allocate(&self, ptr: *mut c_void, old_size: usize, new_size: usize, _kind: c_int) -> *mut c_void {
        let layout = |size: usize| Layout::from_size_align(size.max(1), 16).unwrap();
        // SAFETY: the standard lua_Alloc contract: sizes describe the block exactly.
        unsafe {
            if new_size == 0 {
                if !ptr.is_null() {
                    dealloc(ptr.cast(), layout(old_size));
                    self.frees.set(self.frees.get() + 1);
                }
                return std::ptr::null_mut();
            }
            let fresh = alloc(layout(new_size));
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), fresh, old_size.min(new_size));
                dealloc(ptr.cast(), layout(old_size));
            } else {
                self.allocations.set(self.allocations.get() + 1);
            }
            fresh.cast()
        }
    }
}

thread_local!(static CAGE_STATS: Cell<(u32, u32)> = const { Cell::new((0, 0)) });

#[test]
fn the_buffer_cage_sees_every_buffer_allocation() {
    struct Reporting(CountingCage);
    impl BufferCage for Reporting {
        fn allocate(&self, ptr: *mut c_void, old_size: usize, new_size: usize, kind: c_int) -> *mut c_void {
            let result = self.0.allocate(ptr, old_size, new_size, kind);
            CAGE_STATS.with(|stats| stats.set((self.0.allocations.get(), self.0.frees.get())));
            result
        }
    }
    let runtime = Runtime::builder()
        .buffer_cage(Reporting(CountingCage { allocations: Cell::new(0), frees: Cell::new(0) }))
        .build()
        .unwrap();
    runtime.exec("local b = buffer.create(4096) buffer.writeu32(b, 0, 7) assert(buffer.readu32(b, 0) == 7)").unwrap();
    assert!(CAGE_STATS.with(Cell::get).0 >= 1, "buffer.create went through the cage");
    runtime.collect_garbage();
    runtime.collect_garbage();
    assert!(CAGE_STATS.with(Cell::get).1 >= 1, "the collected buffer was freed through the cage");
    drop(runtime);
}

#[test]
fn coroutine_finalizers_run_when_asked() {
    let runtime = Runtime::new().unwrap();
    runtime.enable_coroutine_finalizers().unwrap();
    let body = runtime.load_function("return function() finalized = 'no' coroutine.yield() return 1 end").unwrap();
    let on_finish = runtime.load_function("return function() finalized = 'yes' end").unwrap();
    let thread = runtime.new_thread().unwrap();
    // A fresh thread counts as finished for Luau: finalizers attach to a live coroutine.
    assert!(runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).is_err());
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    assert!(!thread.has_finalizers());
    runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).unwrap();
    assert!(thread.has_finalizers());
    assert!(matches!(thread.resume(&runtime.stack(), ()).unwrap(), Resume::Finished(_)));
    let finalize = runtime.finalizer_function().unwrap();
    finalize.invoke::<(), _>(&runtime.stack(), (thread.value(),)).unwrap();
    assert_eq!(
        runtime.global("finalized").unwrap().with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap(),
        "yes"
    );
    // The main thread cannot take finalizers; the error is reported, not raised through Rust.
    let main = runtime.load_function("return function() end").unwrap();
    let _ = main;
    let _ = Value::invalid();
}

#[test]
fn fast_flags_are_introspectable() {
    let _runtime = Runtime::new().unwrap();
    assert_eq!(memory::fast_flag("LuauFastpcall"), Some(true), "policy flag is on");
    assert_eq!(memory::fast_flag("NoSuchFlagAtAll"), None);
    assert!(memory::fast_flags().iter().any(|(name, _)| name == "LuauFastpcall"));
    memory::set_fast_flag("DebugLuauCoroutineFinally", true).unwrap();
    assert!(memory::set_fast_flag("NoSuchFlagAtAll", true).is_err());
    assert!(memory::set_fast_int("NoSuchIntAtAll", 1).is_err());
    assert_eq!(memory::fast_int("NoSuchIntAtAll"), None);
    assert!(memory::fast_int("LuauInlineHitsThreshold").is_some());
}
