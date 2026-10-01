//! `@dream/intern` through Luau: equivalent spellings are one token whether they arrive as
//! strings or buffer spans, tokens are dense numbers that key tables, resolve gives the first
//! spelling back, and bad input fails with the call's name.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::intern::InternExtension;

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/intern", "intern"))
        .extension(InternExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn spellings_and_spans_are_one_integer_identity() {
    runtime()
        .exec(
            "local ids = intern.new('ascii-nocase') \
             local id = ids:intern('cAiUs CoSaDeS') \
             assert(typeof(id) == 'number' and id == 1, typeof(id)) \
             assert(id == ids:intern('Caius Cosades') and id == ids:intern('CAIUS COSADES'), 'case folds') \
             local record = buffer.fromstring('NAME\\0caius cosades\\0FLAG') \
             assert(ids:intern(record, 5, 13) == id, 'buffer span') \
             assert(ids:intern('xx CAIUS COSADES xx', 3, 13) == id, 'string span') \
             assert(ids:resolve(id) == 'cAiUs CoSaDeS', 'first spelling kept') \
             assert(ids:find('fargoth') == nil and ids:count() == 1, 'find never adds') \
             local fargoth = ids:intern('Fargoth') \
             assert(fargoth ~= id and ids:find('FARGOTH') == fargoth and ids:count() == 2, 'distinct') \
             local records = {} records[id] = 'npc' \
             assert(records[ids:intern('caius cosades')] == 'npc' and #records == 1, 'dense keys, the array part') \
             assert(ids:policy() == 'ascii-nocase' and ids:memory() > 0, 'introspection') \
             local internId = ids:interner() \
             assert(internId('CAIUS COSADES') == id and internId(record, 5, 13) == id and internId('Fargoth') == fargoth, 'the bound form is the method') \
             assert(internId('Vivec') == 3 and ids:count() == 3, 'and adds to the same pool') \
             local exact = intern.new() \
             assert(exact:policy() == 'exact' and exact:intern('A') ~= exact:intern('a'), 'exact by default') \
             assert(exact:intern('A') == 1 and exact:intern('a') == 2, 'tokens are pool-relative and dense') \
             assert(exact:resolve(2) == 'a' and exact:resolve(2i) == 'a', 'resolve takes a number or an integer')",
        )
        .unwrap();
}

#[test]
fn a_bound_interner_keeps_its_pool_across_a_collection() {
    let runtime = runtime();
    runtime.exec("orphan = intern.new():interner() assert(orphan('x') == 1)").unwrap();
    runtime.gc(l3i::memory::GcControl::Collect);
    runtime.exec("assert(orphan('x') == 1 and orphan('y') == 2)").unwrap();
}

#[test]
fn bad_input_names_the_call() {
    runtime()
        .exec(
            "local ids = intern.new('exact') \
             local ok, err = pcall(intern.new, 'unicode') assert(not ok and err:find('unknown policy'), err) \
             ok, err = pcall(ids.intern, ids, 'abc', 2, 5) assert(not ok and err:find('Pool:intern: 5 bytes at offset 2'), err) \
             ok, err = pcall(ids.intern, ids, 'abc', -1) assert(not ok and err:find('negative offset'), err) \
             ok, err = pcall(ids.resolve, ids, 1) assert(not ok and err:find('not an identity'), err) \
             ok, err = pcall(ids.resolve, ids, 1.5) assert(not ok, 'a fraction is not a token') \
             ok, err = pcall(ids.intern, ids, 42) assert(not ok, 'not bytes')",
        )
        .unwrap();
}
