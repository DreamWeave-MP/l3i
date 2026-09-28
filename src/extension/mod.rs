//! The extension planner: describe once, resolve per runtime, install after planning.
//!
//! An [`Extension`] is a native crate's Luau surface. It declares what it provides in
//! [`Extension::describe`] (identity, dependencies, modules, userdata ownership and
//! augmentation, member names and their hot/direct policy, services, capabilities, memory
//! categories) without touching a VM, and provides the callables in [`Extension::install`]
//! against a plan that has already resolved every runtime detail: which extension installs
//! first, which Luau tag a type gets in *this* VM, which atom a member name gets, which direct
//! slot a hot member occupies, which numeric memory category a symbolic one maps to.
//!
//! A [`RuntimePlan`] is finalised once and can instantiate any number of runtimes; each VM gets
//! its own tags, atoms, and direct plan. Nothing here is process-global. The same Rust type may
//! be tag 8 in one runtime and untagged in another with identical semantics: the direct path
//! is an optimisation of the canonical metatable path, never a second API.
//!
//! The lifecycle is the one `L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §7 draws: collect
//! descriptions, resolve dependencies, resolve shared userdata, resolve tags, resolve atoms,
//! build the direct plan, validate services and capabilities, create the VM, install, freeze,
//! publish. After publication nothing about the native shape of the VM changes.

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

pub use install::{InstallContext, ModuleInstaller, UserdataInstaller};
pub use plan::{ResolvedMember, ResolvedModule, ResolvedUserdata, RuntimePlan, RuntimePlanBuilder};

use crate::error::{Error, Result};
use crate::userdata::{RuntimeTag, Userdata};

/// A native crate's Luau surface, in two phases. See the module docs.
pub trait Extension: 'static {
    /// The stable public identity, e.g. `dream.archive`: dot-separated segments of identifier
    /// characters and hyphens. Other extensions name it in `requires`; profiler and debug
    /// identities derive from it. Never a Rust `TypeId`.
    fn id(&self) -> &'static str;

    /// Declares everything the planner must know. Must not touch a VM or create Lua values.
    fn describe(&self, descriptor: &mut ExtensionDescriptor) -> Result<()>;

    /// Installs the declared members against the resolved plan. Installing an undeclared member
    /// or leaving a declared one out is an error.
    fn install(&self, context: &mut InstallContext<'_>) -> Result<()>;
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
    /// `obj:name(...)`; reachable by `__namecall` and, when direct, the direct namecall path.
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
    /// Declared hot. Every member of a tagged type already dispatches through the plan; this
    /// marks the ones native lowering and documentation should treat as hot paths.
    pub direct: bool,
    /// A Luau type signature for definition output, e.g. `(self, buffer, offset: number) -> number`.
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// The extension that contributed it.
    pub contributor: &'static str,
}

impl MemberDecl {
    /// Marks the member hot (see [`MemberDecl::direct`]).
    pub fn direct(&mut self) -> &mut Self {
        self.direct = true;
        self
    }

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

/// A userdata type one extension owns or augments.
#[derive(Clone, Debug)]
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
        }
    }

    /// The tag policy (owner only; augmentations inherit the owner's).
    pub fn tag(&mut self, policy: TagPolicy) -> &mut Self {
        self.tag = policy;
        self
    }

    pub fn doc(&mut self, doc: impl Into<String>) -> &mut Self {
        self.doc = Some(doc.into());
        self
    }

    fn member(&mut self, name: &str, kind: MemberKind) -> &mut MemberDecl {
        self.members.push(MemberDecl {
            name: name.to_owned(),
            kind,
            direct: false,
            signature: None,
            doc: None,
            contributor: self.contributor,
        });
        self.members.last_mut().expect("pushed above")
    }

    pub fn method(&mut self, name: &str) -> &mut MemberDecl {
        self.member(name, MemberKind::Method)
    }

    pub fn getter(&mut self, name: &str) -> &mut MemberDecl {
        self.member(name, MemberKind::Getter)
    }

    /// A read/write property: the getter and the setter share the name.
    pub fn setter(&mut self, name: &str) -> &mut MemberDecl {
        self.member(name, MemberKind::Setter)
    }

    /// A direct primitive field (boolean, number, integer64, Vec3, nil). Implies `direct` and
    /// makes the tag policy effectively `Required`.
    pub fn field(&mut self, name: &str) -> &mut MemberDecl {
        let member = self.member(name, MemberKind::Field);
        member.direct = true;
        member
    }
}

/// A native module the extension provides, `require`d by its path.
#[derive(Clone, Debug)]
pub struct ModuleDecl {
    /// The require path, e.g. `@dream/archive`.
    pub path: String,
    /// Read-only after install (the default; a mutable module needs a stated reason).
    pub frozen: bool,
    pub doc: Option<String>,
    pub(crate) provider: &'static str,
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
}

/// A host service an extension needs, matched by Rust type.
#[derive(Clone, Debug)]
pub struct ServiceRequirement {
    pub type_id: TypeId,
    pub type_name: &'static str,
}

/// Everything one extension declares in [`Extension::describe`].
#[derive(Debug)]
pub struct ExtensionDescriptor {
    id: &'static str,
    requires: BTreeSet<String>,
    optional: BTreeSet<String>,
    modules: Vec<ModuleDecl>,
    owned: Vec<UserdataDecl>,
    augmentations: Vec<UserdataDecl>,
    services: Vec<ServiceRequirement>,
    capabilities: BTreeSet<String>,
    memory_categories: BTreeSet<String>,
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
            memory_categories: BTreeSet::new(),
        }
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
        self.modules.push(ModuleDecl { path: path.to_owned(), frozen: true, doc: None, provider: self.id });
        self.modules.last_mut().expect("pushed above")
    }

    /// Declares ownership of the userdata type `T` under the stable `key`.
    pub fn userdata<T: Userdata>(&mut self, key: &str) -> &mut UserdataDecl {
        self.owned.push(UserdataDecl::new::<T>(key, self.id));
        self.owned.last_mut().expect("pushed above")
    }

    /// Declares a [`crate::sequence::Sequence`] over `S` under `key`: a userdata type with
    /// `toTable`, `#`, `[i]`, and `for`; install it with [`InstallContext::sequence`].
    pub fn sequence<S: crate::sequence::SequenceSource>(&mut self, key: &str) -> &mut UserdataDecl {
        let decl = self.userdata::<crate::sequence::Sequence<S>>(key);
        decl.method("toTable").signature("(self): { any }");
        decl
    }

    /// Declares a [`crate::sequence::Stream`] over `S` under `key` (`for` only); install it with
    /// [`InstallContext::stream`].
    pub fn stream<S: crate::sequence::StreamSource>(&mut self, key: &str) -> &mut UserdataDecl {
        self.userdata::<crate::sequence::Stream<S>>(key)
    }

    /// Adds members to a userdata type another extension owns (which this one must `require`).
    pub fn augment_userdata<T: Userdata>(&mut self, key: &str) -> &mut UserdataDecl {
        self.augmentations.push(UserdataDecl::new::<T>(key, self.id));
        self.augmentations.last_mut().expect("pushed above")
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
#[derive(Clone, Debug)]
pub struct NativeCodePolicy {
    pub mode: crate::native_code::NativeCodeMode,
    pub max_total_size: usize,
    pub record_counters: bool,
    pub nop_padding: bool,
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
