//! The extension planner's synthetic gates (`L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §78, §79).

use std::cell::Cell;
use std::rc::Rc;

use l3i::direct::field::{DirectField, FieldValue};
use l3i::extension::{Extension, ExtensionDescriptor, InstallContext, RuntimePlan, RuntimePolicy, TagPolicy};
use l3i::runtime::MemoryCategory;
use l3i::source::CompileConstant;
use l3i::userdata::{Owned, Userdata};
use l3i::{Error, Result, Runtime, TAG_LIMIT};

struct Counter {
    value: Cell<i64>,
}

unsafe impl Userdata for Counter {
    const NAME: &'static str = "dream.tests.Counter";
}

struct Other;

unsafe impl Userdata for Other {
    const NAME: &'static str = "dream.tests.Other";
}

struct ValueField;

impl DirectField<Counter> for ValueField {
    fn get(counter: &Counter) -> FieldValue {
        FieldValue::Integer(counter.value.get())
    }
}

/// A host service extensions may ask for.
struct Greeting(String);

/// `dream.core`: owns `Counter`, provides `@dream/core`.
struct Core {
    tag: TagPolicy,
    with_field: bool,
}

impl Core {
    fn preferred() -> Self {
        Core { tag: TagPolicy::Preferred, with_field: true }
    }
}

impl Extension for Core {
    fn id(&self) -> &'static str {
        "dream.core"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        let counter = d.userdata::<Counter>("dream.tests.Counter");
        counter.tag(self.tag).doc("A counter.");
        counter.method("get").direct().signature("(self): number");
        counter.method("add").direct();
        counter.getter("twice").signature("number");
        counter.setter("twice");
        if self.with_field {
            counter.field("value").signature("number");
        }
        d.module("@dream/core").doc("Counters.");
        d.memory_category("dream.core");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let category = cx.memory_category("dream.core")?;
        assert_eq!(category, MemoryCategory(1));
        let mut counter = cx.userdata::<Counter>("dream.tests.Counter")?;
        counter
            .method("get", |c: &Counter| c.value.get())?
            .method("add", |c: &Counter, n: i64| c.value.set(c.value.get() + n))?
            .property("twice", |c: &Counter| c.value.get() * 2, |c: &Counter, v: i64| c.value.set(v / 2))?
            .metamethod("__tostring", |c: &Counter| format!("Counter({})", c.value.get()))?;
        if self.with_field {
            counter.field::<ValueField>("value")?;
        }
        let mut module = cx.module("@dream/core")?;
        module
            .function("new", |n: i64| Owned(Counter { value: Cell::new(n) }))?
            .constant("ANSWER", CompileConstant::Number(42.0))?
            .constant("LIMIT", CompileConstant::Integer(7))?
            .constant("NAME", CompileConstant::String("core".to_owned()))?;
        module.finish()?;
        Ok(())
    }
}

/// `dream.tools`: requires `dream.core`, augments `Counter`.
struct Tools;

impl Extension for Tools {
    fn id(&self) -> &'static str {
        "dream.tools"
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.requires("dream.core");
        let counter = d.augment_userdata::<Counter>("dream.tests.Counter");
        counter.method("double").direct();
        counter.method("describe");
        d.module("@dream/tools");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        cx.userdata::<Counter>("dream.tests.Counter")?
            .method("double", |c: &Counter| c.value.get() * 2)?
            .method("describe", |c: &Counter| format!("counter at {}", c.value.get()))?;
        let mut module = cx.module("@dream/tools")?;
        module.function("version", || 2i64)?;
        module.finish()?;
        Ok(())
    }
}

const SCRIPT: &str = "local core = require('@dream/core') local tools = require('@dream/tools') \
    local c = core.new(5) assert(c:get() == 5, 'get') c:add(2) assert(c:get() == 7, 'add') \
    assert(c:double() == 14, 'double') assert(c:describe() == 'counter at 7', 'describe') \
    assert(c.twice == 14, 'twice get') c.twice = 20 assert(c:get() == 10, 'twice set') \
    assert(tostring(c) == 'Counter(10)', 'tostring') assert(tools.version() == 2, 'version') assert(core.ANSWER == 42, 'answer') \
    assert(core.LIMIT == 7i, 'limit') assert(c.missing == nil, 'missing')";

