//! The install phase: the planner registers the declared metatables with their bound members,
//! direct dispatch, the declared modules, then runs each extension's `install` for what needs
//! the live VM, freezes the modules, derives compiler metadata, and publishes.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ffi::CString;
use std::rc::Rc;

use super::plan::{ResolvedModule, ResolvedUserdata, RuntimePlan};
use super::{MemberKind, debug_prefix};
use crate::bind::{Binding, MemberEntry};
use crate::convert::Push;
use crate::direct::field::FieldValue;
use crate::direct::plan::DirectPlanBuilder;
use crate::error::{Error, Result};
use crate::runtime::{MemoryCategory, Runtime};
use crate::source::{CompileConstant, CompileOptions, LibraryMembers};
use crate::stack::Scope;
use crate::userdata::metatable::MetatableBuilder;
use crate::userdata::{Userdata, tagged, untagged};
use crate::value::{Function, Table};

/// Installs one member into the metatable being built; returns the entries it created. Shared
/// by every runtime the plan instantiates, so it binds a clone of the callable each time.
pub(crate) type SharedInstaller =
    Rc<dyn Fn(&mut MetatableBuilder<'_>) -> Result<Vec<(String, MemberKind, MemberEntry)>>>;
/// Registers a direct primitive field after the metatable exists.
pub(crate) type SharedFieldRegistrar = Rc<dyn Fn(&Runtime) -> Result<()>>;
/// Registers the type (tagged or untagged) with every collected member: `register_type::<T>`.
pub(crate) type Registrar = fn(&Runtime, &ResolvedUserdata, &[SharedInstaller]) -> Result<()>;
/// Binds a declared module function in one runtime: `(runtime, debug roots, debug name)`.
pub(crate) type SharedModuleFunction = Rc<dyn Fn(&Runtime, &[&str], &str) -> Result<Function>>;

/// What the compiler may know about a module member exposed as a global library.
#[derive(Clone, Debug)]
pub(crate) enum ModuleMemberInfo {
    Function,
    Constant(CompileConstant),
    Unknown,
}

/// Recorded module members: library metadata for the compiler and type definitions.
#[derive(Default, Debug)]
pub(crate) struct ModuleMembers {
    pub(crate) by_module: HashMap<String, Vec<(String, ModuleMemberInfo)>>,
    /// Compat global name to module path, for the compiler's library queries.
    pub(crate) globals: HashMap<String, String>,
}

impl LibraryMembers for ModuleMembers {
    fn member_type(&self, library: &str, member: &str) -> Option<u8> {
        let path = self.globals.get(library)?;
        let (_, info) = self.by_module.get(path)?.iter().find(|(name, _)| name == member)?;
        Some(match info {
            ModuleMemberInfo::Function => bytecode::FUNCTION,
            ModuleMemberInfo::Constant(constant) => match constant {
                CompileConstant::Nil => bytecode::NIL,
                CompileConstant::Boolean(_) => bytecode::BOOLEAN,
                CompileConstant::Number(_) => bytecode::NUMBER,
                CompileConstant::Integer(_) => bytecode::INTEGER,
                CompileConstant::Vector(..) => bytecode::VECTOR,
                CompileConstant::String(_) => bytecode::STRING,
            },
            ModuleMemberInfo::Unknown => return None,
        })
    }

    fn member_constant(&self, library: &str, member: &str) -> Option<CompileConstant> {
        let path = self.globals.get(library)?;
        match self.by_module.get(path)?.iter().find(|(name, _)| name == member)? {
            (_, ModuleMemberInfo::Constant(constant)) => Some(constant.clone()),
            _ => None,
        }
    }
}

/// `LuauBytecodeType` values (Bytecode.h), independent of the `jit` feature.
mod bytecode {
    pub const NIL: u8 = 0;
    pub const BOOLEAN: u8 = 1;
    pub const NUMBER: u8 = 2;
    pub const STRING: u8 = 3;
    pub const FUNCTION: u8 = 5;
    pub const VECTOR: u8 = 8;
    pub const INTEGER: u8 = 10;
}

/// The install-phase view of the runtime one extension receives.
pub struct InstallContext<'r> {
    runtime: &'r Runtime,
    plan: &'r RuntimePlan,
    current: &'static str,
    descriptor_index: usize,
    /// The open modules, lent to the context for the duration of one extension's `install`.
    modules: Vec<ModuleInstaller<'r>>,
}

