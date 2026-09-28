//! The extension planner: describe once, resolve per runtime, instantiate after planning.
//!
//! An [`Extension`] is a native crate's Luau surface. It declares what it provides in
//! [`Extension::describe`], callables included: identity, dependencies, modules with their
//! functions and constants, userdata ownership and augmentation with each member's binding,
//! services, capabilities, memory categories, all without touching a VM. The plan resolves
//! every runtime detail (which extension installs first, which Luau tag a type gets in *this*
//! VM, which atom a member name gets, which direct slot a member occupies, which numeric memory
//! category a symbolic one maps to) and binds the declared callables in each runtime it
//! creates. [`Extension::install`] exists only for what needs the live VM or the resolved
//! policy: capability-gated module functions, module values built from Lua objects, services,
//! runtime-owned state.
//!
//! A [`RuntimePlan`] is finalised once and can instantiate any number of runtimes; each VM gets
//! its own tags, atoms, and direct plan. Nothing here is process-global. The same Rust type may
//! be tag 8 in one runtime and untagged in another with identical semantics: the direct path
//! is an optimisation of the canonical metatable path, never a second API.
//!
//! The lifecycle is the one `L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §7 draws: collect
//! descriptions, resolve dependencies, resolve shared userdata, resolve tags, resolve atoms,
//! build the direct plan, validate services and capabilities, create the VM, register the
//! declared types and modules, run `install`, freeze, publish. After publication nothing about
//! the native shape of the VM changes.

mod dispatch;
mod install;
mod plan;
mod typedefs;

/// Crate-internal seams the runtime uses.
pub(crate) mod install_detail {
    pub(crate) use super::install::{ModuleMembers, downcast, instantiate};
    pub(crate) fn render_definitions(plan: &super::RuntimePlan, members: Option<&ModuleMembers>) -> String {
        super::typedefs::render_with(plan, members)
    }
}

use std::any::TypeId;
use std::collections::BTreeSet;

pub use install::{InstallContext, ModuleInstaller};
pub use plan::{ResolvedMember, ResolvedModule, ResolvedUserdata, RuntimePlan, RuntimePlanBuilder};

use std::rc::Rc;

use install::{Registrar, SharedFieldRegistrar, SharedInstaller, SharedModuleFunction};

use crate::bind::Binding;
use crate::direct::field::DirectField;
use crate::error::{Error, Result};
use crate::source::CompileConstant;
use crate::userdata::{RuntimeTag, Userdata};

/// A native crate's Luau surface. See the module docs.
pub trait Extension: 'static {
    /// The stable public identity, e.g. `dream.archive`: dot-separated segments of identifier
    /// characters and hyphens. Other extensions name it in `requires`; profiler and debug
    /// identities derive from it. Never a Rust `TypeId`.
    fn id(&self) -> &'static str;

    /// Declares everything the planner must know, callables included: userdata members bind
    /// here, so do module functions and constants. Must not touch a VM or create Lua values;
    /// the callables run later, in every runtime the plan instantiates.
    fn describe(&self, descriptor: &mut ExtensionDescriptor) -> Result<()>;

    /// Runs once per runtime after the declared types and modules exist, for what genuinely
    /// needs the live VM or the resolved policy: services, capability-gated module functions,
    /// module values built from Lua objects, runtime-owned state. Most extensions leave the
    /// default.
    fn install(&self, context: &mut InstallContext<'_>) -> Result<()> {
        let _ = context;
        Ok(())
    }
}

/// How much a userdata type wants a Luau tag in a runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagPolicy {
    /// The type needs a tag (direct fields, native lowering); planning fails without one.
    Required,
    /// Take a tag when one is free; fall back to the canonical untagged metatable otherwise.
    Preferred,
    /// Never tag; untagged exact-metatable identity only.
    Never,
}

/// The kind of a declared userdata member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberKind {
    /// `obj:name(...)`; reachable by `__namecall` and, on a tagged type, the direct namecall path.
    Method,
    /// `obj.name` read through a bound getter.
    Getter,
    /// `obj.name = value` through a bound setter (declared with its getter as a pair).
    Setter,
    /// `obj.name` served by a direct primitive field getter (`direct::field`) with the same
    /// getter installed as the canonical property; needs a tag.
    Field,
}

