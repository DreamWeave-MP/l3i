//! Source syntax parsing for scripts: the `dream.luau` extension, module `@dream/luau`.
//!
//! ```lua
//! local luau = require('@dream/luau')
//! local result = luau.parse(source)
//! for _, stat in result.root.body do
//!   print(stat.kind, stat.line, stat.column)
//! end
//! ```
//!
//! `parse` defaults to the `l3i` dialect, exporting comprehensions as source nodes rather
//! than generated calls or locals. `{ dialect = "luau" }` selects strict stock Luau syntax.
//! `parse` runs `Luau::Parser` (the parser the compiler and the type checker use, under the
//! runtime's frozen fast flags) and builds the tree as Luau tables in native code: one table per
//! node, made at its final size, with every key string pushed once per call. Nothing here knows
//! a style or lint rule; it hands a script the same tree, comments and errors Luau's tools see.
//!
//! # The tree
//!
//! Every node has `kind`, the class name without `Ast` (`StatLocal`, `ExprCall`,
//! `TypeReference`), and an exact span: `line`, `column`, `endLine`, `endColumn`, 1-based, the
//! end column inclusive, columns counted in bytes. With `lineStarts[line]`, the 1-based source
//! index of each line's first byte, `string.sub(source, lineStarts[line] + column - 1,
//! lineStarts[endLine] + endColumn - 1)` is the node's exact text. The other fields are Luau's
//! member names (`thenbody`, `elsebody`, `func`, `args`); [`TYPES`] is the complete shape.
//!
//! A local (`Local`) is one table shared by its declaration and every `ExprLocal` that reads
//! it, so identity comparison resolves scopes with no scope tracking. Comprehensions have
//! ordered `ComprehensionGenerator` / `ComprehensionFilter` clauses and a projection; generator
//! sources precede their binding's scope. Scope depths and upvalues exclude generated functions.
//! `#[for ...]` remains an `ExprUnary` around an `ExprComprehension`.
//! The intrinsic `sum[for ...]` is an `ExprReduction` with `op = "sum"` and an inner
//! `ExprComprehension`; its outer span includes `sum`, while the inner span starts at `[`.
//! This intrinsic does not refer to any local/global named `sum`; member accesses are excluded.
//! A direct `range(first, last[, step])` generator source is an `ExprRange`; its arguments
//! retain original nodes and spans while compilation lowers it to a numeric loop.
//!
//! Missing surface expressions are `ExprError` with `isMissing = true` and no children.
//! Missing tokens have absent optional locations; their insertion spans are zero-width.
//! `messageIndex` is zero-based into `errors`, or -1 if no matching diagnostic is available.
//!
//! # Trivia
//!
//! Between two tokens there is only whitespace and comments, and `comments` lists every
//! comment with its span. With `{ tokens = true }`, `tokens` is Luau's own token stream as a
//! buffer of 12-byte records (`kind`, first and last 1-based source index, each a `u32`), the
//! kinds numbered by `luau.tokenKinds`; what precedes any node is then exact. In the `l3i`
//! dialect contiguous `=` and `>` tokens form one `symbol` token for `=>`. Trivia and token
//! offsets always refer to the original source; all 14 kind ids remain unchanged.
//!
//! # Errors
//!
//! A syntax error is data: `errors` lists each with its span, and the tree holds `ExprError`,
//! `StatError` and `TypeError` nodes where the parser recovered. `parse` raises only for a bad
//! argument or when the VM cannot allocate.

use std::ffi::{c_char, c_int};

use crate::bind::{Call, StackResults};
use crate::convert::BytesView;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::options::Options;
use crate::stack::{Scope, ValueView};
use crate::value::Table;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.luau";
/// The module path.
pub const MODULE: &str = "@dream/luau";

/// Token kinds as `luau.tokenKinds` numbers them, from 1. The order is part of the API.
pub const TOKEN_KINDS: [&str; 14] = [
    "name",
    "keyword",
    "number",
    "string",
    "longString",
    "interpolatedBegin",
    "interpolatedMid",
    "interpolatedEnd",
    "interpolatedSimple",
    "comment",
    "blockComment",
    "attribute",
    "symbol",
    "error",
];