impl<'r> InstallContext<'r> {
    pub fn runtime(&self) -> &'r Runtime {
        self.runtime
    }

    pub fn plan(&self) -> &'r RuntimePlan {
        self.plan
    }

    /// The extension being installed.
    pub fn extension_id(&self) -> &'static str {
        self.current
    }

    /// The module at `path`, which this extension declared, with its declared functions and
    /// constants already in place: add what needs the live VM or the resolved policy. The
    /// planner freezes it after every extension has installed.
    pub fn module(&mut self, path: &str) -> Result<&mut ModuleInstaller<'r>> {
        let current = self.current;
        let module = self.modules.iter_mut().find(|module| module.resolved.path == path).ok_or_else(|| {
            Error::logic(format!("extension '{current}' installs module '{path}', which the plan does not know"))
        })?;
        if module.resolved.provider != current {
            return Err(Error::logic(format!(
                "extension '{current}' installs module '{path}', which '{}' provides",
                module.resolved.provider
            )));
        }
        Ok(module)
    }

    /// A host service this extension declared it needs.
    pub fn service<S: 'static>(&self) -> Result<Rc<S>> {
        let declared = self.plan.descriptors[self.descriptor_index]
            .services()
            .iter()
            .any(|service| service.type_id == TypeId::of::<S>());
        if !declared {
            return Err(Error::logic(format!(
                "extension '{}' reads service {} without declaring it in describe",
                self.current,
                std::any::type_name::<S>()
            )));
        }
        self.plan.service::<S>().ok_or_else(|| {
            Error::logic(format!("host service {} is missing from the plan", std::any::type_name::<S>()))
        })
    }

    /// Whether the runtime policy grants `capability`.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.plan.policy.grants(capability)
    }

    /// Fails with a permission error unless the policy grants `capability`, which this
    /// extension must also have declared.
    pub fn require_capability(&self, capability: &str) -> Result<()> {
        let descriptor = &self.plan.descriptors[self.descriptor_index];
        let declared = descriptor.capabilities().any(|c| c == capability)
            || descriptor.optional_capabilities().any(|c| c == capability);
        if !declared {
            return Err(Error::logic(format!(
                "extension '{}' checks capability '{capability}' without declaring it in describe",
                self.current
            )));
        }
        if self.has_capability(capability) {
            Ok(())
        } else {
            Err(Error::permission(format!("capability '{capability}' is not granted to this runtime")))
        }
    }

    /// The Luau memory category for a symbolic name this extension declared.
    pub fn memory_category(&self, name: &str) -> Result<MemoryCategory> {
        let declared = self.plan.descriptors[self.descriptor_index].memory_categories().any(|c| c == name);
        if !declared {
            return Err(Error::logic(format!(
                "extension '{}' asks for memory category '{name}' without declaring it",
                self.current
            )));
        }
        self.plan
            .memory_category(name)
            .ok_or_else(|| Error::logic(format!("memory category '{name}' was not resolved")))
    }

    /// Stores extension-owned runtime state, dropped before the VM closes.
    pub fn insert_state<S: 'static>(&self, state: S) -> Rc<S> {
        self.runtime.insert_state(state)
    }

    pub fn state<S: 'static>(&self) -> Option<Rc<S>> {
        self.runtime.extension_state::<S>()
    }
}

impl Push for FieldValue {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<crate::stack::ValueView<'s>> {
        match self {
            FieldValue::Nil => ().push_into(scope),
            FieldValue::Boolean(value) => value.push_into(scope),
            FieldValue::Number(value) => value.push_into(scope),
            FieldValue::Integer(value) => crate::convert::Integer(*value).push_into(scope),
            FieldValue::Vector(value) => value.push_into(scope),
        }
    }
}

impl crate::bind::Return for FieldValue {
    fn push_results(self, call: &crate::bind::Call<'_>) -> Result<std::ffi::c_int> {
        self.push_into(call)?;
        Ok(1)
    }
}