/// One declared member of a userdata type.
#[derive(Clone, Debug)]
pub struct MemberDecl {
    pub name: String,
    pub kind: MemberKind,
    /// A Luau type signature for definition output, e.g. `(self, buffer, offset: number) -> number`.
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// The extension that contributed it.
    pub contributor: &'static str,
}

impl MemberDecl {
    /// The Luau signature or type for definition output.
    pub fn signature(&mut self, signature: impl Into<String>) -> &mut Self {
        self.signature = Some(signature.into());
        self
    }

    pub fn doc(&mut self, doc: impl Into<String>) -> &mut Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A userdata type one extension owns or augments: its members, and the callables that bind
/// them in every runtime the plan instantiates.
#[derive(Clone)]
pub struct UserdataDecl {
    /// The stable script identity, e.g. `dream.archive.Archive`.
    pub key: String,
    pub type_id: TypeId,
    /// `T::NAME`, the metatable `__type`.
    pub type_name: &'static str,
    pub tag: TagPolicy,
    pub members: Vec<MemberDecl>,
    pub doc: Option<String>,
    contributor: &'static str,
    pub(crate) installers: Vec<SharedInstaller>,
    pub(crate) fields: Vec<(String, SharedFieldRegistrar)>,
    pub(crate) registrar: Registrar,
}

impl std::fmt::Debug for UserdataDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserdataDecl")
            .field("key", &self.key)
            .field("type_name", &self.type_name)
            .field("tag", &self.tag)
            .field("members", &self.members)
            .field("doc", &self.doc)
            .field("contributor", &self.contributor)
            .finish_non_exhaustive()
    }
}

impl UserdataDecl {
    fn new<T: Userdata>(key: &str, contributor: &'static str) -> Self {
        UserdataDecl {
            key: key.to_owned(),
            type_id: TypeId::of::<T>(),
            type_name: T::NAME,
            tag: TagPolicy::Preferred,
            members: Vec::new(),
            doc: None,
            contributor,
            installers: Vec::new(),
            fields: Vec::new(),
            registrar: install::register_type::<T>,
        }
    }

    fn member(&mut self, name: &str, kind: MemberKind) -> &mut MemberDecl {
        self.members.push(MemberDecl {
            name: name.to_owned(),
            kind,
            signature: None,
            doc: None,
            contributor: self.contributor,
        });
        self.members.last_mut().expect("pushed above")
    }
}

/// The typed view of a [`UserdataDecl`] under construction: declares a member and binds its
/// callable in one call. Callables are `Clone` because a plan instantiates any number of
/// runtimes and binds each member once per VM; closures that capture nothing, `Rc`s, or
/// `Clone` data qualify.
pub struct UserdataBuilder<'a, T: Userdata> {
    decl: &'a mut UserdataDecl,
    _type: std::marker::PhantomData<T>,
}