const PARSE_DECLARATIONS: c_int = 1;
const PARSE_TOKENS: c_int = 2;
const PARSE_LUAU: c_int = 4;

mod ffi {
    use std::ffi::{c_char, c_int};

    use crate::raw::ffi::lua_State;

    unsafe extern "C-unwind" {
        /// Parses and pushes one result table; raises (unwinding) only on allocation failure.
        pub fn l3i_luau_parse(L: *mut lua_State, source: *const c_char, length: usize, flags: c_int) -> c_int;
    }
}

fn parse(call: &Call<'_>, source: BytesView<'_>, options: Option<ValueView<'_>>) -> Result<StackResults> {
    const WHAT: &str = "luau.parse";
    let mut flags = 0;
    if let Some(options) = options {
        let (declarations, tokens, stock_luau) = Options::read(call, options, WHAT, |o| {
            let declarations = o.optional::<bool>("declarations")?.unwrap_or(false);
            let tokens = o.optional::<bool>("tokens")?.unwrap_or(false);
            let dialect = o.optional::<String>("dialect")?;
            let stock_luau = match dialect.as_deref().unwrap_or("l3i") {
                "l3i" => false,
                "luau" => true,
                _ => return Err(Error::runtime(format!("{WHAT}.dialect: expected 'l3i' or 'luau'"))),
            };
            Ok((declarations, tokens, stock_luau))
        })?;
        if declarations {
            flags |= PARSE_DECLARATIONS;
        }
        if tokens {
            flags |= PARSE_TOKENS;
        }
        if stock_luau {
            flags |= PARSE_LUAU;
        }
    }
    if u32::try_from(source.len()).is_err() {
        return Err(Error::runtime(format!(
            "{WHAT}: a source of {} bytes is past the 4 GiB a span can address",
            source.len()
        )));
    }
    // SAFETY: the source is an argument slot, so the string or buffer stays alive and in place
    // for the call (Luau's collector never moves objects), and the builder runs no Luau code
    // that could write the buffer: it only creates tables and sets them raw. The shim pushes
    // exactly one value and raises only through Luau's error path, which unwinds to `pcall`.
    unsafe {
        let bytes = source.bytes_unchecked();
        ffi::l3i_luau_parse(call.state(), bytes.as_ptr().cast::<c_char>(), bytes.len(), flags);
    }
    Ok(StackResults)
}

