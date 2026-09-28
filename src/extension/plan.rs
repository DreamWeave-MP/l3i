//! Finalising a set of extensions into one immutable [`RuntimePlan`].

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use super::{
    Extension, ExtensionDescriptor, MemberKind, RuntimePolicy, TagPolicy, UserdataDecl, debug_root,
    validate_extension_id,
};
use crate::TAG_LIMIT;
use crate::direct::plan::{MAX_ATOM_SPAN, MAX_SLOT};
use crate::direct::{AccessKind, Atom, AtomCatalogue};
use crate::error::{Error, Result};
use crate::runtime::MemoryCategory;
use crate::userdata::RuntimeTag;

/// A declared member with its VM-local resolution.
#[derive(Clone, Debug)]
pub struct ResolvedMember {
    pub name: String,
    pub kind: MemberKind,
    /// The atom this VM's catalogue gives the member name.
    pub atom: Atom,
    /// The direct plan slot: every method, getter, and setter of a tagged type has one; direct
    /// fields (unless `through_slot`) and untagged types' members have none.
    pub slot: Option<u16>,
    /// A direct field whose name is also a method, getter, or setter somewhere else in the plan.
    /// Luau rewrites every `obj.name` whose key has an atom into the direct-access opcode, which
    /// consults the tag's index callback and never Luau's field table, so such a field is served
    /// through a plan slot like a getter (about 33 ns instead of 12) rather than failing the plan.
    pub through_slot: bool,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub contributor: &'static str,
}

impl ResolvedMember {
    /// The access kind the member's direct slot serves.
    pub fn access_kind(&self) -> AccessKind {
        match self.kind {
            MemberKind::Method => AccessKind::Namecall,
            MemberKind::Getter | MemberKind::Field => AccessKind::Index,
            MemberKind::Setter => AccessKind::NewIndex,
        }
    }
}

/// A userdata type after ownership and augmentations merged.
#[derive(Clone)]
pub struct ResolvedUserdata {
    pub key: String,
    pub type_id: TypeId,
    pub type_name: &'static str,
    pub owner: &'static str,
    pub policy: TagPolicy,
    /// The tag this VM assigned, or `None` for the canonical untagged metatable.
    pub tag: Option<RuntimeTag>,
    pub members: Vec<ResolvedMember>,
    pub doc: Option<String>,
    pub(crate) installers: Vec<super::install::SharedInstaller>,
    pub(crate) fields: Vec<(String, super::install::SharedFieldRegistrar)>,
    pub(crate) registrar: super::install::Registrar,
}

impl std::fmt::Debug for ResolvedUserdata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedUserdata")
            .field("key", &self.key)
            .field("type_id", &self.type_id)
            .field("type_name", &self.type_name)
            .field("owner", &self.owner)
            .field("policy", &self.policy)
            .field("tag", &self.tag)
            .field("members", &self.members)
            .field("doc", &self.doc)
            .finish_non_exhaustive()
    }
}

impl ResolvedUserdata {
    pub fn member(&self, name: &str) -> Option<&ResolvedMember> {
        self.members.iter().find(|member| member.name == name)
    }

    /// True when the type has any direct member that resolved to a slot.
    pub fn has_direct_slots(&self) -> bool {
        self.members.iter().any(|member| member.slot.is_some())
    }
}

/// A module after planning.
#[derive(Clone)]
pub struct ResolvedModule {
    pub path: String,
    pub frozen: bool,
    pub provider: &'static str,
    pub doc: Option<String>,
    /// The compatibility global the policy exposes it as, if any.
    pub global: Option<String>,
    pub(crate) functions: Vec<(String, super::install::SharedModuleFunction)>,
    pub(crate) constants: Vec<(String, crate::source::CompileConstant)>,
}

impl std::fmt::Debug for ResolvedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModule")
            .field("path", &self.path)
            .field("frozen", &self.frozen)
            .field("provider", &self.provider)
            .field("doc", &self.doc)
            .field("global", &self.global)
            .field("functions", &self.functions.iter().map(|(name, _)| name).collect::<Vec<_>>())
            .field("constants", &self.constants)
            .finish()
    }
}