impl<T: Userdata> UserdataBuilder<'_, T> {
    /// The tag policy (owner only; augmentations inherit the owner's).
    pub fn tag(&mut self, policy: TagPolicy) -> &mut Self {
        self.decl.tag = policy;
        self
    }

    pub fn doc(&mut self, doc: impl Into<String>) -> &mut Self {
        self.decl.doc = Some(doc.into());
        self
    }

    /// The declaration being built.
    pub fn decl(&mut self) -> &mut UserdataDecl {
        self.decl
    }

    /// `obj:name(...)`.
    pub fn method<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> &mut MemberDecl {
        let owned = name.to_owned();
        self.decl.installers.push(Rc::new(move |ty| {
            let entry = ty.method_with_entry(&owned, callable.clone())?;
            Ok(vec![(owned.clone(), MemberKind::Method, entry)])
        }));
        self.decl.member(name, MemberKind::Method)
    }

    /// `obj.name`, read-only, through a bound getter.
    pub fn getter<G: Binding<MG> + Clone + 'static, MG: 'static>(&mut self, name: &str, getter: G) -> &mut MemberDecl {
        let owned = name.to_owned();
        self.decl.installers.push(Rc::new(move |ty| {
            let entry = ty.property_with_entry(&owned, getter.clone())?;
            Ok(vec![(owned.clone(), MemberKind::Getter, entry)])
        }));
        self.decl.member(name, MemberKind::Getter)
    }

    /// `obj.name` read and written through a getter/setter pair. The returned declaration is
    /// the getter's; a signature set on it is the property's type.
    pub fn property<G, MG, S, MS>(&mut self, name: &str, getter: G, setter: S) -> &mut MemberDecl
    where
        G: Binding<MG> + Clone + 'static,
        MG: 'static,
        S: Binding<MS> + Clone + 'static,
        MS: 'static,
    {
        let owned = name.to_owned();
        self.decl.installers.push(Rc::new(move |ty| {
            let (get, set) = ty.property_rw_with_entries(&owned, getter.clone(), setter.clone())?;
            Ok(vec![(owned.clone(), MemberKind::Getter, get), (owned.clone(), MemberKind::Setter, set)])
        }));
        self.decl.member(name, MemberKind::Setter);
        let index = self.decl.members.len() - 1;
        self.decl.member(name, MemberKind::Getter);
        self.decl.members.swap(index, index + 1);
        &mut self.decl.members[index]
    }

    /// A direct primitive field (boolean, number, integer64, Vec3, nil) served by `H`,
    /// installed as the canonical property too. Makes the tag policy effectively `Required`.
    pub fn field<H: DirectField<T>>(&mut self, name: &str) -> &mut MemberDecl {
        let owned = name.to_owned();
        self.decl.installers.push(Rc::new(move |ty| {
            let entry = ty.property_with_entry(&owned, |value: &T| H::get(value))?;
            Ok(vec![(owned.clone(), MemberKind::Field, entry)])
        }));
        let for_register = name.to_owned();
        self.decl.fields.push((
            name.to_owned(),
            Rc::new(move |runtime| crate::direct::field::register::<T, H>(runtime, &for_register)),
        ));
        self.decl.member(name, MemberKind::Field)
    }

    /// A metamethod (`__tostring`, `__eq`, `__len`, ...): not a dispatch member.
    pub fn metamethod<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> &mut Self {
        let owned = name.to_owned();
        self.decl.installers.push(Rc::new(move |ty| {
            ty.metamethod(&owned, callable.clone())?;
            Ok(Vec::new())
        }));
        self
    }
}

/// A native module the extension provides, `require`d by its path, with the functions and
/// constants it declares. Values that need the live VM are added in [`Extension::install`]
/// through [`InstallContext::module`].
#[derive(Clone)]
pub struct ModuleDecl {
    /// The require path, e.g. `@dream/archive`.
    pub path: String,
    /// Read-only after install (the default; a mutable module needs a stated reason).
    pub frozen: bool,
    pub doc: Option<String>,
    pub(crate) provider: &'static str,
    pub(crate) functions: Vec<(String, SharedModuleFunction)>,
    pub(crate) constants: Vec<(String, CompileConstant)>,
}

impl std::fmt::Debug for ModuleDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleDecl")
            .field("path", &self.path)
            .field("frozen", &self.frozen)
            .field("doc", &self.doc)
            .field("provider", &self.provider)
            .field("functions", &self.functions.iter().map(|(name, _)| name).collect::<Vec<_>>())
            .field("constants", &self.constants)
            .finish()
    }
}

impl ModuleDecl {
    pub fn frozen(&mut self) -> &mut Self {
        self.frozen = true;
        self
    }

    pub fn mutable(&mut self) -> &mut Self {
        self.frozen = false;
        self
    }

    pub fn doc(&mut self, doc: impl Into<String>) -> &mut Self {
        self.doc = Some(doc.into());
        self
    }

    /// A module function, bound in every runtime. The callable is `Clone` for the same reason
    /// userdata members' are (see [`UserdataBuilder`]).
    pub fn function<F: Binding<M> + Clone + 'static, M: 'static>(&mut self, name: &str, callable: F) -> &mut Self {
        self.functions.push((
            name.to_owned(),
            Rc::new(move |runtime, roots, debug_name| {
                crate::bind::function(&runtime.stack(), roots, debug_name, callable.clone())
            }),
        ));
        self
    }

    /// A constant the compiler may fold when the module is a known global library.
    pub fn constant(&mut self, name: &str, value: CompileConstant) -> &mut Self {
        self.constants.push((name.to_owned(), value));
        self
    }
}