/// The Luau types of the tree, in the order the definitions declare them: every node kind,
/// the unions over them (`dream_luau_Expr`, `dream_luau_Stat`, `dream_luau_Type`,
/// `dream_luau_TypePack`, `dream_luau_Node`), and the result.
pub const TYPES: &[(&str, &str)] = &[
    ("dream_luau_Span", "{ line: number, column: number, endLine: number, endColumn: number }"),
    (
        "dream_luau_BinaryOp",
        "\"+\" | \"-\" | \"*\" | \"/\" | \"//\" | \"%\" | \"^\" | \"..\" | \"~=\" | \"==\" | \"<\" | \"<=\" | \">\" | \">=\" | \"and\" | \"or\"",
    ),
    ("dream_luau_Access", "\"read\" | \"write\" | \"readwrite\""),
    (
        "dream_luau_Local",
        "{ kind: \"Local\", line: number, column: number, endLine: number, endColumn: number, name: string, isConst: boolean, functionDepth: number, loopDepth: number, annotation: dream_luau_Type?, shadow: dream_luau_Local? }",
    ),
    ("dream_luau_TypeList", "{ types: { dream_luau_Type }, tailType: dream_luau_TypePack? }"),
    (
        "dream_luau_Attr",
        "{ kind: \"Attr\", line: number, column: number, endLine: number, endColumn: number, type: \"checked\" | \"native\" | \"deprecated\" | \"debugnoinline\" | \"unknown\", name: string?, args: { dream_luau_Expr } }",
    ),
    (
        "dream_luau_GenericType",
        "{ kind: \"GenericType\", line: number, column: number, endLine: number, endColumn: number, name: string, defaultValue: dream_luau_Type? }",
    ),
    (
        "dream_luau_GenericTypePack",
        "{ kind: \"GenericTypePack\", line: number, column: number, endLine: number, endColumn: number, name: string, defaultValue: dream_luau_TypePack? }",
    ),
    (
        "dream_luau_TableItem",
        "{ kind: \"TableItem\", line: number, column: number, endLine: number, endColumn: number, itemKind: \"list\" | \"record\" | \"general\", key: dream_luau_Expr?, value: dream_luau_Expr }",
    ),
    (
        "dream_luau_TableProp",
        "{ kind: \"TableProp\", line: number, column: number, endLine: number, endColumn: number, name: string, type: dream_luau_Type, access: dream_luau_Access }",
    ),
    (
        "dream_luau_TableIndexer",
        "{ kind: \"TableIndexer\", line: number, column: number, endLine: number, endColumn: number, indexType: dream_luau_Type, resultType: dream_luau_Type, access: dream_luau_Access }",
    ),
    (
        "dream_luau_DeclaredProp",
        "{ kind: \"DeclaredProp\", line: number, column: number, endLine: number, endColumn: number, name: string, nameLocation: dream_luau_Span, type: dream_luau_Type, isMethod: boolean, access: dream_luau_Access }",
    ),
    (
        "dream_luau_ArgumentName",
        "{ kind: \"ArgumentName\", line: number, column: number, endLine: number, endColumn: number, name: string }",
    ),
    (
        "dream_luau_ComprehensionGenerator",
        "{ kind: \"ComprehensionGenerator\", line: number, column: number, endLine: number, endColumn: number, binding: dream_luau_Local?, source: dream_luau_Expr, inLocation: dream_luau_Span?, keywordLocation: dream_luau_Span, hasIn: boolean }",
    ),
    (
        "dream_luau_ComprehensionFilter",
        "{ kind: \"ComprehensionFilter\", line: number, column: number, endLine: number, endColumn: number, condition: dream_luau_Expr, keywordLocation: dream_luau_Span }",
    ),
    ("dream_luau_ComprehensionClause", "dream_luau_ComprehensionGenerator | dream_luau_ComprehensionFilter"),
    (
        "dream_luau_ExprRange",
        "{ kind: \"ExprRange\", line: number, column: number, endLine: number, endColumn: number, args: { dream_luau_Expr } }",
    ),
    (
        "dream_luau_ExprComprehension",
        "{ kind: \"ExprComprehension\", line: number, column: number, endLine: number, endColumn: number, clauses: { dream_luau_ComprehensionClause }, projection: dream_luau_Expr, openLocation: dream_luau_Span, closeLocation: dream_luau_Span?, arrowLocation: dream_luau_Span?, hasClose: boolean, hasArrow: boolean, complete: boolean }",
    ),
    (
        "dream_luau_ExprReduction",
        "{ kind: \"ExprReduction\", line: number, column: number, endLine: number, endColumn: number, op: \"sum\", expr: dream_luau_ExprComprehension }",
    ),
    (
        "dream_luau_ExprGroup",
        "{ kind: \"ExprGroup\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr }",
    ),
    (
        "dream_luau_ExprConstantNil",
        "{ kind: \"ExprConstantNil\", line: number, column: number, endLine: number, endColumn: number }",
    ),
    (
        "dream_luau_ExprConstantBool",
        "{ kind: \"ExprConstantBool\", line: number, column: number, endLine: number, endColumn: number, value: boolean }",
    ),
    (
        "dream_luau_ExprConstantNumber",
        "{ kind: \"ExprConstantNumber\", line: number, column: number, endLine: number, endColumn: number, value: number }",
    ),
    (
        "dream_luau_ExprConstantInteger",
        "{ kind: \"ExprConstantInteger\", line: number, column: number, endLine: number, endColumn: number, value: integer }",
    ),
    (
        "dream_luau_ExprConstantString",
        "{ kind: \"ExprConstantString\", line: number, column: number, endLine: number, endColumn: number, value: string, quoteStyle: \"double\" | \"single\" | \"backtick\" | \"long\" | \"unquoted\" }",
    ),
    (
        "dream_luau_ExprLocal",
        "{ kind: \"ExprLocal\", line: number, column: number, endLine: number, endColumn: number, ['local']: dream_luau_Local, upvalue: boolean }",
    ),
    (
        "dream_luau_ExprGlobal",
        "{ kind: \"ExprGlobal\", line: number, column: number, endLine: number, endColumn: number, name: string }",
    ),
    (
        "dream_luau_ExprVarargs",
        "{ kind: \"ExprVarargs\", line: number, column: number, endLine: number, endColumn: number }",
    ),
    (
        "dream_luau_ExprCall",
        "{ kind: \"ExprCall\", line: number, column: number, endLine: number, endColumn: number, func: dream_luau_Expr, args: { dream_luau_Expr }, self: boolean, typeArguments: { dream_luau_Type | dream_luau_TypePack }, argLocation: dream_luau_Span }",
    ),
    (
        "dream_luau_ExprIndexName",
        "{ kind: \"ExprIndexName\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr, index: string, indexLocation: dream_luau_Span, op: \".\" | \":\" }",
    ),
    (
        "dream_luau_ExprIndexExpr",
        "{ kind: \"ExprIndexExpr\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr, index: dream_luau_Expr }",
    ),
    (
        "dream_luau_ExprFunction",
        "{ kind: \"ExprFunction\", line: number, column: number, endLine: number, endColumn: number, attributes: { dream_luau_Attr }, generics: { dream_luau_GenericType }, genericPacks: { dream_luau_GenericTypePack }, self: dream_luau_Local?, args: { dream_luau_Local }, vararg: boolean, varargAnnotation: dream_luau_TypePack?, returnAnnotation: dream_luau_TypePack?, body: dream_luau_StatBlock, functionDepth: number, debugname: string?, argLocation: dream_luau_Span? }",
    ),
    (
        "dream_luau_ExprTable",
        "{ kind: \"ExprTable\", line: number, column: number, endLine: number, endColumn: number, items: { dream_luau_TableItem } }",
    ),
    (
        "dream_luau_ExprUnary",
        "{ kind: \"ExprUnary\", line: number, column: number, endLine: number, endColumn: number, op: \"not\" | \"-\" | \"#\", expr: dream_luau_Expr }",
    ),
    (
        "dream_luau_ExprBinary",
        "{ kind: \"ExprBinary\", line: number, column: number, endLine: number, endColumn: number, op: dream_luau_BinaryOp, left: dream_luau_Expr, right: dream_luau_Expr }",
    ),
    (
        "dream_luau_ExprTypeAssertion",
        "{ kind: \"ExprTypeAssertion\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr, annotation: dream_luau_Type }",
    ),
    (
        "dream_luau_ExprIfElse",
        "{ kind: \"ExprIfElse\", line: number, column: number, endLine: number, endColumn: number, condition: dream_luau_Expr, hasThen: boolean, trueExpr: dream_luau_Expr, hasElse: boolean, falseExpr: dream_luau_Expr, conditionLocal: dream_luau_Local? }",
    ),
    (
        "dream_luau_ExprInterpString",
        "{ kind: \"ExprInterpString\", line: number, column: number, endLine: number, endColumn: number, strings: { string }, expressions: { dream_luau_Expr } }",
    ),
    (
        "dream_luau_ExprInstantiate",
        "{ kind: \"ExprInstantiate\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr, typeArguments: { dream_luau_Type | dream_luau_TypePack } }",
    ),
    (
        "dream_luau_ExprError",
        "{ kind: \"ExprError\", line: number, column: number, endLine: number, endColumn: number, expressions: { dream_luau_Expr }, messageIndex: number, isMissing: boolean? }",
    ),
    (
        "dream_luau_StatBlock",
        "{ kind: \"StatBlock\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, body: { dream_luau_Stat }, hasEnd: boolean }",
    ),
    (
        "dream_luau_StatIf",
        "{ kind: \"StatIf\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, condition: dream_luau_Expr, thenbody: dream_luau_StatBlock, elsebody: (dream_luau_StatBlock | dream_luau_StatIf)?, thenLocation: dream_luau_Span?, elseLocation: dream_luau_Span?, conditionLocal: dream_luau_Local? }",
    ),
    (
        "dream_luau_StatWhile",
        "{ kind: \"StatWhile\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, condition: dream_luau_Expr, body: dream_luau_StatBlock, hasDo: boolean }",
    ),
    (
        "dream_luau_StatRepeat",
        "{ kind: \"StatRepeat\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, condition: dream_luau_Expr, body: dream_luau_StatBlock }",
    ),
    (
        "dream_luau_StatBreak",
        "{ kind: \"StatBreak\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean }",
    ),
    (
        "dream_luau_StatContinue",
        "{ kind: \"StatContinue\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean }",
    ),
    (
        "dream_luau_StatReturn",
        "{ kind: \"StatReturn\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, list: { dream_luau_Expr } }",
    ),
    (
        "dream_luau_StatExpr",
        "{ kind: \"StatExpr\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, expr: dream_luau_Expr }",
    ),
    (
        "dream_luau_StatLocal",
        "{ kind: \"StatLocal\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, vars: { dream_luau_Local }, values: { dream_luau_Expr }, isConst: boolean, isExported: boolean, equalsSignLocation: dream_luau_Span? }",
    ),
    (
        "dream_luau_StatFor",
        "{ kind: \"StatFor\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, var: dream_luau_Local, from: dream_luau_Expr, to: dream_luau_Expr, step: dream_luau_Expr?, body: dream_luau_StatBlock, hasDo: boolean }",
    ),
    (
        "dream_luau_StatForIn",
        "{ kind: \"StatForIn\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, vars: { dream_luau_Local }, values: { dream_luau_Expr }, body: dream_luau_StatBlock, hasDo: boolean }",
    ),
    (
        "dream_luau_StatAssign",
        "{ kind: \"StatAssign\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, vars: { dream_luau_Expr }, values: { dream_luau_Expr } }",
    ),
    (
        "dream_luau_StatCompoundAssign",
        "{ kind: \"StatCompoundAssign\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, op: dream_luau_BinaryOp, var: dream_luau_Expr, value: dream_luau_Expr }",
    ),
    (
        "dream_luau_StatFunction",
        "{ kind: \"StatFunction\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: dream_luau_Expr, func: dream_luau_ExprFunction }",
    ),
    (
        "dream_luau_StatLocalFunction",
        "{ kind: \"StatLocalFunction\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: dream_luau_Local, func: dream_luau_ExprFunction, isConst: boolean }",
    ),
    (
        "dream_luau_StatTypeAlias",
        "{ kind: \"StatTypeAlias\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: string, nameLocation: dream_luau_Span, generics: { dream_luau_GenericType }, genericPacks: { dream_luau_GenericTypePack }, type: dream_luau_Type, exported: boolean }",
    ),
    (
        "dream_luau_StatTypeFunction",
        "{ kind: \"StatTypeFunction\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: string, nameLocation: dream_luau_Span, body: dream_luau_ExprFunction, exported: boolean }",
    ),
    (
        "dream_luau_StatDeclareGlobal",
        "{ kind: \"StatDeclareGlobal\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: string, nameLocation: dream_luau_Span, type: dream_luau_Type }",
    ),
    (
        "dream_luau_StatDeclareFunction",
        "{ kind: \"StatDeclareFunction\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, attributes: { dream_luau_Attr }, name: string, nameLocation: dream_luau_Span, generics: { dream_luau_GenericType }, genericPacks: { dream_luau_GenericTypePack }, params: dream_luau_TypeList, paramNames: { dream_luau_ArgumentName }, vararg: boolean, retTypes: dream_luau_TypePack }",
    ),
    (
        "dream_luau_StatDeclareExternType",
        "{ kind: \"StatDeclareExternType\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: string, superName: string?, props: { dream_luau_DeclaredProp }, indexer: dream_luau_TableIndexer? }",
    ),
    (
        "dream_luau_StatClass",
        "{ kind: \"StatClass\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, name: dream_luau_Local, super: dream_luau_Expr?, exported: boolean, open: boolean }",
    ),
    (
        "dream_luau_StatError",
        "{ kind: \"StatError\", line: number, column: number, endLine: number, endColumn: number, hasSemicolon: boolean, expressions: { dream_luau_Expr }, statements: { dream_luau_Stat }, messageIndex: number }",
    ),
    (
        "dream_luau_TypeReference",
        "{ kind: \"TypeReference\", line: number, column: number, endLine: number, endColumn: number, prefix: string?, name: string, nameLocation: dream_luau_Span, hasParameterList: boolean, parameters: { dream_luau_Type | dream_luau_TypePack } }",
    ),
    (
        "dream_luau_TypeTable",
        "{ kind: \"TypeTable\", line: number, column: number, endLine: number, endColumn: number, props: { dream_luau_TableProp }, indexer: dream_luau_TableIndexer?, isExact: boolean }",
    ),
    (
        "dream_luau_TypeFunction",
        "{ kind: \"TypeFunction\", line: number, column: number, endLine: number, endColumn: number, attributes: { dream_luau_Attr }, generics: { dream_luau_GenericType }, genericPacks: { dream_luau_GenericTypePack }, argTypes: dream_luau_TypeList, argNames: { dream_luau_ArgumentName | false }, returnTypes: dream_luau_TypePack }",
    ),
    (
        "dream_luau_TypeTypeof",
        "{ kind: \"TypeTypeof\", line: number, column: number, endLine: number, endColumn: number, expr: dream_luau_Expr }",
    ),
    (
        "dream_luau_TypeOptional",
        "{ kind: \"TypeOptional\", line: number, column: number, endLine: number, endColumn: number }",
    ),
    (
        "dream_luau_TypeUnion",
        "{ kind: \"TypeUnion\", line: number, column: number, endLine: number, endColumn: number, types: { dream_luau_Type } }",
    ),
    (
        "dream_luau_TypeIntersection",
        "{ kind: \"TypeIntersection\", line: number, column: number, endLine: number, endColumn: number, types: { dream_luau_Type } }",
    ),
    (
        "dream_luau_TypeSingletonBool",
        "{ kind: \"TypeSingletonBool\", line: number, column: number, endLine: number, endColumn: number, value: boolean }",
    ),
    (
        "dream_luau_TypeSingletonString",
        "{ kind: \"TypeSingletonString\", line: number, column: number, endLine: number, endColumn: number, value: string }",
    ),
    (
        "dream_luau_TypeGroup",
        "{ kind: \"TypeGroup\", line: number, column: number, endLine: number, endColumn: number, type: dream_luau_Type }",
    ),
    (
        "dream_luau_TypeError",
        "{ kind: \"TypeError\", line: number, column: number, endLine: number, endColumn: number, types: { dream_luau_Type }, isMissing: boolean, messageIndex: number }",
    ),
    (
        "dream_luau_TypePackExplicit",
        "{ kind: \"TypePackExplicit\", line: number, column: number, endLine: number, endColumn: number, typeList: dream_luau_TypeList }",
    ),
    (
        "dream_luau_TypePackVariadic",
        "{ kind: \"TypePackVariadic\", line: number, column: number, endLine: number, endColumn: number, variadicType: dream_luau_Type }",
    ),
    (
        "dream_luau_TypePackGeneric",
        "{ kind: \"TypePackGeneric\", line: number, column: number, endLine: number, endColumn: number, genericName: string }",
    ),
    (
        "dream_luau_Expr",
        "dream_luau_ExprGroup | dream_luau_ExprConstantNil | dream_luau_ExprConstantBool | dream_luau_ExprConstantNumber | dream_luau_ExprConstantInteger | dream_luau_ExprConstantString | dream_luau_ExprLocal | dream_luau_ExprGlobal | dream_luau_ExprVarargs | dream_luau_ExprCall | dream_luau_ExprIndexName | dream_luau_ExprIndexExpr | dream_luau_ExprFunction | dream_luau_ExprTable | dream_luau_ExprUnary | dream_luau_ExprBinary | dream_luau_ExprTypeAssertion | dream_luau_ExprIfElse | dream_luau_ExprInterpString | dream_luau_ExprInstantiate | dream_luau_ExprError | dream_luau_ExprRange | dream_luau_ExprComprehension | dream_luau_ExprReduction",
    ),
    (
        "dream_luau_Stat",
        "dream_luau_StatBlock | dream_luau_StatIf | dream_luau_StatWhile | dream_luau_StatRepeat | dream_luau_StatBreak | dream_luau_StatContinue | dream_luau_StatReturn | dream_luau_StatExpr | dream_luau_StatLocal | dream_luau_StatFor | dream_luau_StatForIn | dream_luau_StatAssign | dream_luau_StatCompoundAssign | dream_luau_StatFunction | dream_luau_StatLocalFunction | dream_luau_StatTypeAlias | dream_luau_StatTypeFunction | dream_luau_StatDeclareGlobal | dream_luau_StatDeclareFunction | dream_luau_StatDeclareExternType | dream_luau_StatClass | dream_luau_StatError",
    ),
    (
        "dream_luau_Type",
        "dream_luau_TypeReference | dream_luau_TypeTable | dream_luau_TypeFunction | dream_luau_TypeTypeof | dream_luau_TypeOptional | dream_luau_TypeUnion | dream_luau_TypeIntersection | dream_luau_TypeSingletonBool | dream_luau_TypeSingletonString | dream_luau_TypeGroup | dream_luau_TypeError",
    ),
    ("dream_luau_TypePack", "dream_luau_TypePackExplicit | dream_luau_TypePackVariadic | dream_luau_TypePackGeneric"),
    (
        "dream_luau_Node",
        "dream_luau_Expr | dream_luau_Stat | dream_luau_Type | dream_luau_TypePack | dream_luau_Local | dream_luau_Attr | dream_luau_GenericType | dream_luau_GenericTypePack | dream_luau_TableItem | dream_luau_TableProp | dream_luau_TableIndexer | dream_luau_DeclaredProp | dream_luau_ArgumentName | dream_luau_ComprehensionClause",
    ),
    (
        "dream_luau_Comment",
        "{ kind: \"line\" | \"block\" | \"broken\", line: number, column: number, endLine: number, endColumn: number }",
    ),
    (
        "dream_luau_HotComment",
        "{ kind: \"HotComment\", line: number, column: number, endLine: number, endColumn: number, header: boolean, content: string }",
    ),
    (
        "dream_luau_ParseError",
        "{ kind: \"Error\", line: number, column: number, endLine: number, endColumn: number, message: string }",
    ),
    ("dream_luau_ParseOptions", "{ declarations: boolean?, tokens: boolean?, dialect: (\"l3i\" | \"luau\")? }"),
    (
        "dream_luau_ParseResult",
        "{ root: dream_luau_StatBlock, errors: { dream_luau_ParseError }, comments: { dream_luau_Comment }, hotComments: { dream_luau_HotComment }, lineStarts: { number }, tokens: buffer? }",
    ),
    (
        "dream_luau_TokenKinds",
        "{ name: number, keyword: number, number: number, string: number, longString: number, interpolatedBegin: number, interpolatedMid: number, interpolatedEnd: number, interpolatedSimple: number, comment: number, blockComment: number, attribute: number, symbol: number, error: number }",
    ),
];