/// Registers `T` with every declared installer and binds the direct slots to their entries.
pub(crate) fn register_type<T: Userdata>(
    runtime: &Runtime,
    resolved: &ResolvedUserdata,
    installers: &[SharedInstaller],
) -> Result<()> {
    let mut entries: Vec<(String, MemberKind, MemberEntry)> = Vec::new();
    let configure = |ty: &mut MetatableBuilder<'_>| -> Result<()> {
        for installer in installers {
            entries.extend(installer(ty)?);
        }
        Ok(())
    };
    match resolved.tag {
        Some(tag) => tagged::register::<T>(runtime, tag, configure)?,
        None => untagged::register::<T>(runtime, configure)?,
    }
    for member in &resolved.members {
        let Some(slot) = member.slot else { continue };
        let entry = entries.iter().find(|(name, k, _)| *name == member.name && *k == member.kind).map(|(_, _, e)| *e);
        let Some(entry) = entry else {
            return Err(Error::logic(format!(
                "no callable installed for direct member '{}'.{}",
                resolved.key, member.name
            )));
        };
        runtime.shared().set_direct_entry(slot, entry);
    }
    Ok(())
}

/// One module table under construction: the declared functions and constants first, then
/// whatever the provider's `install` adds, then frozen by the planner.
pub struct ModuleInstaller<'c> {
    runtime: &'c Runtime,
    plan: &'c RuntimePlan,
    resolved: &'c ResolvedModule,
    table: Table,
    members: Vec<(String, ModuleMemberInfo)>,
    prefix: String,
}