/// A host service an extension needs, matched by Rust type.
#[derive(Clone, Debug)]
pub struct ServiceRequirement {
    pub type_id: TypeId,
    pub type_name: &'static str,
}

/// Everything one extension declares in [`Extension::describe`].
pub struct ExtensionDescriptor {
    id: &'static str,
    requires: BTreeSet<String>,
    optional: BTreeSet<String>,
    modules: Vec<ModuleDecl>,
    owned: Vec<UserdataDecl>,
    augmentations: Vec<UserdataDecl>,
    services: Vec<ServiceRequirement>,
    capabilities: BTreeSet<String>,
    optional_capabilities: BTreeSet<String>,
    memory_categories: BTreeSet<String>,
    packed: Vec<crate::packed::PackedKind>,
    #[cfg(feature = "jit")]
    native_hooks: Vec<std::rc::Rc<dyn crate::native_code::NativeCodeHooks>>,
}

impl std::fmt::Debug for ExtensionDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionDescriptor")
            .field("id", &self.id)
            .field("requires", &self.requires)
            .field("modules", &self.modules)
            .field("owned", &self.owned)
            .field("augmentations", &self.augmentations)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl ExtensionDescriptor {
    pub(crate) fn new(id: &'static str) -> Self {
        ExtensionDescriptor {
            id,
            requires: BTreeSet::new(),
            optional: BTreeSet::new(),
            modules: Vec::new(),
            owned: Vec::new(),
            augmentations: Vec::new(),
            services: Vec::new(),
            capabilities: BTreeSet::new(),
            optional_capabilities: BTreeSet::new(),
            memory_categories: BTreeSet::new(),
            packed: Vec::new(),
            #[cfg(feature = "jit")]
            native_hooks: Vec::new(),
        }
    }

    /// Declares a packed scalar kind this extension's members use, so every runtime from the
    /// plan registers `T` as the kind's owner and the plan refuses a second type on the same
    /// number. l3i's own kinds need no declaration.
    pub fn packed<T: crate::packed::PackedScalar>(&mut self) -> &mut Self {
        let kind = crate::packed::PackedKind::of::<T>();
        if !self.packed.contains(&kind) {
            self.packed.push(kind);
        }
        self
    }

    pub fn packed_kinds(&self) -> &[crate::packed::PackedKind] {
        &self.packed
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    /// Another extension that must be planned and installed before this one.
    pub fn requires(&mut self, id: &str) -> &mut Self {
        self.requires.insert(id.to_owned());
        self
    }

    /// Another extension this one integrates with when present; it installs first if it is.
    pub fn optional(&mut self, id: &str) -> &mut Self {
        self.optional.insert(id.to_owned());
        self
    }

    /// Declares a native module at `path` (frozen by default).
    pub fn module(&mut self, path: &str) -> &mut ModuleDecl {
        self.modules.push(ModuleDecl {
            path: path.to_owned(),
            frozen: true,
            doc: None,
            provider: self.id,
            functions: Vec::new(),
            constants: Vec::new(),
        });
        self.modules.last_mut().expect("pushed above")
    }

    /// Declares ownership of the userdata type `T` under the stable `key`, and its members.
    pub fn userdata<T: Userdata>(&mut self, key: &str) -> UserdataBuilder<'_, T> {
        self.owned.push(UserdataDecl::new::<T>(key, self.id));
        UserdataBuilder { decl: self.owned.last_mut().expect("pushed above"), _type: std::marker::PhantomData }
    }

    /// Declares a [`crate::sequence::Sequence`] over `S` under `key`: a userdata type with
    /// `toTable`, `#`, `[i]`, and `for`, fully bound.
    pub fn sequence<S: crate::sequence::SequenceSource>(
        &mut self,
        key: &str,
    ) -> UserdataBuilder<'_, crate::sequence::Sequence<S>> {
        let builder = self.userdata::<crate::sequence::Sequence<S>>(key);
        builder.decl.installers.push(Rc::new(|ty| {
            let entry = crate::sequence::configure_sequence_with_entry::<S>(ty)?;
            Ok(vec![("toTable".to_owned(), MemberKind::Method, entry)])
        }));
        builder.decl.member("toTable", MemberKind::Method).signature("(self): { any }");
        builder
    }

    /// Declares a [`crate::sequence::Stream`] over `S` under `key` (`for` only), fully bound.
    pub fn stream<S: crate::sequence::StreamSource>(&mut self, key: &str) -> UserdataBuilder<'_, crate::sequence::Stream<S>> {
        let builder = self.userdata::<crate::sequence::Stream<S>>(key);
        builder.decl.installers.push(Rc::new(|ty| {
            crate::sequence::configure_stream::<S>(ty)?;
            Ok(Vec::new())
        }));
        builder
    }

    /// Adds members to a userdata type another extension owns (which this one must `require`).
    pub fn augment_userdata<T: Userdata>(&mut self, key: &str) -> UserdataBuilder<'_, T> {
        self.augmentations.push(UserdataDecl::new::<T>(key, self.id));
        UserdataBuilder { decl: self.augmentations.last_mut().expect("pushed above"), _type: std::marker::PhantomData }
    }

    /// A host service of type `S` this extension reads at install time.
    pub fn service<S: 'static>(&mut self) -> &mut Self {
        self.services.push(ServiceRequirement { type_id: TypeId::of::<S>(), type_name: std::any::type_name::<S>() });
        self
    }

    /// A capability the runtime policy must grant, e.g. `filesystem.read`.
    pub fn capability(&mut self, name: &str) -> &mut Self {
        self.capabilities.insert(name.to_owned());
        self
    }

    /// A capability the extension checks at run time (`InstallContext::has_capability`) but
    /// does not need to install: the runtime plans with or without it.
    pub fn optional_capability(&mut self, name: &str) -> &mut Self {
        self.optional_capabilities.insert(name.to_owned());
        self
    }

    /// Native lowering hooks for this extension's types (`jit`). They see the tag and atoms the
    /// VM being compiled for actually assigned, through `NativeContext`; userdata types are named
    /// to the compiler by their stable key's class name (`dream.quat.Math` → `dream_quat_Math`),
    /// which is what scripts annotate.
    #[cfg(feature = "jit")]
    pub fn native_hooks(&mut self, hooks: impl crate::native_code::NativeCodeHooks) -> &mut Self {
        self.native_hooks.push(std::rc::Rc::new(hooks));
        self
    }

    #[cfg(feature = "jit")]
    pub fn native_hook_sets(&self) -> &[std::rc::Rc<dyn crate::native_code::NativeCodeHooks>] {
        &self.native_hooks
    }

    /// A symbolic memory category the planner maps to a Luau category number.
    pub fn memory_category(&mut self, name: &str) -> &mut Self {
        self.memory_categories.insert(name.to_owned());
        self
    }

    pub fn dependencies(&self) -> impl Iterator<Item = &str> {
        self.requires.iter().map(String::as_str)
    }

    pub fn optional_dependencies(&self) -> impl Iterator<Item = &str> {
        self.optional.iter().map(String::as_str)
    }

    pub fn modules(&self) -> &[ModuleDecl] {
        &self.modules
    }

    pub fn owned_userdata(&self) -> &[UserdataDecl] {
        &self.owned
    }

    pub fn augmentations(&self) -> &[UserdataDecl] {
        &self.augmentations
    }

    pub fn services(&self) -> &[ServiceRequirement] {
        &self.services
    }

    pub fn capabilities(&self) -> impl Iterator<Item = &str> {
        self.capabilities.iter().map(String::as_str)
    }

    pub fn optional_capabilities(&self) -> impl Iterator<Item = &str> {
        self.optional_capabilities.iter().map(String::as_str)
    }

    pub fn memory_categories(&self) -> impl Iterator<Item = &str> {
        self.memory_categories.iter().map(String::as_str)
    }
}

