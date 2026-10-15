//! GC controls, dumps, the buffer cage, embedder GC integration, light userdata, and finalizers.

use std::alloc::{Layout, alloc, dealloc};
use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::rc::Rc;

use l3i::Runtime;
use l3i::memory::{self, BufferCage, EmbedderGc, GcControl, LightUserdata, UserdataMark, WeakRef};
use l3i::stack::Scope;
use l3i::thread::Resume;
use l3i::userdata::{Userdata, tagged};
use l3i::value::Table;

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
    let dir = std::env::temp_dir().join(format!("l3i-dump-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let memory = dir.join("memory.txt");
    let heap = dir.join("heap.json");
    runtime.memory_dump(&memory).unwrap();
    runtime.gc_dump(&heap, Some(&[c"shared", c"scripts"])).unwrap();
    assert!(std::fs::metadata(&memory).unwrap().len() > 0);
    let heap_text = std::fs::read_to_string(&heap).unwrap();
    assert!(heap_text.contains("objects") || heap_text.contains('{'), "{}", &heap_text[..heap_text.len().min(200)]);
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
            if ptr.is_null() {
                self.allocations.set(self.allocations.get() + 1);
            } else {
                std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), fresh, old_size.min(new_size));
                dealloc(ptr.cast(), layout(old_size));
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

/// The first runtime of a process decides whether it has coroutine finalizers, and this binary's
/// other tests build runtimes without them. A scenario that needs them runs alone in a child
/// process of the binary. True in the parent, once the child has passed; false in the child.
fn passed_in_a_child_process(name: &str) -> bool {
    if std::env::var_os("L3I_FINALIZER_CHILD").is_some() {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("L3I_FINALIZER_CHILD", "1")
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("1 passed"), "the child ran the scenario: {stdout}");
    true
}

#[test]
fn coroutine_finalizers_are_refused_unless_the_first_runtime_chose_them() {
    let runtime = Runtime::new().unwrap();
    // Whichever runtime this process built first, it built it without finalizers.
    let error = Runtime::builder().coroutine_finalizers(true).build().err().unwrap().to_string();
    assert!(error.contains("first runtime of the process"), "{error}");
    assert!(!l3i::flags::coroutine_finalizers());
    assert!(runtime.eval::<bool>("return coroutine.finally == nil").unwrap());

    let body = runtime.load_function("return function() coroutine.yield() end").unwrap();
    let callback = runtime.load_function("return function() end").unwrap();
    let thread = runtime.new_thread().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    let error = runtime
        .stack()
        .with_frame(|frame| thread.add_finalizer(frame, callback.push_to(frame)?))
        .unwrap_err()
        .to_string();
    assert!(error.contains("Coroutine finalizers are off"), "{error}");
    assert!(!thread.has_finalizers());
    assert!(thread.finalizer_function(&runtime.stack(), true).is_err());
}

#[test]
fn coroutine_finalizers_run_when_asked() {
    if passed_in_a_child_process("memory::coroutine_finalizers_run_when_asked") {
        return;
    }
    let runtime = Runtime::builder().coroutine_finalizers(true).build().unwrap();
    assert!(l3i::flags::coroutine_finalizers());
    assert!(runtime.eval::<bool>("return type(coroutine.finally) == 'function'").unwrap());
    let body = runtime.load_function("return function() finalized = 'no' coroutine.yield() return 1 end").unwrap();
    let on_finish = runtime.load_function("return function(how) finalized = how end").unwrap();
    let finalized =
        || runtime.global("finalized").unwrap().with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap();
    let thread = runtime.new_thread().unwrap();
    // A fresh thread counts as finished for Luau: finalizers attach to a live coroutine.
    assert!(runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).is_err());
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    assert!(!thread.has_finalizers());
    assert!(thread.finalizer_function(&runtime.stack(), true).is_err(), "nothing is pending yet");
    runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).unwrap();
    assert!(thread.has_finalizers());
    assert!(thread.finalizer_function(&runtime.stack(), false).is_err(), "a suspended coroutine has not finished");
    assert!(matches!(thread.resume(&runtime.stack(), ()).unwrap(), Resume::Finished(_)));
    let finalize = thread.finalizer_function(&runtime.stack(), false).unwrap();
    finalize.invoke::<(), _>(&runtime.stack(), ()).unwrap();
    assert_eq!(finalized(), "finished");
    assert!(!thread.has_finalizers());
    assert!(finalize.invoke::<(), _>(&runtime.stack(), ()).is_err(), "the function works once");

    // Closing cancels a suspended coroutine: its finalizers hear of it and the thread is reset.
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).unwrap();
    let cancel = thread.finalizer_function(&runtime.stack(), true).unwrap();
    assert_eq!(finalized(), "no");
    cancel.invoke::<(), _>(&runtime.stack(), ()).unwrap();
    assert_eq!(finalized(), "cancelled");
    assert!(thread.is_reset() && !thread.has_finalizers());

    // Once a process has finalizers, a runtime built without asking has them too.
    let later = Runtime::new().unwrap();
    assert!(later.eval::<bool>("return type(coroutine.finally) == 'function'").unwrap());
}