/// Collects extensions, services, and policy into a [`RuntimePlan`].
pub struct RuntimePlanBuilder {
    policy: RuntimePolicy,
    extensions: Vec<Box<dyn Extension>>,
    services: HashMap<TypeId, (&'static str, Rc<dyn Any>)>,
    pinned_tags: BTreeMap<String, RuntimeTag>,
}

impl Default for RuntimePlanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimePlanBuilder {
    pub fn new() -> Self {
        RuntimePlanBuilder {
            policy: RuntimePolicy::default(),
            extensions: Vec::new(),
            services: HashMap::new(),
            pinned_tags: BTreeMap::new(),
        }
    }

    pub fn policy(mut self, policy: RuntimePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// A host service extensions may look up by type at install time.
    pub fn service<S: 'static>(mut self, service: S) -> Self {
        self.services.insert(TypeId::of::<S>(), (std::any::type_name::<S>(), Rc::new(service)));
        self
    }

    /// Pins the userdata type with stable `key` to `tag` in every runtime from this plan.
    pub fn pin_tag(mut self, key: &str, tag: RuntimeTag) -> Self {
        self.pinned_tags.insert(key.to_owned(), tag);
        self
    }

    pub fn extension(mut self, extension: impl Extension) -> Self {
        self.extensions.push(Box::new(extension));
        self
    }