/// Execution policy for runtimes made from a plan: the VM's configuration and what scripts may
/// do. Not a Jess Profile; the exact preset names are host vocabulary.
#[derive(Clone, Debug)]
pub struct RuntimePolicy {
    /// Extra debug-name roots beside the ones extension identities imply.
    pub debug_roots: Vec<&'static str>,
    pub standard_libraries: bool,
    /// `luaL_sandbox` after installation: globals read-only, safe environment on.
    pub sandbox: bool,
    pub limits: crate::runtime::Limits,
    pub profiler: bool,
    pub pointer_encoding: bool,
    /// The first tag the planner hands out; lower tags stay free for the host.
    pub first_tag: RuntimeTag,
    /// Capabilities granted to extensions (checked at finalisation and at install).
    pub capabilities: BTreeSet<String>,
    /// Module paths also exposed as globals, host policy during migrations:
    /// `("@dream/archive", "dreamArchive")`. Frozen modules exposed this way also become
    /// compiler-known libraries.
    pub compat_globals: Vec<(String, String)>,
    /// Native code generation for scripts (`jit` feature).
    #[cfg(feature = "jit")]
    pub native_code: Option<NativeCodePolicy>,
}

/// Native code generation settings a plan can reuse for every runtime it creates (the full
/// [`crate::native_code::NativeCodeOptions`] holds boxed hooks and is built per runtime).
#[cfg(feature = "jit")]
#[derive(Clone)]
pub struct NativeCodePolicy {
    pub mode: crate::native_code::NativeCodeMode,
    pub max_total_size: usize,
    pub record_counters: bool,
    pub nop_padding: bool,
    /// Host lowering hooks, asked after the defaults and before the extensions' own.
    pub hooks: Vec<std::rc::Rc<dyn crate::native_code::NativeCodeHooks>>,
}