impl<'c> ModuleInstaller<'c> {
    pub fn path(&self) -> &str {
        &self.resolved.path
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    fn record(&mut self, name: &str, info: ModuleMemberInfo) -> Result<()> {
        if self.members.iter().any(|(existing, _)| existing == name) {
            return Err(Error::logic(format!("module '{}' member '{name}' is set twice", self.resolved.path)));
        }
        self.members.push((name.to_owned(), info));
        Ok(())
    }

    /// Binds `callable` as the module function `name`.
    pub fn function<F: Binding<M>, M>(&mut self, name: &str, callable: F) -> Result<&mut Self> {
        self.record(name, ModuleMemberInfo::Function)?;
        let stack = self.runtime.stack();
        let debug_name = format!("{}.{name}", self.prefix);
        let function: Function = crate::bind::function(&stack, self.plan.debug_roots(), &debug_name, callable)?;
        self.table.set(&stack, name, &function)?;
        Ok(self)
    }

    /// A constant the compiler may fold when the module is a known global library.
    pub fn constant(&mut self, name: &str, value: CompileConstant) -> Result<&mut Self> {
        let stack = self.runtime.stack();
        match &value {
            CompileConstant::Nil => self.table.set(&stack, name, &())?,
            CompileConstant::Boolean(b) => self.table.set(&stack, name, b)?,
            CompileConstant::Number(n) => self.table.set(&stack, name, n)?,
            CompileConstant::Integer(i) => self.table.set(&stack, name, &crate::convert::Integer(*i))?,
            CompileConstant::Vector(x, y, z) => {
                self.table.set(&stack, name, &crate::convert::Vector3 { x: *x, y: *y, z: *z })?;
            }
            CompileConstant::String(s) => self.table.set(&stack, name, s.as_str())?,
        }
        self.record(name, ModuleMemberInfo::Constant(value))?;
        Ok(self)
    }

    /// Any other value (a nested table, a userdata constructor table); unknown to the compiler.
    pub fn set<T: Push + ?Sized>(&mut self, name: &str, value: &T) -> Result<&mut Self> {
        self.record(name, ModuleMemberInfo::Unknown)?;
        self.table.set(&self.runtime.stack(), name, value)?;
        Ok(self)
    }

    /// Creates the table with the module's declared functions and constants.
    fn open(runtime: &'c Runtime, plan: &'c RuntimePlan, resolved: &'c ResolvedModule) -> Result<Self> {
        let table = Table::new(&runtime.stack(), 0, 8)?;
        let mut module =
            ModuleInstaller { runtime, plan, resolved, table, members: Vec::new(), prefix: debug_prefix(resolved.provider) };
        for (name, bind) in &resolved.functions {
            module.record(name, ModuleMemberInfo::Function)?;
            let debug_name = format!("{}.{name}", module.prefix);
            let function = bind(runtime, plan.debug_roots(), &debug_name)?;
            module.table.set(&runtime.stack(), name, &function)?;
        }
        for (name, value) in &resolved.constants {
            module.constant(name, value.clone())?;
        }
        Ok(module)
    }

    /// Freezes the module (unless declared mutable), registers it for `require`, and exposes
    /// the compatibility global the policy asked for.
    fn finish(self, members: &mut ModuleMembers) -> Result<()> {
        let ModuleInstaller { runtime, resolved, table, members: recorded, .. } = self;
        if resolved.frozen {
            crate::readonly::make_read_only(runtime, &table)?;
        }
        runtime.register_require_module(&resolved.path, table.value())?;
        if let Some(global) = &resolved.global {
            runtime.set_global(global, &table)?;
            members.globals.insert(global.clone(), resolved.path.clone());
        }
        members.by_module.insert(resolved.path.clone(), recorded);
        Ok(())
    }
}

/// `require` with no filesystem: only modules registered by the plan resolve.
struct PlanRequire;

impl crate::require::RequireNavigator for PlanRequire {
    fn reset(&self, _requirer_chunkname: &str) -> crate::require::Navigate {
        crate::require::Navigate::NotFound
    }
    fn to_parent(&self) -> crate::require::Navigate {
        crate::require::Navigate::NotFound
    }
    fn to_child(&self, _name: &str) -> crate::require::Navigate {
        crate::require::Navigate::NotFound
    }
    fn is_module_present(&self) -> bool {
        false
    }
    fn chunkname(&self) -> Option<String> {
        None
    }
    fn loadname(&self) -> Option<String> {
        None
    }
    fn cache_key(&self) -> Option<String> {
        None
    }
    fn load(
        &self,
        _scope: &crate::require::impl_scope::Requirer<'_>,
        path: &str,
        _chunkname: &str,
        _loadname: &str,
    ) -> Result<crate::require::Load> {
        Err(Error::runtime(format!("no module present at resolved path '{path}'")))
    }
}

/// Creates a runtime from a finalised plan: build the VM, register every declared type with its
/// bound members and the direct dispatch over them, open the declared modules, run each
/// extension's `install` in dependency order, freeze the modules, derive compiler metadata,
/// and publish.
pub(crate) fn instantiate(plan: &Rc<RuntimePlan>) -> Result<Runtime> {
    let runtime = build_runtime(plan)?;
    register_types(&runtime, plan)?;
    register_direct(&runtime, plan)?;
    register_fields(&runtime, plan)?;
    let mut modules: Vec<ModuleInstaller<'_>> =
        plan.modules.iter().map(|resolved| ModuleInstaller::open(&runtime, plan, resolved)).collect::<Result<_>>()?;
    install_all(&runtime, plan, &mut modules)?;
    let mut members = ModuleMembers::default();
    for module in modules {
        module.finish(&mut members)?;
    }
    let members = Rc::new(members);
    runtime.set_compile_options(compile_options(plan, &members));
    runtime.set_module_members(members);
    if plan.policy.sandbox {
        runtime.sandbox_globals();
    }
    Ok(runtime)
}

/// The VM from the plan's policy, with the plan's atom catalogue and debug roots.
fn build_runtime(plan: &Rc<RuntimePlan>) -> Result<Runtime> {
    let policy = &plan.policy;
    let mut builder = Runtime::builder()
        .debug_roots(&plan.debug_roots)
        .standard_libraries(policy.standard_libraries)
        .profiler(policy.profiler)
        .pointer_encoding(policy.pointer_encoding)
        .atom_catalogue(plan.atoms.clone());
    if !policy.limits.execution_time.is_zero() {
        builder = builder.execution_time_limit(policy.limits.execution_time);
    }
    if policy.limits.memory_bytes != 0 {
        builder = builder.memory_limit(policy.limits.memory_bytes);
    }
    #[cfg(feature = "jit")]
    if let Some(native) = &policy.native_code {
        let mut options = crate::native_code::NativeCodeOptions {
            mode: native.mode,
            max_total_size: native.max_total_size,
            record_counters: native.record_counters,
            nop_padding: native.nop_padding,
            userdata_types: tagged_in_order(plan).iter().map(|u| super::typedefs::class_name(&u.key)).collect(),
            ..Default::default()
        };
        for hooks in &native.hooks {
            options.hooks.push(Box::new(Rc::clone(hooks)));
        }
        for &index in &plan.order {
            for hooks in plan.descriptors[index].native_hook_sets() {
                options.hooks.push(Box::new(Rc::clone(hooks)));
            }
        }
        builder = builder.native_code(options);
    }
    let runtime = builder.build()?;
    #[cfg(feature = "jit")]
    {
        use crate::native_code::ir::bytecode_type::{TAGGED_USERDATA_BASE, TAGGED_USERDATA_END};
        // Luau encodes at most 32 userdata types; the compiler ignores the rest of the list.
        let capacity = usize::from(TAGGED_USERDATA_END - TAGGED_USERDATA_BASE);
        for (index, resolved) in tagged_in_order(plan).into_iter().take(capacity).enumerate() {
            runtime.shared().set_userdata_type(resolved.type_id, TAGGED_USERDATA_BASE + index as u8);
        }
    }
    runtime.set_plan(Rc::clone(plan));
    runtime.install_require(PlanRequire)?;
    Ok(runtime)
}

/// Tagged types by tag: the compiler's and the code generator's userdata type order.
fn tagged_in_order(plan: &RuntimePlan) -> Vec<&ResolvedUserdata> {
    let mut tagged: Vec<&ResolvedUserdata> = plan.userdata.iter().filter(|u| u.tag.is_some()).collect();
    tagged.sort_by_key(|u| u.tag);
    tagged
}

/// Runs every extension's `install` in dependency order.
fn install_all<'r>(runtime: &'r Runtime, plan: &'r RuntimePlan, modules: &mut Vec<ModuleInstaller<'r>>) -> Result<()> {
    for &index in &plan.order {
        let extension = &plan.extensions[index];
        let id = plan.descriptors[index].id();
        let mut context =
            InstallContext { runtime, plan, current: id, descriptor_index: index, modules: std::mem::take(modules) };
        extension.install(&mut context)?;
        *modules = context.modules;
    }
    Ok(())
}

