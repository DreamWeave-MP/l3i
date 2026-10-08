//! `@dream/intl` through Luau: locales parse to their canonical spelling, compare by it, give
//! their subtags back, and a malformed tag fails with the call's name and the reason; plural
//! rules take numbers, integers and decimal strings, keep visible fraction digits, and refuse
//! what is not a finite number; decimal formatters write each locale's digits and separators
//! under the grouping and fraction digit options, and refuse bad options by name.

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

#[test]
fn decimal_formatters_follow_the_locale_and_options() {
    runtime()
        .exec(
            "local v = 1234567.891 \
             assert(intl.decimalFormatter('en'):format(v) == '1,234,567.891') \
             assert(intl.decimalFormatter('de'):format(v) == '1.234.567,891') \
             local fr = intl.decimalFormatter('fr', { grouping = 'auto', minFractionDigits = 2, maxFractionDigits = 2 }) \
             assert(fr:format('1234567.895') == '1\\u{202f}234\\u{202f}567,90', fr:format('1234567.895')) \
             assert(fr:format(v) == '1\\u{202f}234\\u{202f}567,89' and fr:locale() == 'fr') \
             assert(intl.decimalFormatter(intl.locale('ar-EG')):format(123) == '١٢٣') \
             assert(intl.decimalFormatter('ar-EG-u-nu-latn'):format(123) == '123') \
             local en = intl.decimalFormatter('en') \
             assert(en:format(1234i) == '1,234' and en:format(-1234.5) == '-1,234.5' and en:format('-0') == '-0') \
             assert(en:format(-0.0) == '-0' and en:format(0) == '0' and en:format(0.1) == '0.1') \
             assert(en:format(9223372036854775807i) == '9,223,372,036,854,775,807') \
             assert(en:format(1e21) == '1,000,000,000,000,000,000,000') \
             assert(en:format(0.123456789) == '0.123' and en:format('1.50') == '1.5') \
             local never = intl.decimalFormatter('en', { grouping = 'never' }) \
             local min2 = intl.decimalFormatter('en', { grouping = 'min2' }) \
             assert(never:format(1234567) == '1234567' and min2:format(1234) == '1234' and min2:format(12345) == '12,345') \
             local cents = intl.decimalFormatter('en', { minFractionDigits = 2, maxFractionDigits = 2 }) \
             assert(cents:format(2) == '2.00' and cents:format('0.125') == '0.12' and cents:format('0.135') == '0.14') \
             local whole = intl.decimalFormatter('en', { maxFractionDigits = 0 }) \
             assert(whole:format(2.5) == '2' and whole:format(3.5) == '4' and whole:format(-2.5) == '-2', 'half to even') \
             local options = intl.decimalFormatter('pl', { minFractionDigits = 5 }):resolvedOptions() \
             assert(options.locale == 'pl' and options.grouping == 'auto', options.grouping) \
             assert(options.minFractionDigits == 5 and options.maxFractionDigits == 5) \
             options = en:resolvedOptions() \
             assert(options.minFractionDigits == 0 and options.maxFractionDigits == 3) \
             local pl = intl.decimalFormatter('pl') \
             assert(pl:format(1234) == '1234' and pl:format(12345) == '12\\u{a0}345') \
             local plural = intl.pluralRules('pl') \
             local forms = { one = 'plik', few = 'pliki', many = 'plików', other = 'pliku' } \
             assert(pl:format(1.5) .. ' ' .. forms[plural:category(1.5)] == '1,5 pliku') \
             assert(pl:format(22) .. ' ' .. forms[plural:category(22)] == '22 pliki')",
        )
        .unwrap();
}

#[test]
fn decimal_formatters_refuse_bad_options_and_numbers() {
    runtime()
        .exec(
            "local function fails(pattern, f, ...) \
               local ok, err = pcall(f, ...) \
               assert(not ok and err:find(pattern, 1, true), tostring(err)) \
             end \
             local new = intl.decimalFormatter \
             fails(\"intl.decimalFormatter: unknown grouping 'always' (auto, never or min2)\", new, 'en', { grouping = 'always' }) \
             fails('minFractionDigits must be a whole number from 0 to 100, got 101', new, 'en', { minFractionDigits = 101 }) \
             fails('maxFractionDigits must be a whole number from 0 to 100, got -1', new, 'en', { maxFractionDigits = -1 }) \
             fails('minFractionDigits (3) is more than maxFractionDigits (2)', new, 'en', { minFractionDigits = 3, maxFractionDigits = 2 }) \
             fails('minFractionDigits', new, 'en', { minFractionDigits = 1.5 }) \
             fails('maximumFractionDigits', new, 'en', { maximumFractionDigits = 2 }) \
             fails('options must be a table', new, 'en', 'auto') \
             fails('not a BCP 47 language tag', new, 'en-') \
             local en = new('en') \
             fails('DecimalFormatter:format: NaN is not a finite number', en.format, en, 0/0) \
             fails('DecimalFormatter:format: inf is not a finite number', en.format, en, math.huge) \
             fails(\"DecimalFormatter:format: '1,5' is not a decimal number\", en.format, en, '1,5') \
             fails('number or decimal string', en.format, en, {}) \
             fails('has more digits than a decimal holds', en.format, en, string.rep('9', 40000)) \
             assert(new('en', nil):format(1) == '1' and new('en', {}):format(1) == '1')",
        )
        .unwrap();
}