#[test]
fn a_finalizer_registers_from_inside_a_lua_call() {
    use l3i::bind::Call;
    use l3i::value::Function;
    if passed_in_a_child_process("memory::a_finalizer_registers_from_inside_a_lua_call") {
        return;
    }
    let runtime = Runtime::builder().coroutine_finalizers(true).build().unwrap();
    let thread = Rc::new(runtime.new_thread().unwrap());
    let running = Rc::clone(&thread);
    // The callback is the call's second argument, so the value to register is the one the
    // binding pushed on top, not whatever sits in the native call's first slot.
    let watch = runtime
        .bind_function("dreamweave.test.watch", move |call: &Call, _tag: f64, callback: Function| -> l3i::Result<()> {
            call.with_frame(|frame| running.add_finalizer(frame, callback.push_to(frame)?))
        })
        .unwrap();
    runtime.set_global("watch", &watch).unwrap();
    let body = runtime
        .load_function("return function() watch(42, function(how) finalized = how end) coroutine.yield() end")
        .unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    assert!(thread.has_finalizers());
    assert!(matches!(thread.resume(&runtime.stack(), ()).unwrap(), Resume::Finished(_)));
    let finalize = thread.finalizer_function(&runtime.stack(), false).unwrap();
    finalize.invoke::<(), _>(&runtime.stack(), ()).unwrap();
    assert_eq!(
        runtime.global("finalized").unwrap().with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap(),
        "finished"
    );

    // A thread of another VM is refused before Luau is asked.
    let other = Runtime::new().unwrap();
    let foreign = other.new_thread().unwrap();
    let callback = runtime.load_function("return function() end").unwrap();
    let error = runtime
        .stack()
        .with_frame(|frame| foreign.add_finalizer(frame, callback.push_to(frame)?))
        .unwrap_err()
        .to_string();
    assert!(error.contains("different VM"), "{error}");
}

#[test]
fn a_failed_coroutine_hands_its_error_to_its_finalizers() {
    if passed_in_a_child_process("memory::a_failed_coroutine_hands_its_error_to_its_finalizers") {
        return;
    }
    let runtime = Runtime::builder().coroutine_finalizers(true).build().unwrap();
    let body = runtime.load_function("return function() coroutine.yield() error('boom') end").unwrap();
    let on_finish =
        runtime.load_function("return function(how, problem) finalized = how .. ': ' .. problem end").unwrap();
    let thread = runtime.new_thread().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    runtime.stack().with_frame(|frame| thread.add_finalizer(frame, on_finish.push_to(frame)?)).unwrap();
    let reported = thread.resume(&runtime.stack(), ()).unwrap_err().to_string();
    let message = reported.strip_prefix("Lua error: ").unwrap();
    assert!(message.ends_with(":1: boom"), "{message}");

    // `resume` reported the failure and left the error object for the finalizers: the message
    // with its position, not the `'boom'` argument still lying in the dead frame under it. The
    // function runs them and then raises the error again, as Luau does for a failed coroutine.
    let finalize = thread.finalizer_function(&runtime.stack(), false).unwrap();
    let raised = finalize.invoke::<(), _>(&runtime.stack(), ()).unwrap_err().to_string();
    assert!(raised.ends_with(message), "{raised}");
    let finalized =
        runtime.global("finalized").unwrap().with_value(&runtime.stack(), |_, v| v.read::<String>()).unwrap();
    assert_eq!(finalized, format!("error: {message}"));
    assert!(!thread.has_finalizers());

    // The thread starts over after a reset, and a failure with nothing pending has no function.
    thread.reset().unwrap();
    assert!(matches!(thread.start(&runtime.stack(), &body, ()).unwrap(), Resume::Yielded(_)));
    assert!(thread.resume(&runtime.stack(), ()).is_err());
    assert!(thread.finalizer_function(&runtime.stack(), false).is_err(), "nothing is pending");
    thread.reset().unwrap();
    assert!(thread.is_reset());
}

#[test]
fn fast_flags_are_introspectable() {
    let _runtime = Runtime::new().unwrap();
    assert_eq!(memory::fast_flag("LuauFastpcall"), Some(true), "policy flag is on");
    assert_eq!(memory::fast_flag("NoSuchFlagAtAll"), None);
    assert!(memory::fast_flags().iter().any(|(name, _)| name == "LuauFastpcall"));
    // Writes the value the policy already set: other tests' VMs share these process-wide flags.
    memory::set_fast_flag("LuauFastpcall", true).unwrap();
    assert!(memory::set_fast_flag("NoSuchFlagAtAll", true).is_err());
    assert!(memory::set_fast_int("NoSuchIntAtAll", 1).is_err());
    assert_eq!(memory::fast_int("NoSuchIntAtAll"), None);
    assert!(memory::fast_int("LuauInlineHitsThreshold").is_some());
}

#[test]
fn host_pushes_past_the_stack_limit_fail_instead_of_aborting() {
    let runtime = Runtime::new().unwrap();
    let stack = runtime.stack();
    for push in [0, 1, 2] {
        let error = stack
            .with_frame(|frame| {
                for index in 0..100_000 {
                    match push {
                        0 => frame.push_number(f64::from(index)).map(drop)?,
                        1 => frame.push_string("slot").map(drop)?,
                        _ => frame.push_table(0, 0).map(drop)?,
                    }
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("stack overflow"), "{error}");
        assert_eq!(stack.top(), 0);
    }
    let error = stack
        .with_frame(|frame| {
            for _ in 0..100_000 {
                frame.push(&"argument")?;
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("stack overflow"), "{error}");
    drop(stack);
    runtime.exec("local x = 1").unwrap();
}