#[cfg(feature = "jit")]
impl std::fmt::Debug for NativeCodePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCodePolicy")
            .field("mode", &self.mode)
            .field("max_total_size", &self.max_total_size)
            .field("hooks", &self.hooks.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "jit")]
impl Default for NativeCodePolicy {
    fn default() -> Self {
        let defaults = crate::native_code::NativeCodeOptions::default();
        NativeCodePolicy {
            mode: defaults.mode,
            max_total_size: defaults.max_total_size,
            record_counters: defaults.record_counters,
            nop_padding: defaults.nop_padding,
            hooks: Vec::new(),
        }
    }
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        RuntimePolicy {
            debug_roots: Vec::new(),
            standard_libraries: true,
            sandbox: false,
            limits: crate::runtime::Limits::default(),
            profiler: false,
            pointer_encoding: true,
            first_tag: 1,
            capabilities: BTreeSet::new(),
            compat_globals: Vec::new(),
            #[cfg(feature = "jit")]
            native_code: None,
        }
    }
}

impl RuntimePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn capability(mut self, name: &str) -> Self {
        self.capabilities.insert(name.to_owned());
        self
    }

    pub fn compat_global(mut self, module_path: &str, global: &str) -> Self {
        self.compat_globals.push((module_path.to_owned(), global.to_owned()));
        self
    }

    pub fn sandbox(mut self, enabled: bool) -> Self {
        self.sandbox = enabled;
        self
    }

    pub fn standard_libraries(mut self, enabled: bool) -> Self {
        self.standard_libraries = enabled;
        self
    }

    pub fn limits(mut self, limits: crate::runtime::Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn profiler(mut self, enabled: bool) -> Self {
        self.profiler = enabled;
        self
    }

    pub fn first_tag(mut self, tag: RuntimeTag) -> Self {
        self.first_tag = tag;
        self
    }

    pub fn debug_root(mut self, root: &'static str) -> Self {
        self.debug_roots.push(root);
        self
    }

    #[cfg(feature = "jit")]
    pub fn native_code(mut self, policy: NativeCodePolicy) -> Self {
        self.native_code = Some(policy);
        self
    }

    pub fn grants(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }
}

/// Validates an extension id: dot-separated segments of identifier characters and hyphens.
pub(crate) fn validate_extension_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && !segment.as_bytes()[0].is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(Error::logic(format!(
            "extension id '{id}' must be dot-separated segments of letters, digits, '_' or '-', not starting with a digit"
        )))
    }
}

/// The debug-name spelling of an extension id: hyphens become underscores so `dream.openmw-config`
/// binds functions as `dream.openmw_config.<name>`.
pub(crate) fn debug_prefix(id: &str) -> String {
    id.replace('-', "_")
}

/// The debug root an extension id implies (its first segment, hyphens folded).
pub(crate) fn debug_root(id: &str) -> String {
    debug_prefix(id.split('.').next().unwrap_or(id))
}