/// Registers every declared type with its bound members.
fn register_types(runtime: &Runtime, plan: &RuntimePlan) -> Result<()> {
    for resolved in &plan.userdata {
        (resolved.registrar)(runtime, resolved, &resolved.installers)?;
    }
    Ok(())
}

/// Registers the direct primitive fields, except those the plan serves through a slot.
fn register_fields(runtime: &Runtime, plan: &RuntimePlan) -> Result<()> {
    for resolved in &plan.userdata {
        for (name, register_field) in &resolved.fields {
            let served_by_slot = resolved
                .members
                .iter()
                .any(|member| member.kind == MemberKind::Field && member.name == *name && member.through_slot);
            if !served_by_slot {
                register_field(runtime)?;
            }
        }
    }
    Ok(())
}

/// The direct plan and the generic VM callbacks for every tagged type with direct slots.
fn register_direct(runtime: &Runtime, plan: &RuntimePlan) -> Result<()> {
    let entries = plan.plan_entries();
    if entries.is_empty() {
        return Ok(());
    }
    let mut direct = DirectPlanBuilder::new(runtime);
    for entry in entries {
        direct.push_resolved(entry)?;
    }
    direct.finish()?;
    for resolved in plan.userdata.iter().filter(|u| u.has_direct_slots()) {
        super::dispatch::register(runtime, resolved)?;
    }
    runtime.shared().publish_direct_entries();
    Ok(())
}

/// Compiler knowledge derived from the same declarations: frozen modules exposed as globals are
/// known libraries with typed and constant members; tagged types are the userdata type list.
fn compile_options(plan: &RuntimePlan, members: &Rc<ModuleMembers>) -> CompileOptions {
    let mut options = CompileOptions::default();
    let mut known: Vec<String> =
        plan.modules.iter().filter(|module| module.frozen).filter_map(|module| module.global.clone()).collect();
    known.sort();
    options.known_libraries =
        known.iter().map(|name| CString::new(name.as_str()).expect("global names have no NUL")).collect();
    if !known.is_empty() {
        options.library_members = Some(Rc::clone(members) as Rc<dyn LibraryMembers>);
    }
    // Userdata types are named to the compiler by class name (what `.d.luau` declares and what
    // scripts annotate); with native code on, type information reaches the code generator.
    options.userdata_types = tagged_in_order(plan)
        .iter()
        .map(|u| CString::new(super::typedefs::class_name(&u.key)).expect("class names have no NUL"))
        .collect();
    #[cfg(feature = "jit")]
    if plan.policy.native_code.is_some() {
        options.type_info_level = 1;
    }
    options
}

/// Type erasure helper for `Runtime::state`.
pub(crate) fn downcast<S: 'static>(state: &Rc<dyn Any>) -> Option<Rc<S>> {
    Rc::clone(state).downcast::<S>().ok()
}