fn plan_with(core: Core, policy: RuntimePolicy) -> Rc<RuntimePlan> {
    // Tools registered first: order comes from the dependency graph, not registration.
    RuntimePlan::builder().policy(policy).extension(Tools).extension(core).finalize().unwrap()
}

#[test]
fn dependency_order_composition_and_direct_dispatch() {
    let plan = plan_with(Core::preferred(), RuntimePolicy::new());
    assert_eq!(plan.installation_order(), ["dream.core", "dream.tools"]);
    let counter = plan.userdata_by_key("dream.tests.Counter").unwrap();
    assert_eq!(counter.owner, "dream.core");
    assert_eq!(counter.tag, Some(1));
    let names: Vec<&str> = counter.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["get", "add", "twice", "twice", "value", "double", "describe"]);
    // Every method, getter, and setter of a tagged type has a dense slot; direct fields none.
    let slots: Vec<Option<u16>> = counter.members.iter().map(|m| m.slot).collect();
    assert_eq!(slots, [Some(1), Some(2), Some(3), Some(4), None, Some(5), Some(6)]);
    // Atoms are dense over the sorted member names; direct field names get none.
    assert_eq!(plan.atom_of("add"), Some(1));
    assert_eq!(plan.atom_of("twice"), Some(5));
    assert_eq!(plan.atom_of("value"), None);

    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(SCRIPT).unwrap();
    runtime.exec("local core = require('@dream/core') local c = core.new(1) assert(c.value == 1i, 'field') c:add(41) assert(c.value == 42i, 'field after add')").unwrap();
    // A frozen module refuses writes.
    let error = runtime.exec("local core = require('@dream/core') core.new = nil").unwrap_err().to_string();
    assert!(error.contains("readonly"), "{error}");
}

#[test]
fn tagged_and_untagged_runtimes_agree() {
    let tagged = plan_with(Core::preferred(), RuntimePolicy::new());
    let untagged = plan_with(Core { tag: TagPolicy::Never, with_field: false }, RuntimePolicy::new());
    assert_eq!(tagged.tag_of("dream.tests.Counter"), Some(1));
    assert_eq!(untagged.tag_of("dream.tests.Counter"), None);
    assert!(untagged.userdata_by_key("dream.tests.Counter").unwrap().members.iter().all(|m| m.slot.is_none()));
    for plan in [&tagged, &untagged] {
        let runtime = Runtime::from_plan(plan).unwrap();
        runtime.exec(SCRIPT).unwrap();
    }
}

#[test]
fn per_vm_tags_and_atoms_differ_with_identical_semantics() {
    let a = plan_with(Core::preferred(), RuntimePolicy::new().first_tag(7));
    let b = plan_with(Core::preferred(), RuntimePolicy::new().first_tag(40));
    assert_eq!(a.tag_of("dream.tests.Counter"), Some(7));
    assert_eq!(b.tag_of("dream.tests.Counter"), Some(40));
    // Another extension in one plan shifts the atom of a shared member name.
    struct Extra;
    impl Extension for Extra {
        fn id(&self) -> &'static str {
            "dream.aaa"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Other>("dream.tests.Other").method("aardvark");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            cx.userdata::<Other>("dream.tests.Other")?.method("aardvark", |_: &Other| 1i64)?;
            Ok(())
        }
    }
    let c = RuntimePlan::builder().extension(Core::preferred()).extension(Tools).extension(Extra).finalize().unwrap();
    assert_ne!(a.atom_of("get"), c.atom_of("get"));
    // Runtimes from all three plans live at once in this process and agree.
    let runtimes: Vec<Runtime> = [&a, &b, &c].into_iter().map(|plan| Runtime::from_plan(plan).unwrap()).collect();
    for runtime in &runtimes {
        runtime.exec(SCRIPT).unwrap();
    }
}

