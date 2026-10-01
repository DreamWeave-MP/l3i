//! Luau type definitions (`.d.luau`) rendered from the resolved plan, so the definition file
//! reflects the final composition (owner plus every augmentation) rather than hand-kept blobs.

use super::install::ModuleMembers;
use super::plan::RuntimePlan;
use super::{MemberKind, ModuleMemberKind, ResolvedUserdata, ViewKind};
use crate::source::CompileConstant;

/// A Luau identifier for a stable key: non-identifier characters become `_`.
pub(crate) fn class_name(key: &str) -> String {
    let mut name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

fn render_userdata(out: &mut String, userdata: &ResolvedUserdata) {
    use std::fmt::Write;
    let _ = writeln!(
        out,
        "-- {} (owned by {}; {})",
        userdata.key,
        userdata.owner,
        match userdata.tag {
            Some(tag) => format!("tag {tag}"),
            None => "untagged".to_owned(),
        }
    );
    if let Some(doc) = &userdata.doc {
        for line in doc.lines() {
            let _ = writeln!(out, "-- {line}");
        }
    }
    let _ = writeln!(out, "declare extern type {} with", class_name(&userdata.key));
    let mut seen_pairs = std::collections::HashSet::new();
    for member in &userdata.members {
        if let Some(doc) = &member.doc {
            for line in doc.lines() {
                let _ = writeln!(out, "    -- {line}");
            }
        }
        match member.kind {
            MemberKind::Method => {
                let signature = member.signature.clone().unwrap_or_else(|| "(self, ...any): any".to_owned());
                let _ = writeln!(out, "    function {}{}", member.name, signature);
            }
            MemberKind::Getter | MemberKind::Setter | MemberKind::Field => {
                if !seen_pairs.insert(member.name.clone()) {
                    continue;
                }
                let ty = member
                    .signature
                    .clone()
                    .or_else(|| {
                        userdata
                            .members
                            .iter()
                            .find(|m| m.name == member.name && m.signature.is_some())
                            .and_then(|m| m.signature.clone())
                    })
                    .unwrap_or_else(|| "any".to_owned());
                let _ = writeln!(out, "    {}: {ty}", member.name);
            }
        }
    }
    if let Some(view) = &userdata.view {
        // What the checker needs for `#v`, `v[i]`, and `for _, x in v` on an extern type: an
        // indexer and the metamethods as functions (`__index` as a function does not count).
        let item = &view.item;
        if view.kind == ViewKind::Sequence {
            let _ = writeln!(out, "    function __len(self): number");
            let _ = writeln!(out, "    [number]: {item}?");
        }
        let _ = writeln!(out, "    function __iter(self): (({{}}, number) -> (number?, {item}), {{}}, number)");
    }
    let _ = writeln!(out, "end\n");
}

/// Modules in an order where a type referenced by another module's signature is declared
/// first: a definitions file has no forward references between exported types. Cycles keep
/// the plan's order.
fn module_order(plan: &RuntimePlan) -> Vec<usize> {
    let names: Vec<String> = plan.modules.iter().map(|m| format!("Module_{}", class_name(&m.path))).collect();
    let mut remaining: Vec<usize> = (0..plan.modules.len()).collect();
    let mut order = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let position = remaining.iter().position(|&i| {
            let module = &plan.modules[i];
            !remaining.iter().any(|&j| {
                j != i
                    && module
                        .members
                        .iter()
                        .any(|m| m.signature.as_deref().is_some_and(|s| s.contains(names[j].as_str())))
            })
        });
        let next = position.unwrap_or(0);
        order.push(remaining.remove(next));
    }
    order
}

fn constant_type(constant: &CompileConstant) -> &'static str {
    match constant {
        CompileConstant::Nil => "nil",
        CompileConstant::Boolean(_) => "boolean",
        CompileConstant::Number(_) => "number",
        CompileConstant::Integer(_) => "integer",
        CompileConstant::Vector(..) => "vector",
        CompileConstant::String(_) => "string",
    }
}

/// Renders the definitions for `plan`: every userdata type and every module with its declared
/// members. The plan knows the whole shape before a VM exists, so nothing here waits for an
/// installed runtime; `members` (a runtime's installed record) is accepted for callers that
/// have one and ignored.
pub(crate) fn render_with(plan: &RuntimePlan, members: Option<&ModuleMembers>) -> String {
    use std::fmt::Write;
    let _ = members;
    let mut out = String::new();
    let _ = writeln!(out, "-- Generated by l3i from the runtime plan; do not edit.");
    let _ = writeln!(out, "-- Extensions: {}\n", plan.installation_order().join(", "));
    for userdata in &plan.userdata {
        render_userdata(&mut out, userdata);
    }
    for &index in &plan.order {
        let descriptor = &plan.descriptors[index];
        if descriptor.type_aliases().is_empty() {
            continue;
        }
        let _ = writeln!(out, "-- types declared by {}", descriptor.id());
        for (name, definition) in descriptor.type_aliases() {
            let _ = writeln!(out, "export type {name} = {definition}");
        }
        out.push('\n');
    }
    for index in module_order(plan) {
        let module = &plan.modules[index];
        let type_name = format!("Module_{}", class_name(&module.path));
        let _ = writeln!(out, "-- module {} (provided by {})", module.path, module.provider);
        if let Some(doc) = &module.doc {
            for line in doc.lines() {
                let _ = writeln!(out, "-- {line}");
            }
        }
        let _ = writeln!(out, "export type {type_name} = {}", module_type(module));
        if let Some(global) = &module.global {
            let _ = writeln!(out, "declare {global}: {type_name}");
        }
        out.push('\n');
    }
    out
}

/// The Luau type of a module's table: every declared member with its signature, docs as
/// comments. Shared by the definitions file and the module stubs the analyzer requires.
pub(crate) fn module_type(module: &super::plan::ResolvedModule) -> String {
    use std::fmt::Write;
    let mut out = String::from("{\n");
    for member in &module.members {
        if let Some(doc) = &member.doc {
            for line in doc.lines() {
                let _ = writeln!(out, "    -- {line}");
            }
        }
        let ty = match (&member.signature, &member.kind) {
            (Some(signature), _) => signature.clone(),
            (None, ModuleMemberKind::Function) => "(...any) -> ...any".to_owned(),
            (None, ModuleMemberKind::Constant(constant)) => constant_type(constant).to_owned(),
            (None, ModuleMemberKind::Installed) => "any".to_owned(),
        };
        let _ = writeln!(out, "    {}: {ty},", member.name);
    }
    out.push('}');
    out
}

/// The source the analyzer reads for `require("<module path>")`: a module whose returned
/// value has the module's declared type, so canonical requires type check against the plan.
pub(crate) fn module_stub(module: &super::plan::ResolvedModule) -> String {
    format!(
        "--!strict\n-- l3i stub for {} (provided by {}); the runtime module has this shape.\nlocal module: {} = (nil :: any)\nreturn module\n",
        module.path,
        module.provider,
        module_type(module)
    )
}

pub(crate) fn render(plan: &RuntimePlan) -> String {
    render_with(plan, None)
}
