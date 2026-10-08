//! `@dream/intl` through Luau: locales parse to their canonical spelling, compare by it, give
//! their subtags back, and a malformed tag fails with the call's name and the reason.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::intl::IntlExtension;

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/intl", "intl"))
        .extension(IntlExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn locales_are_their_canonical_spelling() {
    runtime()
        .exec(
            "local pt = intl.locale('pt_br') \
             assert(pt:tag() == 'pt-BR' and tostring(pt) == 'pt-BR', pt:tag()) \
             assert(pt:language() == 'pt' and pt:region() == 'BR' and pt:script() == nil) \
             assert(#pt:variants() == 0 and pt:baseName() == 'pt-BR') \
             assert(pt == intl.locale('PT-br') and pt ~= intl.locale('pt'), 'equal by canonical spelling') \
             assert(typeof(pt) == 'dream.intl.Locale') \
             local zh = intl.locale('ZH_hant_tw') \
             assert(zh:tag() == 'zh-Hant-TW' and zh:script() == 'Hant' and zh:region() == 'TW') \
             local sl = intl.locale('sl-rozaj-BISKE-1994') \
             local variants = sl:variants() \
             assert(sl:tag() == 'sl-1994-biske-rozaj' and #variants == 3, sl:tag()) \
             assert(variants[1] == '1994' and variants[2] == 'biske' and variants[3] == 'rozaj') \
             local de = intl.locale('DE-de-1996-u-NU-latn-CA-gregory') \
             assert(de:tag() == 'de-DE-1996-u-ca-gregory-nu-latn', de:tag()) \
             assert(de:baseName() == 'de-DE-1996' and de:language() == 'de') \
             assert(intl.locale('und'):language() == 'und') \
             assert(intl.canonicalize('ZH-hant-tw') == 'zh-Hant-TW') \
             assert(intl.canonicalize('en_us') == 'en-US') \
             assert(intl.canonicalize('en-X-Private') == 'en-x-private') \
             local cache = {} cache[intl.canonicalize('pt-br')] = 1 \
             assert(cache[pt:tag()] == 1, 'the canonical tag keys a table')",
        )
        .unwrap();
}

#[test]
fn malformed_tags_name_the_call_and_the_reason() {
    runtime()
        .exec(
            "for _, tag in { '', 'en-US-', 'en--US', 'x2', 'english', 'root', 'i-klingon', ' en', 'en US' } do \
               local ok, err = pcall(intl.locale, tag) \
               assert(not ok and err:find('intl.locale: ', 1, true), tostring(err)) \
               ok, err = pcall(intl.canonicalize, tag) \
               assert(not ok and err:find('intl.canonicalize: ', 1, true), tostring(err)) \
             end \
             local ok, err = pcall(intl.locale, 'english') \
             assert(err:find(\"'english' is not a BCP 47 language tag: the given language subtag is invalid\", 1, true), err) \
             ok, err = pcall(intl.locale, '') \
             assert(err:find('an empty string is not a language tag', 1, true), err) \
             ok, err = pcall(intl.locale, 42) \
             assert(not ok, 'numbers are not tags') \
             ok, err = pcall(intl.locale) \
             assert(not ok, 'a tag is required')",
        )
        .unwrap();
}