#[test]
fn stale_direct_cache_is_rejected_across_types() {
    // Two tagged types with a method of the same name at one call site: the per-instruction
    // cache alternates between them and must never dispatch to the wrong entry.
    struct Pair;
    impl Extension for Pair {
        fn id(&self) -> &'static str {
            "dream.pair"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Counter>("dream.tests.Counter").tag(TagPolicy::Required).method("get").direct();
            d.userdata::<Other>("dream.tests.Other").tag(TagPolicy::Required).method("get").direct();
            d.module("@dream/pair");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            cx.userdata::<Counter>("dream.tests.Counter")?.method("get", |c: &Counter| c.value.get())?;
            cx.userdata::<Other>("dream.tests.Other")?.method("get", |_: &Other| -1i64)?;
            let mut module = cx.module("@dream/pair")?;
            module
                .function("counter", |n: i64| Owned(Counter { value: Cell::new(n) }))?
                .function("other", || Owned(Other))?;
            module.finish()?;
            Ok(())
        }
    }
    let plan = RuntimePlan::builder().extension(Pair).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local pair = require('@dream/pair') local items = { pair.counter(3), pair.other(), pair.counter(9), pair.other() } \
             local expected = { 3, -1, 9, -1 } \
             for round = 1, 50 do for i, item in ipairs(items) do assert(item:get() == expected[i]) end end",
        )
        .unwrap();
}

#[test]
fn compiler_metadata_and_type_definitions_follow_composition() {
    let policy = RuntimePolicy::new().compat_global("@dream/core", "core");
    let plan = plan_with(Core::preferred(), policy);
    let runtime = Runtime::from_plan(&plan).unwrap();
    let options = runtime.compile_options();
    assert_eq!(options.known_libraries, vec![std::ffi::CString::new("core").unwrap()]);
    let members = options.library_members.as_ref().unwrap();
    assert_eq!(members.member_constant("core", "ANSWER"), Some(CompileConstant::Number(42.0)));
    assert_eq!(members.member_constant("core", "LIMIT"), Some(CompileConstant::Integer(7)));
    assert_eq!(members.member_type("core", "ANSWER"), Some(2));
    assert_eq!(members.member_type("core", "LIMIT"), Some(10));
    assert_eq!(members.member_type("core", "new"), Some(5));
    assert_eq!(members.member_type("core", "missing"), None);
    assert_eq!(options.userdata_types, vec![std::ffi::CString::new("dream.tests.Counter").unwrap()]);
    // The compat global works and the folded constant reads the same.
    runtime.exec("assert(core.ANSWER == 42, 'answer') assert(core.LIMIT == 7i, 'limit') assert(core.NAME == 'core', 'name') assert(core.new(1):get() == 1, 'get')").unwrap();

    let definitions = runtime.type_definitions().unwrap();
    assert!(definitions.contains("declare class dream_tests_Counter"), "{definitions}");
    assert!(definitions.contains("function get(self): number"), "{definitions}");
    assert!(definitions.contains("function double(self, ...any): any"), "{definitions}");
    assert!(definitions.contains("    twice: number"), "{definitions}");
    assert!(definitions.contains("    value: number"), "{definitions}");
    assert!(definitions.contains("export type Module__dream_core = {"), "{definitions}");
    assert!(definitions.contains("    ANSWER: number,"), "{definitions}");
    assert!(definitions.contains("    LIMIT: number,"), "{definitions}");
    assert!(definitions.contains("declare core: Module__dream_core"), "{definitions}");
    // The plan alone renders the userdata and module stubs.
    assert!(plan.type_definitions().contains("declare class dream_tests_Counter"));
}

