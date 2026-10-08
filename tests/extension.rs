//! The extension planner's synthetic gates (`L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §78, §79).

use std::cell::Cell;
use std::rc::Rc;

use l3i::direct::field::{DirectField, FieldValue};
use l3i::extension::{
    COMPILER_TYPE_CAPACITY, CompilerTypePolicy, Extension, ExtensionDescriptor, InstallContext, RuntimePlan,
    RuntimePolicy, TagPolicy,
};
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

#[derive(Clone)]
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
        let mut counter = d.userdata::<Counter>("dream.tests.Counter");
        counter.tag(self.tag).doc("A counter.");
        counter.method("get", |c: &Counter| c.value.get()).signature("(self): number");
        counter.method("add", |c: &Counter, n: i64| c.value.set(c.value.get() + n)).untyped();
        counter
            .property("twice", |c: &Counter| c.value.get() * 2, |c: &Counter, v: i64| c.value.set(v / 2))
            .signature("number");
        counter.metamethod("__tostring", |c: &Counter| format!("Counter({})", c.value.get()));
        if self.with_field {
            counter.field::<ValueField>("value").signature("number");
        }
        d.module("@dream/core")
            .doc("Counters.")
            .function("new", |n: i64| Owned(Counter { value: Cell::new(n) }))
            .untyped()
            .constant("ANSWER", CompileConstant::Number(42.0))
            .constant("LIMIT", CompileConstant::Integer(7))
            .constant("NAME", CompileConstant::String("core".to_owned()));
        d.memory_category("dream.core");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let category = cx.memory_category("dream.core")?;
        assert_eq!(category, MemoryCategory(1));
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
        let mut counter = d.augment_userdata::<Counter>("dream.tests.Counter");
        counter.method("double", |c: &Counter| c.value.get() * 2).untyped();
        counter.method("describe", |c: &Counter| format!("counter at {}", c.value.get())).untyped();
        d.module("@dream/tools").function("version", || 2i64).untyped();
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
    assert_eq!(plan.installation_order(), ["dream.core", "dream.tools", "dream.udp"]);
    let counter = plan.userdata_by_key("dream.tests.Counter").unwrap();
    assert_eq!(counter.owner, "dream.core");
    assert_eq!(counter.tag, Some(1), "the network bridge's Client keys after it");
    let names: Vec<&str> = counter.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["get", "add", "twice", "twice", "value", "double", "describe"]);
    // Every method, getter, and setter of a tagged type has a dense slot; direct fields none.
    let slots: Vec<Option<u16>> = counter.members.iter().map(|m| m.slot).collect();
    // Slots are dense across the plan in key order; the network bridge's Client comes after.
    let base = slots[0].expect("the first method has a slot");
    assert_eq!(
        slots,
        [Some(base), Some(base + 1), Some(base + 2), Some(base + 3), None, Some(base + 4), Some(base + 5)]
    );
    // Atoms are dense over the sorted member names; direct field names get none.
    assert_eq!(plan.atom_of("add"), Some(1));
    assert!(plan.atom_of("twice").is_some_and(|atom| atom > 1), "every method and property name has an atom");
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
    // Tags follow key order; the network bridge's Client (`dream.udp.Client`) sorts after Counter.
    assert_eq!(tagged.tag_of("dream.tests.Counter"), Some(1));
    assert_eq!(tagged.tag_of("dream.udp.Client"), Some(2));
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
            d.userdata::<Other>("dream.tests.Other").method("aardvark", |_: &Other| 1i64).untyped();
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
            d.userdata::<Counter>("dream.tests.Counter")
                .tag(TagPolicy::Required)
                .method("get", |c: &Counter| c.value.get())
                .untyped();
            d.userdata::<Other>("dream.tests.Other")
                .tag(TagPolicy::Required)
                .method("get", |_: &Other| -1i64)
                .untyped();
            d.module("@dream/pair")
                .function("counter", |n: i64| Owned(Counter { value: Cell::new(n) }))
                .untyped()
                .function("other", || Owned(Other))
                .untyped();
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
    // Userdata types reach the compiler under the class name scripts annotate, in slot order
    // (tag order among equal policies): the network bridge's types are in every plan.
    let names: Vec<&str> = options.userdata_types.iter().map(|n| n.to_str().unwrap()).collect();
    assert_eq!(names, ["dream_tests_Counter", "dream_udp_Client", "dream_udp_Server"]);
    // The compat global works and the folded constant reads the same.
    runtime.exec("assert(core.ANSWER == 42, 'answer') assert(core.LIMIT == 7i, 'limit') assert(core.NAME == 'core', 'name') assert(core.new(1):get() == 1, 'get')").unwrap();

    let definitions = runtime.type_definitions().unwrap();
    assert!(definitions.contains("declare extern type dream_tests_Counter"), "{definitions}");
    assert!(definitions.contains("function get(self): number"), "{definitions}");
    assert!(definitions.contains("function double(self, ...any): any"), "{definitions}");
    assert!(definitions.contains("    twice: number"), "{definitions}");
    assert!(definitions.contains("    value: number"), "{definitions}");
    assert!(definitions.contains("export type Module__dream_core = {"), "{definitions}");
    assert!(definitions.contains("    ANSWER: number,"), "{definitions}");
    assert!(definitions.contains("    LIMIT: integer,"), "{definitions}");
    assert!(definitions.contains("declare core: Module__dream_core"), "{definitions}");
    // The plan alone renders the userdata and module stubs.
    assert!(plan.type_definitions().contains("declare extern type dream_tests_Counter"));
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
            d.optional_capability("filesystem.write");
            d.module("@dream/needy").installed("greet").signature("() -> string");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            let greeting = cx.service::<Greeting>()?;
            cx.require_capability("filesystem.read")?;
            assert!(cx.has_capability("filesystem.read")?);
            assert!(!cx.has_capability("filesystem.write")?, "declared optional, not granted");
            let error = cx.has_capability("filesytem.write").unwrap_err().to_string();
            assert!(error.contains("checks capability 'filesytem.write' without declaring it"), "{error}");
            let pinned = cx.runtime().load_function("return function() end").unwrap().into_value();
            cx.insert_state(State { pinned, dropped: Rc::clone(&self.dropped) });
            let text = greeting.0.clone();
            // A module function that depends on a service binds at install, next to the
            // declared ones.
            cx.module("@dream/needy")?.function("greet", move || text.clone())?;
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
    assert!(runtime.state_of::<State>("dream.needy").is_some());
    assert!(runtime.host_state::<State>().is_none(), "an extension's state is not the host's");
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
    // Deterministic order among independents: lexicographic; the network bridge is in every
    // plan without being asked for, and the plan knows its module.
    let plan =
        RuntimePlan::builder().extension(Bare("zeta", vec![])).extension(Bare("alpha", vec![])).finalize().unwrap();
    assert_eq!(plan.installation_order(), ["alpha", "dream.udp", "zeta"]);
    assert!(plan.modules().iter().any(|m| m.path == "@dream/udp"));
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec("local udp = require('@dream/udp') assert(type(udp.schema) == 'function')").unwrap();
    // The id is reserved: nothing can stand in for the bridge.
    let error = text(RuntimePlan::builder().extension(Bare("dream.udp", vec![])).finalize());
    assert!(error.contains("'dream.udp' is reserved for l3i's network bridge"), "{error}");

    // Names that fold to one identifier: debug prefixes, generated class and module type
    // names; and compat globals, one per module and one module per global.
    let error = text(
        RuntimePlan::builder().extension(Bare("dream.a-b", vec![])).extension(Bare("dream.a_b", vec![])).finalize(),
    );
    assert!(error.contains("share the debug prefix 'dream.a_b'"), "{error}");
    struct Mod(&'static str, &'static str);
    impl Extension for Mod {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.module(self.1);
            Ok(())
        }
    }
    let error =
        text(RuntimePlan::builder().extension(Mod("a", "@dream/x-y")).extension(Mod("b", "@dream/x_y")).finalize());
    assert!(error.contains("would share the generated type name 'Module__dream_x_y'"), "{error}");
    let error = text(
        RuntimePlan::builder()
            .policy(RuntimePolicy::new().compat_global("@dream/a", "g").compat_global("@dream/b", "g"))
            .extension(Mod("a", "@dream/a"))
            .extension(Mod("b", "@dream/b"))
            .finalize(),
    );
    assert!(error.contains("compat global 'g' is mapped to both '@dream/a' and '@dream/b'"), "{error}");
    let error = text(
        RuntimePlan::builder()
            .policy(RuntimePolicy::new().compat_global("@dream/a", "g").compat_global("@dream/a", "h"))
            .extension(Mod("a", "@dream/a"))
            .finalize(),
    );
    assert!(error.contains("module '@dream/a' is exposed as two compat globals"), "{error}");
    struct Keyed(&'static str, &'static str, bool);
    impl Extension for Keyed {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            if self.2 {
                d.userdata::<Counter>(self.1);
            } else {
                d.userdata::<Other>(self.1);
            }
            Ok(())
        }
    }
    let error = text(
        RuntimePlan::builder()
            .extension(Keyed("a", "dream.x-y.T", true))
            .extension(Keyed("b", "dream.x_y.T", false))
            .finalize(),
    );
    assert!(error.contains("would share the generated class name 'dream_x_y_T'"), "{error}");

    // Identities and spellings the generated definitions and the VM would choke on fail here.
    struct Twin;
    // SAFETY: a plain unit type; its NAME deliberately repeats Other's.
    unsafe impl Userdata for Twin {
        const NAME: &'static str = "dream.tests.Other";
    }
    struct Named(&'static str, u8);
    impl Extension for Named {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            match self.1 {
                0 => {
                    d.userdata::<Other>("dream.tests.Other")
                        .method("get", |_: &Other| 1i64)
                        .signature("(self): number");
                    d.userdata::<Twin>("dream.tests.Twin").method("get", |_: &Twin| 1i64).signature("(self): number");
                }
                1 => {
                    d.userdata::<Other>("dream.tests.Other")
                        .method("bad-name", |_: &Other| 1i64)
                        .signature("(self): number");
                }
                2 => {
                    d.module("@dream/spaced path").function("f", || 1i64).signature("() -> number");
                }
                _ => {
                    d.module("@dream/named").function("end", || 1i64).signature("() -> number");
                }
            }
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Named("dream.named", 0)).finalize());
    assert!(error.contains("share the Luau type name 'dream.tests.Other'"), "{error}");
    let error = text(RuntimePlan::builder().extension(Named("dream.named", 1)).finalize());
    assert!(error.contains("member of 'dream.tests.Other' 'bad-name' is not a Luau identifier"), "{error}");
    let error = text(RuntimePlan::builder().extension(Named("dream.named", 2)).finalize());
    assert!(error.contains("module path '@dream/spaced path' must be an optional '@'"), "{error}");
    struct Spelled(u8);
    struct BadName;
    // SAFETY: a plain unit type; its NAME is deliberately malformed.
    unsafe impl Userdata for BadName {
        const NAME: &'static str = "dream.bad name";
    }
    impl Extension for Spelled {
        fn id(&self) -> &'static str {
            "dream.spelled"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            match self.0 {
                0 => {
                    d.module("@dream/quo\"te").function("f", || 1i64).signature("() -> number");
                }
                1 => {
                    d.userdata::<Other>("dream/tests/Other")
                        .method("get", |_: &Other| 1i64)
                        .signature("(self): number");
                }
                2 => {
                    d.userdata::<BadName>("dream.tests.BadName")
                        .method("get", |_: &BadName| 1i64)
                        .signature("(self): number");
                }
                _ => {
                    d.module("@dream/spelled").function("continue", || 1i64).signature("() -> number");
                }
            }
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Spelled(0)).finalize());
    assert!(error.contains("module path '@dream/quo\"te' must be"), "{error}");
    let error = text(RuntimePlan::builder().extension(Spelled(1)).finalize());
    assert!(error.contains("userdata key 'dream/tests/Other' must be"), "{error}");
    let error = text(RuntimePlan::builder().extension(Spelled(2)).finalize());
    assert!(error.contains("Luau type name 'dream.bad name', which is not dot-separated identifiers"), "{error}");
    let error = text(RuntimePlan::builder().extension(Spelled(3)).finalize());
    assert!(error.contains("'continue' is not a Luau identifier"), "{error}");
    let error = text(RuntimePlan::builder().extension(Named("dream.named", 3)).finalize());
    assert!(error.contains("member of module '@dream/named' 'end' is not a Luau identifier"), "{error}");
    let error = text(
        RuntimePlan::builder()
            .policy(RuntimePolicy::new().compat_global("@dream/core", "my core"))
            .extension(Core::preferred())
            .finalize(),
    );
    assert!(error.contains("compat global 'my core' is not a Luau identifier"), "{error}");

    // Types are never accidental: a member with neither a signature nor untyped() fails the plan.
    struct Unsigned;
    impl Extension for Unsigned {
        fn id(&self) -> &'static str {
            "dream.unsigned"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Other>("dream.tests.Other").method("mystery", |_: &Other| 1i64);
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Unsigned).finalize());
    assert!(
        error.contains("member 'mystery' of 'dream.tests.Other' (from 'dream.unsigned') has no signature"),
        "{error}"
    );
    struct UnsignedModule;
    impl Extension for UnsignedModule {
        fn id(&self) -> &'static str {
            "dream.unsignedmod"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.module("@dream/unsigned").installed("later");
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(UnsignedModule).finalize());
    assert!(error.contains("member 'later' of module '@dream/unsigned'") && error.contains("no signature"), "{error}");

    // Packed kinds: one type per number across the plan, l3i's numbers off limits.
    struct Kinded(&'static str, u8);
    struct KindA;
    struct KindB;
    macro_rules! kind {
        ($t:ident, $n:expr) => {
            impl l3i::packed::PackedScalar for $t {
                const KIND: u8 = $n;
                const NAME: &'static str = stringify!($t);
                fn pack(&self) -> (u64, u8) {
                    (0, 0)
                }
                fn unpack(_: u64, _: u8) -> Result<Self> {
                    Ok($t)
                }
            }
        };
    }
    kind!(KindA, 7);
    kind!(KindB, 7);
    struct KindLow;
    kind!(KindLow, 2);
    impl Extension for Kinded {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            match self.1 {
                0 => d.packed::<KindA>(),
                1 => d.packed::<KindB>(),
                _ => d.packed::<KindLow>(),
            };
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Kinded("a", 0)).extension(Kinded("b", 1)).finalize());
    assert!(error.contains("packed kind 7 is declared for KindA by 'a' and for KindB by 'b'"), "{error}");
    let error = text(RuntimePlan::builder().extension(Kinded("a", 2)).finalize());
    assert!(error.contains("kind 2, which belongs to l3i (AnimationKey)"), "{error}");
    let plan = RuntimePlan::builder().extension(Kinded("a", 0)).extension(Kinded("b", 0)).finalize().unwrap();
    assert_eq!(plan.packed_kinds().len(), 1, "the same type declared twice is one kind");
    // A planned runtime's kinds are the plan's; a hand-assembled one takes registrations.
    let runtime = Runtime::from_plan(&plan).unwrap();
    assert!(runtime.register_packed::<KindA>().is_err());
    let plain = Runtime::new().unwrap();
    plain.register_packed::<KindA>().unwrap();
    let error = plain.register_packed::<KindB>().unwrap_err().to_string();
    assert!(error.contains("already registered to KindA"), "{error}");

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
            d.userdata::<Counter>("dream.tests.Counter").method("get", |c: &Counter| c.value.get()).untyped();
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
                    d.augment_userdata::<Other>("dream.tests.Counter").method("x", |_: &Other| 0i64).untyped();
                }
                1 => {
                    d.augment_userdata::<Counter>("dream.tests.Counter").method("x", |_: &Counter| 0i64).untyped();
                }
                _ => {
                    d.requires("dream.core");
                    d.augment_userdata::<Counter>("dream.tests.Counter").method("get", |_: &Counter| 0i64).untyped();
                }
            }
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(0)).finalize());
    assert!(error.contains("with Rust type dream.tests.Other"), "{error}");
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(1)).finalize());
    assert!(error.contains("does not require its owner"), "{error}");
    let error = text(RuntimePlan::builder().extension(Core::preferred()).extension(BadAugment(2)).finalize());
    assert!(error.contains("member 'get' is declared by both"), "{error}");

    // A direct field name that is a method elsewhere keeps working: the planner gives the name an
    // atom and serves the field through a plan slot instead of Luau's field table.
    struct FieldClash;
    impl Extension for FieldClash {
        fn id(&self) -> &'static str {
            "dream.clash"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.requires("dream.core");
            d.userdata::<Other>("dream.tests.Other")
                .tag(TagPolicy::Required)
                .method("value", |_: &Other| 99i64)
                .untyped();
            d.module("@dream/clash").function("new", || Owned(Other)).untyped();
            Ok(())
        }
    }
    let plan = RuntimePlan::builder().extension(Core::preferred()).extension(FieldClash).finalize().unwrap();
    let field = plan.userdata_by_key("dream.tests.Counter").unwrap().member("value").unwrap();
    assert!(field.through_slot && field.slot.is_some() && plan.atom_of("value").is_some(), "{field:?}");
    assert!(!plan.userdata_by_key("dream.tests.Other").unwrap().member("value").unwrap().through_slot);
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local core = require('@dream/core') local clash = require('@dream/clash') \
             local c = core.new(4) local o = clash.new() \
             for _ = 1, 3 do assert(c.value == 4i, 'field through slot') assert(o:value() == 99, 'method') end \
             c:add(1) assert(c.value == 5i, 'field after add')",
        )
        .unwrap();
    // On one type the two meanings would share a key.
    struct SameTypeClash;
    impl Extension for SameTypeClash {
        fn id(&self) -> &'static str {
            "dream.sameclash"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            struct OtherValue;
            impl DirectField<Other> for OtherValue {
                fn get(_: &Other) -> FieldValue {
                    FieldValue::Integer(0)
                }
            }
            let mut other = d.userdata::<Other>("dream.tests.Other");
            other.field::<OtherValue>("value").untyped();
            other.method("value", |_: &Other| 0i64).untyped();
            Ok(())
        }
    }
    let error = text(RuntimePlan::builder().extension(SameTypeClash).finalize());
    assert!(error.contains("is declared twice by 'dream.sameclash' (as Field and Method)"), "{error}");

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
fn install_adds_to_declared_modules_and_rejects_duplicates() {
    struct Late(u8);
    impl Extension for Late {
        fn id(&self) -> &'static str {
            "dream.late"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Other>("dream.tests.Other").method("get", |_: &Other| 5i64).untyped();
            let module = d.module("@dream/late");
            module.function("declared", || 1i64).signature("() -> number");
            if self.0 != 3 {
                module.installed("installed").signature("() -> number").installed("instance").signature("Other");
            }
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            match self.0 {
                // Values that need the live VM: an instance of a declared type, a policy-bound function.
                0 => cx.module("@dream/late")?.function("installed", || 2i64)?.set("instance", &Owned(Other))?,
                // A declared function is bound from its declaration; install cannot replace it.
                1 => cx.module("@dream/late")?.function("declared", || 3i64)?,
                // A module another extension provides.
                2 => cx.module("@dream/core")?,
                // A member the plan never declared.
                3 => cx.module("@dream/late")?.function("surprise", || 4i64)?,
                // Declared for install, never provided.
                _ => cx.module("@dream/late")?,
            };
            Ok(())
        }
    }
    let plan = RuntimePlan::builder().extension(Late(0)).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "local late = require('@dream/late') assert(late.declared() == 1 and late.installed() == 2) \
             assert(late.instance:get() == 5, 'a declared type instance set at install')",
        )
        .unwrap();
    // The module is frozen after install, whoever added the member.
    let error = runtime.exec("require('@dream/late').declared = nil").unwrap_err().to_string();
    assert!(error.contains("readonly"), "{error}");
    // The plan knows the whole module shape before any runtime exists.
    let definitions = plan.type_definitions();
    assert!(definitions.contains("    declared: () -> number,"), "{definitions}");
    assert!(definitions.contains("    installed: () -> number,"), "{definitions}");
    assert!(definitions.contains("    instance: Other,"), "{definitions}");
    let instantiate = |late: Late| {
        Runtime::from_plan(&RuntimePlan::builder().extension(late).finalize().unwrap()).err().unwrap().to_string()
    };
    let error = instantiate(Late(1));
    assert!(error.contains("member 'declared' is bound from its declaration; install cannot replace it"), "{error}");
    let error = instantiate(Late(2));
    assert!(error.contains("which the plan does not know"), "{error}");
    let error = instantiate(Late(3));
    assert!(error.contains("member 'surprise' is not declared"), "{error}");
    let error = instantiate(Late(4));
    assert!(error.contains("declares member 'installed' for install, which 'dream.late' did not provide"), "{error}");
    // Declaring a name twice fails the plan.
    struct Twice;
    impl Extension for Twice {
        fn id(&self) -> &'static str {
            "dream.twice"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.module("@dream/twice").function("a", || 1i64).untyped().installed("a").untyped();
            Ok(())
        }
    }
    let error = RuntimePlan::builder().extension(Twice).finalize().err().unwrap().to_string();
    assert!(error.contains("module '@dream/twice' declares member 'a' twice"), "{error}");
}