    pub fn boxed_extension(mut self, extension: Box<dyn Extension>) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Runs every `describe`, resolves the plan, and freezes it.
    pub fn finalize(self) -> Result<Rc<RuntimePlan>> {
        let RuntimePlanBuilder { policy, extensions, services, pinned_tags } = self;

        // 1. Describe.
        let mut descriptors = Vec::with_capacity(extensions.len());
        let mut seen = HashSet::new();
        for extension in &extensions {
            let id = extension.id();
            validate_extension_id(id)?;
            if !seen.insert(id) {
                return Err(Error::logic(format!("extension '{id}' is registered twice")));
            }
            let mut descriptor = ExtensionDescriptor::new(id);
            extension.describe(&mut descriptor)?;
            descriptors.push(descriptor);
        }

        // 2. Dependencies: every `requires` present, no cycles, deterministic order.
        let order = dependency_order(&descriptors)?;

        // 3. Modules: unique paths.
        let mut modules = Vec::new();
        let mut module_paths = HashSet::new();
        for &index in &order {
            for module in descriptors[index].modules() {
                if !module_paths.insert(module.path.clone()) {
                    return Err(Error::logic(format!(
                        "module '{}' is provided twice (second time by '{}')",
                        module.path, module.provider
                    )));
                }
                let global =
                    policy.compat_globals.iter().find(|(path, _)| *path == module.path).map(|(_, g)| g.clone());
                modules.push(ResolvedModule {
                    path: module.path.clone(),
                    frozen: module.frozen,
                    provider: module.provider,
                    doc: module.doc.clone(),
                    global,
                    functions: module.functions.clone(),
                    constants: module.constants.clone(),
                });
            }
        }
        for (path, _) in &policy.compat_globals {
            if !module_paths.contains(path) {
                return Err(Error::logic(format!("compat global for '{path}', which no extension provides")));
            }
        }

        // 4. Userdata: owners, then augmentations merged in dependency order.
        let mut userdata = merge_userdata(&descriptors, &order)?;

        // 5. Tags: pinned, then Required, then Preferred while tags remain.
        assign_tags(&mut userdata, &pinned_tags, policy.first_tag)?;

        let atom_catalogue = resolve_atoms(&mut userdata)?;
        assign_slots(&mut userdata)?;
        let categories = resolve_categories(&descriptors)?;
        check_services_and_capabilities(&descriptors, &services, &policy)?;
        let roots = debug_roots(&policy, &descriptors, &userdata);
        let packed_kinds = resolve_packed_kinds(&descriptors, &order)?;

        Ok(Rc::new(RuntimePlan {
            policy,
            extensions,
            descriptors,
            order,
            modules,
            userdata,
            atoms: atom_catalogue,
            categories,
            services,
            debug_roots: roots,
            packed_kinds,
        }))
    }
}

/// Every declared packed kind, validated: numbers in the host range (or l3i's own types), and
/// one type per number across the whole plan.
fn resolve_packed_kinds(descriptors: &[ExtensionDescriptor], order: &[usize]) -> Result<Vec<crate::packed::PackedKind>> {
    let mut kinds: Vec<(crate::packed::PackedKind, &'static str)> = Vec::new();
    for &index in order {
        for kind in descriptors[index].packed_kinds() {
            kind.validate()?;
            match kinds.iter().find(|(existing, _)| existing.kind == kind.kind) {
                Some((existing, _)) if existing.type_id == kind.type_id => {}
                Some((existing, owner)) => {
                    return Err(Error::logic(format!(
                        "packed kind {} is declared for {} by '{owner}' and for {} by '{}'",
                        kind.kind,
                        existing.name,
                        kind.name,
                        descriptors[index].id()
                    )));
                }
                None => kinds.push((*kind, descriptors[index].id())),
            }
        }
    }
    for builtin in crate::packed::builtin_kinds() {
        if let Some((kind, owner)) = kinds.iter().find(|(k, _)| k.kind == builtin.kind && k.type_id != builtin.type_id) {
            return Err(Error::logic(format!(
                "packed kind {} belongs to l3i ({}); '{owner}' declares it for {}",
                builtin.kind, builtin.name, kind.name
            )));
        }
    }
    Ok(kinds.into_iter().map(|(kind, _)| kind).collect())
}

/// Atoms for every method, getter, and setter name, densely from 1, written back into the
/// members. Direct field names get no atom: Luau rewrites every `obj.name` whose key has an
/// atom into the direct-access opcode, which consults the tag's index callback and never the
/// direct-field table, so an atom would take the field off its fast path. A field whose name is
/// another member kind on a *different* type keeps the name and is served through a plan slot
/// instead (`ResolvedMember::through_slot`); on the same type the two would be one key with two
/// meanings, which `add_members` has already rejected.
fn resolve_atoms(userdata: &mut [ResolvedUserdata]) -> Result<AtomCatalogue> {
    let mut names = BTreeSet::new();
    let mut field_names = BTreeSet::new();
    for resolved in userdata.iter() {
        for member in &resolved.members {
            if member.kind == MemberKind::Field {
                field_names.insert(member.name.clone());
            } else {
                names.insert(member.name.clone());
            }
        }
    }
    let through_slot: BTreeSet<&String> = field_names.intersection(&names).collect();
    for resolved in userdata.iter_mut() {
        for member in &mut resolved.members {
            if member.kind == MemberKind::Field && through_slot.contains(&member.name) {
                member.through_slot = true;
            }
        }
    }
    if names.len() > MAX_ATOM_SPAN || names.len() >= usize::from(i16::MAX as u16) {
        return Err(Error::logic(format!(
            "{} member names exceed the {MAX_ATOM_SPAN} atoms one plan may use",
            names.len()
        )));
    }
    let atoms: Vec<(String, Atom)> = names.into_iter().zip(1..).map(|(name, atom)| (name, atom as Atom)).collect();
    let atom_of: HashMap<&str, Atom> = atoms.iter().map(|(name, atom)| (name.as_str(), *atom)).collect();
    for resolved in userdata.iter_mut() {
        for member in &mut resolved.members {
            if member.kind != MemberKind::Field || member.through_slot {
                member.atom = atom_of[member.name.as_str()];
            }
        }
    }
    AtomCatalogue::try_new(atoms.iter().map(|(name, atom)| (name.clone(), *atom)))
}

/// Direct slots for every method, getter, and setter of a tagged type, densely from 1. Every
/// bound member of a tagged type dispatches through the plan (a slot lookup is cheaper than the
/// metamethod fallback for cold members too). Direct fields have no slot unless their name is
/// an atom elsewhere: Luau serves the others from its own field table.
fn assign_slots(userdata: &mut [ResolvedUserdata]) -> Result<()> {
    let mut next_slot: u16 = 1;
    for resolved in userdata.iter_mut() {
        if resolved.tag.is_none() {
            continue;
        }
        for member in &mut resolved.members {
            if member.kind == MemberKind::Field && !member.through_slot {
                continue;
            }
            if next_slot > MAX_SLOT {
                return Err(Error::logic(format!("more than {MAX_SLOT} direct members in one plan")));
            }
            member.slot = Some(next_slot);
            next_slot += 1;
        }
    }
    Ok(())
}

/// Symbolic memory categories to 1..=255 (0 is the shared category).
fn resolve_categories(descriptors: &[ExtensionDescriptor]) -> Result<BTreeMap<String, MemoryCategory>> {
    let mut names = BTreeSet::new();
    for descriptor in descriptors {
        names.extend(descriptor.memory_categories().map(str::to_owned));
    }
    if names.len() > 255 {
        return Err(Error::logic("more than 255 memory categories requested"));
    }
    Ok(names.into_iter().zip(1u8..).map(|(name, id)| (name, MemoryCategory(id))).collect())
}

fn check_services_and_capabilities(
    descriptors: &[ExtensionDescriptor],
    services: &HashMap<TypeId, (&'static str, Rc<dyn Any>)>,
    policy: &RuntimePolicy,
) -> Result<()> {
    for descriptor in descriptors {
        for service in descriptor.services() {
            if !services.contains_key(&service.type_id) {
                return Err(Error::logic(format!(
                    "extension '{}' requires host service {} which the plan does not provide",
                    descriptor.id(),
                    service.type_name
                )));
            }
        }
        for capability in descriptor.capabilities() {
            if !policy.grants(capability) {
                return Err(Error::permission(format!(
                    "extension '{}' requires capability '{capability}' which the runtime policy does not grant",
                    descriptor.id()
                )));
            }
        }
    }
    Ok(())
}

/// The policy's debug roots plus one per extension id family and per userdata type name root.
fn debug_roots(
    policy: &RuntimePolicy,
    descriptors: &[ExtensionDescriptor],
    userdata: &[ResolvedUserdata],
) -> Vec<&'static str> {
    let mut roots: Vec<&'static str> = policy.debug_roots.clone();
    for descriptor in descriptors {
        let root = debug_root(descriptor.id());
        if !roots.iter().any(|existing| *existing == root) {
            roots.push(Box::leak(root.into_boxed_str()));
        }
    }
    for resolved in userdata {
        if let Some(root) = resolved.type_name.split('.').next()
            && !roots.contains(&root)
        {
            roots.push(Box::leak(root.to_owned().into_boxed_str()));
        }
    }
    roots
}

/// Kahn's algorithm with lexicographic tie-breaking, so the order depends on the graph only.
fn dependency_order(descriptors: &[ExtensionDescriptor]) -> Result<Vec<usize>> {
    let index_of: HashMap<&str, usize> = descriptors.iter().enumerate().map(|(i, d)| (d.id(), i)).collect();
    let mut dependencies: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); descriptors.len()];
    for (index, descriptor) in descriptors.iter().enumerate() {
        for required in descriptor.dependencies() {
            let Some(&dependency) = index_of.get(required) else {
                return Err(Error::logic(format!(
                    "extension '{}' requires '{required}', which is not in the plan",
                    descriptor.id()
                )));
            };
            if dependency == index {
                return Err(Error::logic(format!("extension '{}' requires itself", descriptor.id())));
            }
            dependencies[index].insert(dependency);
        }
        for optional in descriptor.optional_dependencies() {
            if let Some(&dependency) = index_of.get(optional)
                && dependency != index
            {
                dependencies[index].insert(dependency);
            }
        }
    }
    let mut remaining: BTreeSet<usize> = (0..descriptors.len()).collect();
    let mut order = Vec::with_capacity(descriptors.len());
    let mut done: HashSet<usize> = HashSet::new();
    while !remaining.is_empty() {
        let mut ready: Vec<usize> =
            remaining.iter().copied().filter(|&i| dependencies[i].iter().all(|d| done.contains(d))).collect();
        if ready.is_empty() {
            let cycle = describe_cycle(descriptors, &dependencies, &remaining);
            return Err(Error::logic(format!("extension dependency cycle: {cycle}")));
        }
        ready.sort_by_key(|&i| descriptors[i].id());
        let next = ready[0];
        remaining.remove(&next);
        done.insert(next);
        order.push(next);
    }
    Ok(order)
}