#[test]
fn services_capabilities_state_and_drop_order() {
    struct Needy {
        dropped: Rc<Cell<bool>>,
    }
    struct State {
        pinned: l3i::value::Value,
        dropped: Rc<Cell<bool>>,
    }
    impl Drop for State {
        fn drop(&mut self) {
            assert!(self.pinned.is_valid(), "state dropped after the VM closed");
            self.dropped.set(true);
        }
    }
    impl Extension for Needy {
        fn id(&self) -> &'static str {
            "dream.needy"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.service::<Greeting>();
            d.capability("filesystem.read");
            d.module("@dream/needy");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            let greeting = cx.service::<Greeting>()?;
            cx.require_capability("filesystem.read")?;
            assert!(!cx.has_capability("filesystem.write"));
            let pinned = cx.runtime().load_function("return function() end").unwrap().into_value();
            cx.insert_state(State { pinned, dropped: Rc::clone(&self.dropped) });
            let text = greeting.0.clone();
            let mut module = cx.module("@dream/needy")?;
            module.function("greet", move || text.clone())?;
            module.finish()?;
            Ok(())
        }
    }
    let dropped = Rc::new(Cell::new(false));
    // Missing service.
    let error = RuntimePlan::builder()
        .policy(RuntimePolicy::new().capability("filesystem.read"))
        .extension(Needy { dropped: Rc::clone(&dropped) })
        .finalize()
        .err()
        .unwrap();
    assert!(error.to_string().contains("requires host service"), "{error}");
    // Denied capability is a permission error, not a runtime one.
    let error = RuntimePlan::builder()
        .service(Greeting("hi".to_owned()))
        .extension(Needy { dropped: Rc::clone(&dropped) })
        .finalize()
        .err()
        .unwrap();
    assert!(matches!(error, Error::Permission(_)), "{error:?}");
    // Both present.
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().capability("filesystem.read"))
        .service(Greeting("hello".to_owned()))
        .extension(Needy { dropped: Rc::clone(&dropped) })
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec("assert(require('@dream/needy').greet() == 'hello')").unwrap();
    assert!(runtime.extension_state::<State>().is_some());
    assert!(!dropped.get());
    drop(runtime);
    assert!(dropped.get(), "extension state must drop with the runtime, before lua_close");
}

#[test]
fn finalization_rejects_bad_compositions() {
    struct Bare(&'static str, Vec<&'static str>);
    impl Extension for Bare {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            for id in &self.1 {
                d.requires(id);
            }
            Ok(())
        }
        fn install(&self, _: &mut InstallContext<'_>) -> Result<()> {
            Ok(())
        }
    }
    let text = |result: std::result::Result<Rc<RuntimePlan>, Error>| result.err().unwrap().to_string();

    // Missing requirement.
    let error = text(RuntimePlan::builder().extension(Tools).finalize());
    assert!(error.contains("requires 'dream.core', which is not in the plan"), "{error}");
    // Cycle.
    let error = text(RuntimePlan::builder().extension(Bare("a", vec!["b"])).extension(Bare("b", vec!["a"])).finalize());
    assert!(error.contains("dependency cycle: a -> b -> a"), "{error}");
    // Duplicate id.
    let error = text(RuntimePlan::builder().extension(Bare("a", vec![])).extension(Bare("a", vec![])).finalize());
    assert!(error.contains("registered twice"), "{error}");
    // Deterministic order among independents: lexicographic.
    let plan =
        RuntimePlan::builder().extension(Bare("zeta", vec![])).extension(Bare("alpha", vec![])).finalize().unwrap();
    assert_eq!(plan.installation_order(), ["alpha", "zeta"]);

    // Duplicate module.
    struct Dup(&'static str);
    impl Extension for Dup {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.module("@dream/dup");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            cx.module("@dream/dup")?.finish()?;
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Dup("x")).extension(Dup("y")).finalize());
    assert!(error.contains("module '@dream/dup' is provided twice"), "{error}");

    // Duplicate owner.
    struct Owner(&'static str);
    impl Extension for Owner {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Counter>("dream.tests.Counter").method("get");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            cx.userdata::<Counter>("dream.tests.Counter")?.method("get", |c: &Counter| c.value.get())?;
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Owner("p")).extension(Owner("q")).finalize());
    assert!(error.contains("owned by both"), "{error}");

    // Augmentation with the wrong Rust type, augmentation without requiring the owner, and a
    // member declared twice.
    struct BadAugment(u8);
    impl Extension for BadAugment {
        fn id(&self) -> &'static str {
            "dream.bad"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            match self.0 {
                0 => {
                    d.requires("dream.core");
                    d.augment_userdata::<Other>("dream.tests.Counter").method("x");
                }
                1 => {
                    d.augment_userdata::<Counter>("dream.tests.Counter").method("x");
                }
                _ => {
                    d.requires("dream.core");
                    d.augment_userdata::<Counter>("dream.tests.Counter").method("get");
                }
            }
            Ok(())
        }
        fn install(&self, _: &mut InstallContext<'_>) -> Result<()> {
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(0)).finalize());
    assert!(error.contains("with Rust type dream.tests.Other"), "{error}");
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(1)).finalize());
    assert!(error.contains("does not require its owner"), "{error}");
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(2)).finalize());
    assert!(error.contains("member 'get' is declared by both"), "{error}");

    // A direct field name shared with another member kind cannot keep its fast path.
    struct FieldClash;
    impl Extension for FieldClash {
        fn id(&self) -> &'static str {
            "dream.clash"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.requires("dream.core");
            d.userdata::<Other>("dream.tests.Other").method("value");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            cx.userdata::<Other>("dream.tests.Other")?.method("value", |_: &Other| 0i64)?;
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(FieldClash).finalize());
    assert!(error.contains("'value' is a direct field on one type"), "{error}");

    // Tags: Never with a direct field, and running out of tags for Required types.
    let error = text(RuntimePlan::builder().extension(Core { tag: TagPolicy::Never, with_field: true }).finalize());
    assert!(error.contains("direct fields, which need a tag"), "{error}");
    let error = text(
        RuntimePlan::builder()
            .policy(RuntimePolicy::new().first_tag(TAG_LIMIT - 1))
            .extension(Core { tag: TagPolicy::Required, with_field: false })
            .extension(Owner("dream.second"))
            .finalize(),
    );
    assert!(error.contains("owned by both") || error.contains("no Luau tag left"), "{error}");
    // Pinned tags win, and a pinned tag for an unknown key is an error.
    let plan =
        RuntimePlan::builder().pin_tag("dream.tests.Counter", 99).extension(Core::preferred()).finalize().unwrap();
    assert_eq!(plan.tag_of("dream.tests.Counter"), Some(99));
    let error = text(RuntimePlan::builder().pin_tag("dream.tests.Nope", 5).extension(Core::preferred()).finalize());
    assert!(error.contains("which no extension owns"), "{error}");
}