// ---- compiler type slots ---------------------------------------------------------------------

macro_rules! slot_types {
    ($($t:ident),*) => {
        $(
            struct $t;
            // SAFETY: plain unit types.
            unsafe impl Userdata for $t {
                const NAME: &'static str = concat!("dream.slots.", stringify!($t));
            }
        )*
        /// Declares every slot type with `tag` and `compiler_type`.
        fn declare_slot_types(d: &mut ExtensionDescriptor, tag: TagPolicy, compiler_type: CompilerTypePolicy) {
            $(
                d.userdata::<$t>(concat!("dream.slots.", stringify!($t))).tag(tag).compiler_type(compiler_type);
            )*
        }
    };
}

slot_types!(
    S0, S1, S2, S3, S4, S5, S6, S7, S8, S9, S10, S11, S12, S13, S14, S15, S16, S17, S18, S19, S20, S21, S22, S23, S24,
    S25, S26, S27, S28, S29, S30, S31, S32, S33
);

struct Hot;
// SAFETY: a plain unit type.
unsafe impl Userdata for Hot {
    const NAME: &'static str = "dream.zlowered.Hot";
}

#[test]
fn compiler_type_slots_go_to_required_types_first_and_a_required_type_without_one_fails_the_plan() {
    struct Many(TagPolicy, CompilerTypePolicy);
    impl Extension for Many {
        fn id(&self) -> &'static str {
            "dream.slots"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            declare_slot_types(d, self.0, self.1);
            Ok(())
        }
    }
    struct Lowered;
    impl Extension for Lowered {
        fn id(&self) -> &'static str {
            "dream.zlowered"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.userdata::<Hot>("dream.zlowered.Hot")
                .tag(TagPolicy::Required)
                .compiler_type(CompilerTypePolicy::Required);
            Ok(())
        }
    }
    let text = |result: std::result::Result<Rc<RuntimePlan>, Error>| result.err().unwrap().to_string();

    // Thirty-four Required types: the thirty-third in tag order (tags follow the keys' order,
    // where "S8" sorts after "S33") fails the plan instead of silently losing its compiler type.
    let error =
        text(RuntimePlan::builder().extension(Many(TagPolicy::Required, CompilerTypePolicy::Required)).finalize());
    assert!(error.contains("no compiler type slot left for 'dream.slots.S8'"), "{error}");

    // Preferred types take what is left, in tag order, and the rest go without.
    let plan =
        RuntimePlan::builder().extension(Many(TagPolicy::Required, CompilerTypePolicy::Preferred)).finalize().unwrap();
    let typed: Vec<&str> =
        plan.userdata().iter().filter(|u| u.bytecode_type.is_some()).map(|u| u.key.as_str()).collect();
    assert_eq!(typed.len(), COMPILER_TYPE_CAPACITY);
    // `dream.udp.Client` keys after the slot types, which take every slot before it.
    assert_eq!(plan.userdata_by_key("dream.slots.S0").unwrap().bytecode_type, Some(64));
    assert_eq!(plan.userdata_by_key("dream.udp.Client").unwrap().bytecode_type, None);
    assert_eq!(plan.userdata_by_key("dream.slots.S8").unwrap().bytecode_type, None);

    // A Required type declared after thirty-four Preferred ones still gets the first slot.
    let plan = RuntimePlan::builder()
        .extension(Many(TagPolicy::Required, CompilerTypePolicy::Preferred))
        .extension(Lowered)
        .finalize()
        .unwrap();
    let hot = plan.userdata_by_key("dream.zlowered.Hot").unwrap();
    assert!(hot.tag.unwrap() > plan.userdata_by_key("dream.slots.S9").unwrap().tag.unwrap(), "keyed last, tagged last");
    assert_eq!(hot.bytecode_type, Some(64));
    assert_eq!(plan.userdata().iter().filter(|u| u.bytecode_type.is_some()).count(), COMPILER_TYPE_CAPACITY);
    let runtime = Runtime::from_plan(&plan).unwrap();
    assert_eq!(runtime.compile_options().userdata_types.len(), COMPILER_TYPE_CAPACITY);
    assert_eq!(runtime.compile_options().userdata_types[0].to_str().unwrap(), "dream_zlowered_Hot");

    // The built-ins whose methods lower natively keep their slots behind thirty-four Preferred
    // types: those declare Required.
    #[cfg(feature = "jit")]
    {
        let plan = RuntimePlan::builder()
            .extension(Many(TagPolicy::Required, CompilerTypePolicy::Preferred))
            .extension(l3i::quat::QuatExtension)
            .extension(l3i::raster::RasterExtension)
            .finalize()
            .unwrap();
        for key in ["dream.quat.Math", "dream.raster.Math"] {
            let resolved = plan.userdata_by_key(key).unwrap();
            assert_eq!(resolved.compiler_type, CompilerTypePolicy::Required, "{key}");
            assert!(resolved.bytecode_type.is_some(), "{key} lost its compiler slot");
        }
        assert_eq!(plan.userdata().iter().filter(|u| u.bytecode_type.is_some()).count(), COMPILER_TYPE_CAPACITY);
    }

    // Required needs a tag.
    let error = text(RuntimePlan::builder().extension(Many(TagPolicy::Never, CompilerTypePolicy::Required)).finalize());
    assert!(error.contains("requires a compiler type slot, which needs a tag"), "{error}");
    // Never is never named.
    let plan =
        RuntimePlan::builder().extension(Many(TagPolicy::Required, CompilerTypePolicy::Never)).finalize().unwrap();
    assert!(plan.userdata().iter().filter(|u| u.owner == "dream.slots").all(|u| u.bytecode_type.is_none()));
}