fn describe_cycle(
    descriptors: &[ExtensionDescriptor],
    dependencies: &[BTreeSet<usize>],
    remaining: &BTreeSet<usize>,
) -> String {
    // Walk from the lexicographically first stuck node until a node repeats.
    let start = *remaining.iter().min_by_key(|&&i| descriptors[i].id()).expect("non-empty");
    let mut chain = vec![start];
    let mut current = start;
    while let Some(&next) = dependencies[current].iter().find(|d| remaining.contains(d)) {
        if let Some(position) = chain.iter().position(|&i| i == next) {
            chain.push(next);
            return chain[position..].iter().map(|&i| descriptors[i].id()).collect::<Vec<_>>().join(" -> ");
        }
        chain.push(next);
        current = next;
    }
    chain.iter().map(|&i| descriptors[i].id()).collect::<Vec<_>>().join(" -> ")
}

fn merge_userdata(descriptors: &[ExtensionDescriptor], order: &[usize]) -> Result<Vec<ResolvedUserdata>> {
    let mut by_key: BTreeMap<String, ResolvedUserdata> = BTreeMap::new();
    let mut owner_of_type: HashMap<TypeId, String> = HashMap::new();
    for &index in order {
        for decl in descriptors[index].owned_userdata() {
            if let Some(existing) = by_key.get(&decl.key) {
                return Err(Error::logic(format!(
                    "userdata '{}' is owned by both '{}' and '{}'",
                    decl.key,
                    existing.owner,
                    descriptors[index].id()
                )));
            }
            if let Some(other_key) = owner_of_type.get(&decl.type_id) {
                return Err(Error::logic(format!(
                    "Rust type {} is registered under two keys, '{other_key}' and '{}'",
                    decl.type_name, decl.key
                )));
            }
            owner_of_type.insert(decl.type_id, decl.key.clone());
            let mut resolved = ResolvedUserdata {
                key: decl.key.clone(),
                type_id: decl.type_id,
                type_name: decl.type_name,
                owner: descriptors[index].id(),
                policy: decl.tag,
                tag: None,
                members: Vec::new(),
                doc: decl.doc.clone(),
                installers: Vec::new(),
                fields: Vec::new(),
                registrar: decl.registrar,
            };
            add_members(&mut resolved, decl)?;
            by_key.insert(decl.key.clone(), resolved);
        }
    }
    for &index in order {
        let descriptor = &descriptors[index];
        for decl in descriptor.augmentations() {
            let Some(resolved) = by_key.get_mut(&decl.key) else {
                return Err(Error::logic(format!(
                    "extension '{}' augments userdata '{}', which no extension owns",
                    descriptor.id(),
                    decl.key
                )));
            };
            if resolved.type_id != decl.type_id {
                return Err(Error::logic(format!(
                    "extension '{}' augments '{}' with Rust type {}, but its owner '{}' registered {}",
                    descriptor.id(),
                    decl.key,
                    decl.type_name,
                    resolved.owner,
                    resolved.type_name
                )));
            }
            if resolved.owner != descriptor.id() && !descriptor.dependencies().any(|id| id == resolved.owner) {
                return Err(Error::logic(format!(
                    "extension '{}' augments '{}' but does not require its owner '{}'",
                    descriptor.id(),
                    decl.key,
                    resolved.owner
                )));
            }
            add_members(resolved, decl)?;
        }
    }
    Ok(by_key.into_values().collect())
}