#[test]
fn install_must_match_declarations() {
    struct Sloppy(u8);
    impl Extension for Sloppy {
        fn id(&self) -> &'static str {
            "dream.sloppy"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Counter>("dream.tests.Counter").method("get");
            d.module("@dream/sloppy");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            match self.0 {
                0 => {
                    // Undeclared member.
                    cx.userdata::<Counter>("dream.tests.Counter")?.method("nope", |c: &Counter| c.value.get())?;
                }
                1 => {
                    // Declared member left out; module installed.
                    cx.module("@dream/sloppy")?.finish()?;
                }
                _ => {
                    // Member installed, module never finished.
                    cx.userdata::<Counter>("dream.tests.Counter")?.method("get", |c: &Counter| c.value.get())?;
                }
            }
            Ok(())
        }
    }
    for (variant, expected) in
        [(0, "installs undeclared Method 'nope'"), (1, "did not install it"), (2, "did not install it")]
    {
        let plan = RuntimePlan::builder().extension(Sloppy(variant)).finalize().unwrap();
        let error = Runtime::from_plan(&plan).err().unwrap().to_string();
        assert!(error.contains(expected), "variant {variant}: {error}");
    }
}

#[test]
fn generated_dispatch_survives_collection_before_first_use() {
    let plan = plan_with(Core::preferred(), RuntimePolicy::new());
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.collect_garbage();
    runtime.collect_garbage();
    runtime.exec(SCRIPT).unwrap();
}

#[test]
fn sandboxed_policy_freezes_globals() {
    let plan = plan_with(Core::preferred(), RuntimePolicy::new().sandbox(true).compat_global("@dream/core", "core"));
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec("assert(core.new(2):get() == 2)").unwrap();
    let error = runtime.exec("newGlobal = 1").unwrap_err().to_string();
    assert!(error.contains("readonly"), "{error}");
}