/// The `dream.luau` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyntaxExtension;

impl Extension for SyntaxExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        for (name, definition) in TYPES {
            d.type_alias(name, *definition);
        }
        d.module(MODULE)
            .doc("Source syntax trees, comments, errors and original tokens as tables; l3i and stock luau dialects.")
            .function("parse", parse)
            .signature("(source: string | buffer, options: dream_luau_ParseOptions?) -> dream_luau_ParseResult")
            .doc("Parses source (dialect defaults to l3i; luau selects strict stock syntax). Comprehensions and intrinsic sum reductions remain source nodes, including recovery holes. declarations allows definition-file syntax; tokens adds original 12-byte token records. Syntax errors are data; invalid options raise.")
            .installed("tokenKinds")
            .signature("dream_luau_TokenKinds")
            .doc("The token kinds in parse's tokens buffer, by name.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        // The root stack is released before the installer opens its own.
        let kinds = {
            let stack = cx.runtime().stack();
            let kinds = Table::new(&stack, 0, TOKEN_KINDS.len())?;
            for (index, name) in TOKEN_KINDS.iter().enumerate() {
                kinds.set(&stack, name, &((index + 1) as f64))?;
            }
            stack.with_frame(|frame| {
                let view = kinds.push_to(frame)?;
                // SAFETY: the table is on the frame; read-only is a flag on it.
                unsafe { crate::raw::ffi::lua_setreadonly(frame.state(), view.index(), 1) };
                Ok(())
            })?;
            kinds
        };
        cx.module(MODULE)?.set("tokenKinds", &kinds)?;
        Ok(())
    }
}