fn add_members(resolved: &mut ResolvedUserdata, decl: &UserdataDecl) -> Result<()> {
    resolved.installers.extend(decl.installers.iter().cloned());
    resolved.fields.extend(decl.fields.iter().cloned());
    for member in &decl.members {
        if let Some(existing) = resolved.members.iter().find(|m| m.name == member.name) {
            // A getter and a setter of one name are a pair; anything else collides.
            let pair = matches!(
                (existing.kind, member.kind),
                (MemberKind::Getter, MemberKind::Setter) | (MemberKind::Setter, MemberKind::Getter)
            );
            if !pair && existing.contributor == member.contributor {
                return Err(Error::logic(format!(
                    "userdata '{}' member '{}' is declared twice by '{}' (as {:?} and {:?})",
                    resolved.key, member.name, member.contributor, existing.kind, member.kind
                )));
            }
            if !pair {
                return Err(Error::logic(format!(
                    "userdata '{}' member '{}' is declared by both '{}' and '{}'",
                    resolved.key, member.name, existing.contributor, member.contributor
                )));
            }
        }
        if member.name.is_empty() || member.name.contains('\0') {
            return Err(Error::logic(format!("userdata '{}' has an invalid member name", resolved.key)));
        }
        resolved.members.push(ResolvedMember {
            name: member.name.clone(),
            kind: member.kind,
            atom: 0,
            slot: None,
            through_slot: false,
            signature: member.signature.clone(),
            doc: member.doc.clone(),
            contributor: member.contributor,
        });
    }
    Ok(())
}