#[test]
fn two_extensions_storing_the_same_state_type_keep_their_own() {
    #[derive(Debug)]
    struct Cache(&'static str);
    struct Keeper(&'static str, &'static str);
    impl Extension for Keeper {
        fn id(&self) -> &'static str {
            self.0
        }
        fn describe(&self, _: &mut ExtensionDescriptor) -> Result<()> {
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            assert!(cx.state::<Cache>().is_none());
            cx.insert_state(Cache(self.1));
            assert_eq!(cx.state::<Cache>().unwrap().0, self.1);
            assert_eq!(cx.state_of::<Cache>(self.0).unwrap().unwrap().0, self.1, "own state by id");
            if self.0 == "dream.b" {
                // Installed after dream.a, but reading its state without declaring the
                // dependency is refused: order is not a contract, the graph is.
                let error = cx.state_of::<Cache>("dream.a").unwrap_err().to_string();
                assert!(error.contains("reads state of 'dream.a' without declaring it"), "{error}");
            }
            Ok(())
        }
    }
    struct Reader;
    impl Extension for Reader {
        fn id(&self) -> &'static str {
            "dream.reader"
        }
        fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
            d.requires("dream.a");
            d.optional("dream.zzz");
            Ok(())
        }
        fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
            assert_eq!(cx.state_of::<Cache>("dream.a")?.unwrap().0, "a's", "a declared dependency's state");
            assert!(cx.state_of::<Cache>("dream.zzz").is_err(), "an optional dependency absent from the plan");
            Ok(())
        }
    }
    let plan = RuntimePlan::builder()
        .extension(Keeper("dream.a", "a's"))
        .extension(Keeper("dream.b", "b's"))
        .extension(Reader)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    assert_eq!(runtime.state_of::<Cache>("dream.a").unwrap().0, "a's");
    assert_eq!(runtime.state_of::<Cache>("dream.b").unwrap().0, "b's");
    assert!(runtime.host_state::<Cache>().is_none());
    runtime.insert_state(Cache("host's"));
    assert_eq!(runtime.host_state::<Cache>().unwrap().0, "host's");
    assert_eq!(runtime.state_of::<Cache>("dream.a").unwrap().0, "a's");
}

#[test]
fn a_planned_runtime_refuses_shape_changes() {
    let plan = plan_with(Core::preferred(), RuntimePolicy::new());
    let runtime = Runtime::from_plan(&plan).unwrap();
    let error = runtime.set_compile_options(runtime.compile_options()).unwrap_err().to_string();
    assert!(error.contains("cannot replace the compiler options on a runtime made from a plan"), "{error}");
    let error = runtime.register_packed::<l3i::raster::Color>().unwrap_err().to_string();
    assert!(error.contains("cannot register a packed kind on a runtime made from a plan"), "{error}");
    // A hand-assembled runtime keeps both.
    let plain = Runtime::new().unwrap();
    plain.set_compile_options(plain.compile_options()).unwrap();
    plain.register_packed::<l3i::raster::Color>().unwrap();
}
