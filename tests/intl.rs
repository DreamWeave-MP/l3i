//! `@dream/intl` through Luau: locales parse to their canonical spelling, compare by it, give
//! their subtags back, and a malformed tag fails with the call's name and the reason; plural
//! rules take numbers, integers and decimal strings, keep visible fraction digits, and refuse
//! what is not a finite number.

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

#[test]
fn plural_rules_select_cldr_categories() {
    runtime()
        .exec(
            "local en = intl.pluralRules('en') \
             assert(en:type() == 'cardinal' and en:locale() == 'en') \
             assert(en:category(1) == 'one' and en:category(2) == 'other' and en:category(0) == 'other') \
             assert(en:category(1i) == 'one' and en:category(-1) == 'one' and en:category(1.0) == 'one') \
             assert(en:category('1') == 'one' and en:category('1.0') == 'other' and en:category('1.00') == 'other') \
             local ordinal = intl.pluralRules(intl.locale('EN'), 'ordinal') \
             assert(ordinal:type() == 'ordinal') \
             local suffix = { one = 'st', two = 'nd', few = 'rd', other = 'th' } \
             local out = {} \
             for _, n in { 1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 101, 111 } do table.insert(out, n .. suffix[ordinal:category(n)]) end \
             assert(table.concat(out, ' ') == '1st 2nd 3rd 4th 11th 12th 13th 21st 22nd 23rd 101st 111th', table.concat(out, ' ')) \
             local pl = intl.pluralRules(intl.locale('pl-PL'), 'cardinal') \
             assert(pl:locale() == 'pl-PL') \
             for n, want in { [1] = 'one', [2] = 'few', [4] = 'few', [5] = 'many', [12] = 'many', [22] = 'few', [25] = 'many' } do \
               assert(pl:category(n) == want, n) \
             end \
             assert(pl:category(1.5) == 'other' and pl:category('1.00') == 'other') \
             local cats = pl:categories() \
             assert(table.concat(cats, ',') == 'one,few,many,other', table.concat(cats, ',')) \
             local ru = intl.pluralRules('ru') \
             assert(ru:category(1) == 'one' and ru:category(3) == 'few' and ru:category(11) == 'many' and ru:category(21) == 'one') \
             local ar = intl.pluralRules('ar') \
             local seen = {} \
             for _, n in { 0, 1, 2, 3, 11, 100 } do table.insert(seen, ar:category(n)) end \
             assert(table.concat(seen, ',') == 'zero,one,two,few,many,other', table.concat(seen, ',')) \
             assert(#ar:categories() == 6) \
             local fr = intl.pluralRules('fr') \
             assert(fr:category(1000000) == 'many' and fr:category('1000000') == 'many' and fr:category('1000000.0') == 'other') \
             assert(pl:category(9223372036854775807i) == 'many' and pl:category(2^53) == 'few' and pl:category(1e300) == 'many') \
             assert(pl:category('12345678901234567892') == 'few')",
        )
        .unwrap();
}

#[test]
fn plural_rules_refuse_what_is_not_a_number() {
    runtime()
        .exec(
            "local en = intl.pluralRules('en') \
             for _, bad in { 0/0, math.huge, -math.huge } do \
               local ok, err = pcall(en.category, en, bad) \
               assert(not ok and err:find('PluralRules:category: ', 1, true) and err:find('is not a finite number', 1, true), tostring(err)) \
             end \
             for _, bad in { '', 'abc', '1,5', ' 1', '5.', '0x10' } do \
               local ok, err = pcall(en.category, en, bad) \
               assert(not ok and err:find('is not a decimal number', 1, true), tostring(err)) \
             end \
             local ok, err = pcall(en.category, en, true) \
             assert(not ok and err:find('number or decimal string', 1, true), tostring(err)) \
             ok, err = pcall(en.category, en) \
             assert(not ok, 'a value is required') \
             ok, err = pcall(intl.pluralRules, 'en', 'cardinals') \
             assert(not ok and err:find(\"intl.pluralRules: unknown type 'cardinals' (cardinal or ordinal)\", 1, true), err) \
             ok, err = pcall(intl.pluralRules, 'english') \
             assert(not ok and err:find('intl.pluralRules: ', 1, true) and err:find('not a BCP 47 language tag', 1, true), err) \
             ok, err = pcall(intl.pluralRules, 7) \
             assert(not ok and err:find('a language tag or a dream.intl.Locale', 1, true), err) \
             local unknown = intl.pluralRules('qaa') \
             assert(unknown:category(1) == 'other' and #unknown:categories() == 1, 'root rules')",
        )
        .unwrap();
}