fn assign_tags(
    userdata: &mut [ResolvedUserdata],
    pinned: &BTreeMap<String, RuntimeTag>,
    first_tag: RuntimeTag,
) -> Result<()> {
    if first_tag == 0 || first_tag >= TAG_LIMIT {
        return Err(Error::logic(format!("first_tag must be in 1..{TAG_LIMIT}")));
    }
    let mut taken: BTreeSet<RuntimeTag> = BTreeSet::new();
    for (key, &tag) in pinned {
        if tag == 0 || tag >= TAG_LIMIT {
            return Err(Error::logic(format!("pinned tag {tag} for '{key}' is outside 1..{TAG_LIMIT}")));
        }
        let Some(resolved) = userdata.iter_mut().find(|u| u.key == *key) else {
            return Err(Error::logic(format!("pinned tag for '{key}', which no extension owns")));
        };
        if resolved.policy == TagPolicy::Never {
            return Err(Error::logic(format!("'{key}' is pinned to tag {tag} but declares TagPolicy::Never")));
        }
        if !taken.insert(tag) {
            return Err(Error::logic(format!("tag {tag} is pinned twice")));
        }
        resolved.tag = Some(tag);
    }
    let mut free = (first_tag..TAG_LIMIT).filter(|tag| !taken.contains(tag));
    // Direct fields need a tag whatever the declared policy says.
    for resolved in userdata.iter_mut() {
        if resolved.tag.is_none()
            && resolved.policy == TagPolicy::Never
            && resolved.members.iter().any(|m| m.kind == MemberKind::Field)
        {
            return Err(Error::logic(format!(
                "'{}' declares direct fields, which need a tag, but TagPolicy::Never",
                resolved.key
            )));
        }
    }
    for policy in [TagPolicy::Required, TagPolicy::Preferred] {
        for resolved in userdata.iter_mut() {
            let wants = resolved.policy == policy
                || (policy == TagPolicy::Required && resolved.members.iter().any(|m| m.kind == MemberKind::Field));
            if resolved.tag.is_some() || !wants {
                continue;
            }
            match free.next() {
                Some(tag) => resolved.tag = Some(tag),
                None if policy == TagPolicy::Required => {
                    return Err(Error::logic(format!("no Luau tag left for '{}', which requires one", resolved.key)));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// An immutable, reusable description of one runtime composition.
pub struct RuntimePlan {
    pub(crate) policy: RuntimePolicy,
    pub(crate) extensions: Vec<Box<dyn Extension>>,
    pub(crate) descriptors: Vec<ExtensionDescriptor>,
    pub(crate) order: Vec<usize>,
    pub(crate) modules: Vec<ResolvedModule>,
    pub(crate) userdata: Vec<ResolvedUserdata>,
    pub(crate) atoms: AtomCatalogue,
    pub(crate) categories: BTreeMap<String, MemoryCategory>,
    pub(crate) services: HashMap<TypeId, (&'static str, Rc<dyn Any>)>,
    pub(crate) debug_roots: Vec<&'static str>,
    pub(crate) packed_kinds: Vec<crate::packed::PackedKind>,
}

impl RuntimePlan {
    pub fn builder() -> RuntimePlanBuilder {
        RuntimePlanBuilder::new()
    }

    pub fn policy(&self) -> &RuntimePolicy {
        &self.policy
    }

    /// Extension ids in installation order.
    pub fn installation_order(&self) -> Vec<&'static str> {
        self.order.iter().map(|&i| self.descriptors[i].id()).collect()
    }

    pub fn descriptors(&self) -> &[ExtensionDescriptor] {
        &self.descriptors
    }

    pub fn userdata(&self) -> &[ResolvedUserdata] {
        &self.userdata
    }

    pub fn userdata_by_key(&self, key: &str) -> Option<&ResolvedUserdata> {
        self.userdata.iter().find(|u| u.key == key)
    }

    pub fn userdata_of_type<T: 'static>(&self) -> Option<&ResolvedUserdata> {
        self.userdata.iter().find(|u| u.type_id == TypeId::of::<T>())
    }

    pub fn modules(&self) -> &[ResolvedModule] {
        &self.modules
    }

    /// The tag runtimes from this plan give the type with stable `key`.
    pub fn tag_of(&self, key: &str) -> Option<RuntimeTag> {
        self.userdata_by_key(key).and_then(|u| u.tag)
    }

    /// The atom every runtime from this plan gives `member`.
    pub fn atom_of(&self, member: &str) -> Option<Atom> {
        self.atoms.atom_of(member)
    }

    pub fn atoms(&self) -> &AtomCatalogue {
        &self.atoms
    }

    /// The Luau memory category for a symbolic name.
    pub fn memory_category(&self, name: &str) -> Option<MemoryCategory> {
        self.categories.get(name).copied()
    }

    pub fn memory_categories(&self) -> &BTreeMap<String, MemoryCategory> {
        &self.categories
    }

    pub fn service<S: 'static>(&self) -> Option<Rc<S>> {
        self.services.get(&TypeId::of::<S>()).and_then(|(_, service)| Rc::clone(service).downcast::<S>().ok())
    }

    /// The packed scalar kinds the plan's extensions declared (l3i's own are implicit).
    pub fn packed_kinds(&self) -> &[crate::packed::PackedKind] {
        &self.packed_kinds
    }

    pub fn debug_roots(&self) -> &[&'static str] {
        &self.debug_roots
    }

    /// The direct slot entries this plan resolves, for the dense dispatch table.
    pub(crate) fn plan_entries(&self) -> Vec<crate::direct::plan::PlanEntry> {
        let mut entries = Vec::new();
        for resolved in &self.userdata {
            let Some(tag) = resolved.tag else { continue };
            for member in &resolved.members {
                if let Some(slot) = member.slot {
                    entries.push(crate::direct::plan::PlanEntry {
                        slot,
                        tag,
                        atom: member.atom,
                        kind: member.access_kind(),
                        type_id: resolved.type_id,
                        type_name: resolved.type_name,
                        member: member.name.clone(),
                    });
                }
            }
        }
        entries
    }

    /// Luau type definitions (`.d.luau`) for every module and userdata type, after composition.
    pub fn type_definitions(&self) -> String {
        super::typedefs::render(self)
    }
}

impl std::fmt::Debug for RuntimePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimePlan")
            .field("order", &self.installation_order())
            .field("modules", &self.modules)
            .field("userdata", &self.userdata)
            .field("categories", &self.categories)
            .finish_non_exhaustive()
    }
}
