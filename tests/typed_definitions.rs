//! The plan's generated `.d.luau` is checked by Luau's own frontend, and strict scripts that
//! `require` every built-in module by its canonical path type check against the plan's stubs:
//! the declared API and the runtime agree, with no compatibility global in sight.

use std::collections::HashMap;
use std::rc::Rc;

use l3i::analysis::{Analysis, AnalysisOptions, Definitions, Mode, ModuleConfig, SourceCode, SourceProvider};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;
use l3i::soft_render::SoftRenderExtension;

struct Scripts(HashMap<&'static str, &'static str>);

impl SourceProvider for Scripts {
    fn read_source(&self, name: &str) -> Option<SourceCode> {
        self.0.get(name).map(|text| SourceCode { text: (*text).to_owned(), is_script: true })
    }
    fn resolve_module(&self, _requirer: &str, _required: &str) -> Option<String> {
        None
    }
    fn module_config(&self, _name: &str) -> ModuleConfig {
        ModuleConfig { mode: Mode::Strict, ..ModuleConfig::default() }
    }
    fn human_name(&self, name: &str) -> Option<String> {
        Some(format!("{name}.luau"))
    }
}

fn plan() -> Rc<RuntimePlan> {
    let builder = RuntimePlan::builder()
        .policy(RuntimePolicy::new())
        .extension(QuatExtension)
        .extension(RasterExtension)
        .extension(SoftRenderExtension);
    #[cfg(feature = "bytes")]
    let builder = builder.extension(l3i::bytes::BytesExtension);
    #[cfg(feature = "intern")]
    let builder = builder.extension(l3i::intern::InternExtension);
    #[cfg(feature = "intl")]
    let builder = builder.extension(l3i::intl::IntlExtension);
    #[cfg(feature = "syntax")]
    let builder = builder.extension(l3i::syntax::SyntaxExtension);
    #[cfg(feature = "fs")]
    let builder = builder.extension(l3i::fs::FsExtension);
    #[cfg(feature = "process")]
    let builder = builder.extension(l3i::process::ProcessExtension);
    #[cfg(feature = "tcp")]
    let builder = builder.extension(l3i::tcp::TcpExtension);
    #[cfg(feature = "dns")]
    let builder = builder.extension(l3i::dns::DnsExtension::default());
    builder.finalize().unwrap()
}

/// Every feature's members are declared, so the bytes script only needs the core to exist.
#[cfg(feature = "bytes")]
const BYTES_SCRIPT: (&str, &str) = (
    "bytes_script",
    "--!strict\n\
     local bytes = require('@dream/bytes')\n\
     local buf = buffer.create(32)\n\
     local at: number? = bytes.find(buf, 'x', 0)\n\
     local same: boolean = bytes.equals(buf, buffer.tostring(buf))\n\
     local piece: buffer = bytes.slice(buf, 0, 4)\n\
     local text: string, after: number = bytes.readCString('abc', 0)\n\
     local next: number = bytes.writeCString(buf, 0, text, 8)\n\
     local value: integer, after2: number = bytes.readVarint(buf, 0)\n\
     local written: number = bytes.writeVarint(buf, 8, value)\n\
     local n: number = bytes.readu32be(buf, 0) + bytes.readf16(buf, 4) + bytes.readi24be('abc', 0)\n\
     bytes.writef64be(buf, 8, n)\n\
     local big: integer = bytes.readi64be(buf, 8)\n\
     local B = bytes.math()\n\
     local m: number = B:readu16be(buf, 0) + B:readf32be(buf, 4)\n\
     B:writei64be(buf, 16, big)\n\
     local hex: string = bytes.toHex(piece)\n\
     print(at, same, after, next, after2, written, m, hex)\n",
);

#[cfg(feature = "intern")]
const INTERN_SCRIPT: (&str, &str) = (
    "intern_script",
    "--!strict\n\
     local intern = require('@dream/intern')\n\
     local ids = intern.new('ascii-nocase')\n\
     local id: number = ids:intern('Caius Cosades')\n\
     local span: number = ids:intern(buffer.fromstring('xx'), 0, 2)\n\
     local found: number? = ids:find('caius cosades')\n\
     local text: string = ids:resolve(id)\n\
     local internId = ids:interner()\n\
     local again: number = internId('CAIUS COSADES', 0, 13)\n\
     local n: number = ids:count() + ids:memory()\n\
     local policy: string = ids:policy()\n\
     print(span, found, text, again, n, policy)\n",
);

#[cfg(feature = "intl")]
const INTL_SCRIPT: (&str, &str) = (
    "intl_script",
    "--!strict\n\
     local intl = require('@dream/intl')\n\
     local locale = intl.locale('pt-br')\n\
     local tag: string = locale:tag()\n\
     local base: string = locale:baseName()\n\
     local language: string = locale:language()\n\
     local script: string?, region: string? = locale:script(), locale:region()\n\
     local variants: { string } = locale:variants()\n\
     local canonical: string = intl.canonicalize('zh_hant')\n\
     local rules = intl.pluralRules(locale, 'ordinal')\n\
     local category: dream_intl_PluralCategory = rules:category(21)\n\
     local exact: string = rules:category('1.00')\n\
     local whole: string = intl.pluralRules('pl'):category(5i)\n\
     local categories: { dream_intl_PluralCategory } = rules:categories()\n\
     local kind: dream_intl_PluralType = rules:type()\n\
     local rulesLocale: string = rules:locale()\n\
     local formatter = intl.decimalFormatter('fr', { grouping = 'min2', minFractionDigits = 2, maxFractionDigits = 2 })\n\
     local text: string = formatter:format('1234567.895') .. formatter:format(2) .. formatter:format(3i)\n\
     local options = formatter:resolvedOptions()\n\
     local grouping: dream_intl_Grouping = options.grouping\n\
     local digits: number = options.minFractionDigits + options.maxFractionDigits\n\
     local formatterLocale: string = formatter:locale() .. options.locale\n\
     local plain = intl.decimalFormatter(locale)\n\
     print(tag, base, language, script, region, #variants, canonical, category, exact, whole, #categories, kind, rulesLocale)\n\
     print(text, grouping, digits, formatterLocale, plain:format(1))\n",
);

#[cfg(feature = "fs")]
const FS_SCRIPT: (&str, &str) = (
    "fs_script",
    "--!strict\n\
     local fs = require('@dream/fs')\n\
     local reader, message, failure = fs.open('x')\n\
     if not reader then\n\
       local kind: dream_fs_ErrorKind? = failure\n\
       error(`{message} ({kind})`)\n\
     end\n\
     local head = reader:readAt(0, 4) or buffer.create(0)\n\
     local count: number = (reader:readInto(buffer.create(4)) or 0) + (reader:readAtInto(head, 0) or 0) + reader:size() + reader:tell()\n\
     reader:seek(0)\n\
     reader:close()\n\
     local stat: dream_fs_Stat? = fs.lstat('x')\n\
     local kind: dream_fs_FileKind = if stat then stat.kind else 'other'\n\
     local walk = fs.walk('.', { followLinks = true, include = 'files', metadata = true, sort = true, maxDepth = 3 })\n\
     if walk then\n\
       for index, path in walk.paths do\n\
         print(path, walk.kinds[index], walk.sizes and walk.sizes[index])\n\
       end\n\
     end\n\
     local writer, refused = fs.openWrite('y', { append = true })\n\
     if not writer then error(refused) end\n\
     local written: number = (writer:write('abc') or 0) + (fs.writeFile('z', buffer.create(1), { offset = 0 }) or 0)\n\
     local closed: boolean? = writer:close()\n\
     local made: boolean? = fs.mkdir('d', { recursive = true })\n\
     fs.hardLink('x', 'h')\n\
     fs.symlink('x', 's', { directory = false })\n\
     local same: boolean = fs.sameFile('x', 'h') == true and fs.exists('s') == true\n\
     local target: string? = fs.readLink('s')\n\
     local names: { string } = fs.list('.') or {}\n\
     fs.remove('d', { recursive = true })\n\
     print(count, kind, written, closed, made, same, target, names, fs.canonicalize('.'), fs.copy('x', 'w'))\n",
);

#[cfg(feature = "process")]
const PROCESS_SCRIPT: (&str, &str) = (
    "process_script",
    "--!strict\n\
     local process = require('@dream/process')\n\
     local result, message = process.run('tool', { '--help' }, { cwd = '.', env = { A = 'b' }, stdout = 'capture', stdin = 'x' })\n\
     if not result then error(message) end\n\
     local code: number? = result.code\n\
     local output: string = result.stdout or ''\n\
     local home: string? = process.env('HOME')\n\
     local wrote: boolean? = process.write('stderr', 'x')\n\
     print(result.success, code, output, result.signal, home, wrote, process.isTerminal('stdout'))\n",
);

#[cfg(feature = "tcp")]
const TCP_SCRIPT: (&str, &str) = (
    "tcp_script",
    "--!strict\n\
     local tcp = require('@dream/tcp')\n\
     local listener, message, kind = tcp.listen('127.0.0.1:0', { backlog = 16, noDelay = true, maxStreams = 8 })\n\
     if not listener then error(message) end\n\
     local stream: dream_tcp_Stream? = tcp.connect(listener.localAddress, { noDelay = true })\n\
     local poller = tcp.poller({ maxEvents = 64, maxWatches = 16, maxWaitMs = 250 })\n\
     local watched: boolean? = poller:watch(listener, 1, 'read')\n\
     local count: number? = poller:wait(0)\n\
     local token: number?, readable: boolean, writable: boolean, closed: boolean = poller:next()\n\
     local accepted, peer, failure = listener:accept()\n\
     if accepted then\n\
         local read: number? = accepted:readInto(buffer.create(16), 0, 16)\n\
         local wrote: number? = accepted:write('x', 0, 1)\n\
         local half: boolean? = accepted:shutdown('write')\n\
         local state: dream_tcp_StreamState = accepted.state\n\
         local local_: string? = accepted.localAddress\n\
         print(read, wrote, half, state, accepted.peerAddress, local_, accepted.closed)\n\
         accepted:close()\n\
     end\n\
     if stream then\n\
         local done: boolean? = stream:finishConnect()\n\
         print(done)\n\
     end\n\
     poller:modify(1, 'read')\n\
     poller:unwatch(1)\n\
     poller:close()\n\
     local why: dream_tcp_ErrorKind? = failure\n\
     print(kind, watched, count, token, readable, writable, closed, peer, why, listener.streams, listener.closed, poller.watching, tcp.MAX_WAIT_MS)\n\
     listener:close()\n",
);

#[cfg(feature = "dns")]
const DNS_SCRIPT: (&str, &str) = (
    "dns_script",
    "--!strict\n\
     local dns = require('@dream/dns')\n\
     local request, message, kind = dns.resolve('example.com', 443, { timeoutMs = 5000, maxAddresses = 8 })\n\
     if not request then error(message) end\n\
     local done: boolean = request:wait(250)\n\
     local status: dream_dns_Status = request.status\n\
     local addresses, why, failure = request:take()\n\
     local first: string? = addresses and addresses[1]\n\
     local failed: dream_dns_ErrorKind? = failure\n\
     request:cancel()\n\
     request:close()\n\
     print(done, status, first, why, failed, kind, request.host, request.asciiHost, request.port, request.closed, dns.MAX_TIMEOUT_MS, dns.MAX_ADDRESSES)\n",
);

/// A strict walker over the tree: refinement on `kind` narrows each union to its node type.
#[cfg(feature = "syntax")]
const SYNTAX_SCRIPT: (&str, &str) = (
    "syntax_script",
    "--!strict\n\
     local luau = require('@dream/luau')\n\
     local result = luau.parse('local x = 1', { tokens = true })\n\
     local function names(stat: dream_luau_Stat): { string }\n\
       if stat.kind ~= 'StatLocal' then\n\
         return {}\n\
       end\n\
       local out = {}\n\
       for _, var in stat.vars do\n\
         table.insert(out, var.name)\n\
       end\n\
       return out\n\
     end\n\
     local function callee(expr: dream_luau_Expr): string?\n\
       if expr.kind == 'ExprCall' and expr.func.kind == 'ExprGlobal' then\n\
         return expr.func.name\n\
       end\n\
       return nil\n\
     end\n\
     local first: dream_luau_Stat = result.root.body[1]\n\
     local line: number = first.line + first.endColumn + result.lineStarts[1]\n\
     local comment: dream_luau_Comment? = result.comments[1]\n\
     local kinds: dream_luau_TokenKinds = luau.tokenKinds\n\
     local tokens: buffer? = result.tokens\n\
     print(names(first), callee, line, comment, kinds.name, tokens, #result.errors)\n",
);

const SCRIPTS: &[(&str, &str)] = &[
    (
        "quat_script",
        "--!strict\n\
         local quat = require('@dream/quat')\n\
         local q: integer = quat.axisAngle(vector.create(0, 0, 1), 1.0)\n\
         local r: vector = quat.rotate(q, vector.create(1, 0, 0))\n\
         local m: integer = quat.mul(q, quat.IDENTITY)\n\
         local s: integer = quat.slerp(q, quat.inverse(m), 0.5)\n\
         local x, y, z, w = quat.toXYZW(s)\n\
         local angle: number = quat.angleTo(q, m) + x + y + z + w\n\
         local k: integer = quat.key(q, 3)\n\
         local back: integer = quat.keyRotation(k)\n\
         local flags: number = quat.keyFlags(k)\n\
         local math = quat.math()\n\
         local lowered: integer = math:mul(back, math:slerp(q, m, 0.25))\n\
         print(r, angle, flags, lowered)\n",
    ),
    (
        "raster_script",
        "--!strict\n\
         local raster = require('@dream/raster')\n\
         local c: integer = raster.rgba8(1, 2, 3, 4)\n\
         local d: integer = raster.lerp(c, raster.WHITE, 0.5)\n\
         local r, g, b, a = raster.channels(d)\n\
         local sum: number = r + g + b + a + raster.packed(c)\n\
         local m = raster.math()\n\
         local e: integer = m:add(m:mul(c, d), m:scale(raster.BLACK, 0.5))\n\
         local wide: integer = raster.widen(e)\n\
         local narrow: integer = m:narrow(m:lerp16(wide, raster.WHITE16, 0.5))\n\
         local clip: integer = raster.clip(0, 0, 10, 10)\n\
         local minX, minY, maxX, maxY = raster.clipBounds(raster.CLIP_ALL)\n\
         print(sum, narrow, clip, minX + minY + maxX + maxY)\n",
    ),
    (
        "soft_script",
        "--!strict\n\
         local raster = require('@dream/raster')\n\
         local soft = require('@dream/soft-render')\n\
         local renderer = soft.renderer()\n\
         local frame = renderer:beginFrame(64, 48)\n\
         frame:clear(raster.BLACK)\n\
         frame:rect(vector.create(0, 0, 0), vector.create(8, 8, 0), raster.WHITE, raster.CLIP_ALL)\n\
         local texture = renderer:createTexture(2, 2, buffer.create(16))\n\
         frame:image(vector.create(0, 0, 0), vector.create(2, 2, 0), vector.create(0, 0, 0), vector.create(1, 1, 0), texture, raster.WHITE, raster.CLIP_ALL)\n\
         local vertices = soft.vertices()\n\
         local mesh = buffer.create(soft.VERTEX_BYTES * 3)\n\
         local next: number = vertices:write(mesh, 0, vector.create(0, 0, 0), vector.create(0, 0, 0), soft.premultiply(raster.WHITE))\n\
         frame:mesh(mesh, buffer.create(12), texture, raster.CLIP_ALL)\n\
         frame:finish()\n\
         local width: number = renderer.width + frame.width + texture.width\n\
         texture:free()\n\
         print(next, width)\n",
    ),
    (
        "udp_script",
        "--!strict\n\
         local udp = require('@dream/udp')\n\
         local schema = udp.schema({ version = 1, channels = { { name = 'state', delivery = 'unreliable' } }, events = { { name = 'ping', channel = 'state', maxPayload = 8 } } })\n\
         local id: integer = schema:eventId('ping') or 0i\n\
         local name: string = schema:eventName(id) or ''\n\
         local count: number = schema.eventCount\n\
         local client = udp.client({ schema = schema })\n\
         client:update()\n\
         local kind, peer, a = client:pollInto(buffer.create(64))\n\
         client:sendEvent(id, buffer.create(8), 0, 8)\n\
         local rtt: number? = client.rtt\n\
         print(name, count, kind, peer, a, rtt, client.status, udp.MAX_CHANNELS)\n",
    ),
];

/// The plan's definitions parse and type check in Luau's own frontend, and a strict script
/// against every built-in module has no diagnostics: the declared API and the runtime agree.
#[test]
fn the_generated_definitions_type_check_and_typed_scripts_pass_strict_mode() {
    let plan = plan();
    let definitions = plan.type_definitions();
    assert!(definitions.contains("declare extern type dream_quat_Math with"), "{definitions}");
    assert!(definitions.contains("declare extern type dream_udp_Client with"), "{definitions}");
    assert!(definitions.contains("    IDENTITY: integer,"), "{definitions}");
    for fallback in ["(self, ...any): any", "(...any) -> ...any", ": any,\n", ": any\n"] {
        assert!(!definitions.contains(fallback), "every built-in member is typed ({fallback:?} found):\n{definitions}");
    }
    assert!(!definitions.contains("declare quat"), "no compatibility global is declared:\n{definitions}");
    let mut all_scripts: Vec<(&str, &str)> = SCRIPTS.to_vec();
    #[cfg(feature = "bytes")]
    all_scripts.push(BYTES_SCRIPT);
    #[cfg(feature = "intern")]
    all_scripts.push(INTERN_SCRIPT);
    #[cfg(feature = "intl")]
    all_scripts.push(INTL_SCRIPT);
    #[cfg(feature = "syntax")]
    all_scripts.push(SYNTAX_SCRIPT);
    #[cfg(feature = "fs")]
    all_scripts.push(FS_SCRIPT);
    #[cfg(feature = "process")]
    all_scripts.push(PROCESS_SCRIPT);
    #[cfg(feature = "tcp")]
    all_scripts.push(TCP_SCRIPT);
    #[cfg(feature = "dns")]
    all_scripts.push(DNS_SCRIPT);
    let scripts = plan.analysis_sources(Scripts(all_scripts.iter().copied().collect()));
    let options = AnalysisOptions {
        definitions: vec![Definitions { name: "dream.d.luau".to_owned(), source: definitions.clone() }],
        ..Default::default()
    };
    let analysis = match Analysis::new(scripts, options) {
        Ok(analysis) => analysis,
        Err(error) => panic!("{error}\n---\n{definitions}"),
    };
    for (name, _) in &all_scripts {
        let report = analysis.check(name, false);
        let text: Vec<String> = report
            .diagnostics
            .iter()
            .map(|d| format!("{name}:{}:{}: {} ({:?})", d.span.begin_line + 1, d.span.begin_column + 1, d.text, d.kind))
            .collect();
        assert!(report.is_clean(), "{}\n---\n{definitions}", text.join("\n"));
    }
    // The reusable gate every extension crate runs: the same proof, from the plan alone.
    plan.check_definitions().unwrap();
    // A signature that is not Luau, or that names a type that does not exist, fails that gate
    // with the frontend's diagnostics rather than finalizing quietly.
    struct Broken(&'static str);
    impl l3i::extension::Extension for Broken {
        fn id(&self) -> &'static str {
            "dream.broken"
        }
        fn describe(&self, d: &mut l3i::extension::ExtensionDescriptor) -> l3i::Result<()> {
            d.module("@dream/broken").function("f", || 1i64).signature(self.0);
            Ok(())
        }
    }
    let broken = RuntimePlan::builder().extension(Broken("(foo: Completely Broken Syntax")).finalize().unwrap();
    let error = broken.check_definitions().unwrap_err().to_string();
    assert!(error.contains("declared types do not check") && error.contains("@dream/broken"), "{error}");
    let missing = RuntimePlan::builder().extension(Broken("() -> NoSuchType")).finalize().unwrap();
    let error = missing.check_definitions().unwrap_err().to_string();
    assert!(error.contains("NoSuchType"), "{error}");
    // The definitions are also what a runtime built from the plan reports.
    let runtime = l3i::Runtime::from_plan(&plan).unwrap();
    assert_eq!(runtime.type_definitions().as_deref(), Some(definitions.as_str()));
    // A definitions file with an error is refused with its diagnostics, not silently ignored.
    let broken = AnalysisOptions {
        definitions: vec![Definitions { name: "broken.d.luau".to_owned(), source: "declare oops: Nope\n".to_owned() }],
        ..Default::default()
    };
    let error = Analysis::new(Scripts(HashMap::new()), broken).err().unwrap().to_string();
    assert!(error.contains("broken.d.luau:1") && error.contains("Nope"), "{error}");
}

// ---- views, forward references, owned rows ----------------------------------------------------

struct Numbers(Vec<i64>);

impl l3i::sequence::SequenceSource for Numbers {
    const NAME: &'static str = "dream.views.Numbers";
    type Item = l3i::convert::Integer;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, index: usize) -> Option<Self::Item> {
        self.0.get(index).map(|n| l3i::convert::Integer(*n))
    }
}

/// A row that is deliberately not `Clone`: the sequence moves it into each userdata.
struct Row {
    name: String,
}

// SAFETY: plain Rust data.
unsafe impl l3i::userdata::Userdata for Row {
    const NAME: &'static str = "dream.views.Row";
}

struct Rows(Vec<String>);

impl l3i::sequence::SequenceSource for Rows {
    const NAME: &'static str = "dream.views.Rows";
    type Item = l3i::userdata::Owned<Row>;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, index: usize) -> Option<Self::Item> {
        self.0.get(index).map(|name| l3i::userdata::Owned(Row { name: name.clone() }))
    }
}

struct Countdown(i64);

impl l3i::sequence::StreamSource for Countdown {
    const NAME: &'static str = "dream.views.Countdown";
    type Item = f64;
    type Cursor = std::cell::Cell<i64>;
    fn open(&self) -> Self::Cursor {
        std::cell::Cell::new(self.0)
    }
    fn next(cursor: &Self::Cursor) -> Option<Self::Item> {
        let value = cursor.get();
        if value <= 0 {
            return None;
        }
        cursor.set(value - 1);
        Some(value as f64)
    }
}

struct Views;

impl l3i::extension::Extension for Views {
    fn id(&self) -> &'static str {
        "dream.views"
    }
    fn describe(&self, d: &mut l3i::extension::ExtensionDescriptor) -> l3i::Result<()> {
        d.sequence::<Numbers>("dream.views.Numbers").item_type("integer");
        d.sequence::<Rows>("dream.views.Rows").item_type("dream_views_Row");
        d.stream::<Countdown>("dream.views.Countdown").item_type("number");
        d.userdata::<Row>("dream.views.Row").getter("name", |r: &Row| r.name.clone()).signature("string");
        d.module("@dream/views")
            .function("numbers", |call: &l3i::bind::Call, count: l3i::convert::Exact<i64>| {
                l3i::sequence::Sequence::push(call, Numbers((1..=count.0).map(|n| n * 10).collect()))
                    .map(l3i::value::Value::store)?
            })
            .signature("(count: number) -> dream_views_Numbers")
            .function("rows", |call: &l3i::bind::Call| {
                l3i::sequence::Sequence::push(call, Rows(vec!["a".into(), "b".into()])).map(l3i::value::Value::store)?
            })
            .signature("() -> dream_views_Rows")
            .function("countdown", |call: &l3i::bind::Call, from: l3i::convert::Exact<i64>| {
                l3i::sequence::Stream::push(call, Countdown(from.0)).map(l3i::value::Value::store)?
            })
            .signature("(from: number) -> dream_views_Countdown");
        // A parent module whose member is typed as a module declared after it (a forward
        // reference in plan order): the renderer orders the definitions by reference.
        d.module("@dream/parent").function("child", || 1i64).signature("() -> Module__dream_parent_child");
        d.module("@dream/parent/child").function("leaf", || 2i64).signature("() -> number");
        Ok(())
    }
}

const VIEW_SCRIPT: &str = "--!strict\n\
    local views = require('@dream/views')\n\
    local s = views.numbers(3)\n\
    local n: number = #s\n\
    local first: integer? = s[1]\n\
    local sum: number = 0\n\
    for i, v in s do local x: integer = v sum += i end\n\
    local t: { integer } = s:toTable()\n\
    local rows = views.rows()\n\
    local names = ''\n\
    for _, r in rows do local name: string = r.name names ..= name end\n\
    local c = views.countdown(3)\n\
    local total: number = 0\n\
    for _, v in c do total += v end\n\
    return n, first, sum, #t, names, total\n";

#[test]
fn views_are_typed_forward_module_references_resolve_and_owned_rows_need_no_clone() {
    let plan = RuntimePlan::builder().extension(Views).finalize().unwrap();
    let definitions = plan.type_definitions();
    assert!(definitions.contains("    [number]: integer?"), "{definitions}");
    assert!(
        definitions.contains("function __iter(self): (({}, number) -> (number?, dream_views_Row), {}, number)"),
        "{definitions}"
    );
    assert!(definitions.contains("    function toTable(self): { integer }"), "{definitions}");
    let child = definitions.find("export type Module__dream_parent_child").unwrap();
    let parent = definitions.find("export type Module__dream_parent =").unwrap();
    assert!(child < parent, "the referenced module is declared first:\n{definitions}");
    plan.check_definitions().unwrap();
    // The strict script type checks through the stubs...
    let scripts = plan.analysis_sources(Scripts([("view_script", VIEW_SCRIPT)].into_iter().collect()));
    let options = AnalysisOptions {
        definitions: vec![Definitions { name: "views.d.luau".to_owned(), source: definitions.clone() }],
        ..Default::default()
    };
    let analysis = Analysis::new(scripts, options).unwrap_or_else(|e| panic!("{e}\n{definitions}"));
    let report = analysis.check("view_script", false);
    let text: Vec<String> = report
        .diagnostics
        .iter()
        .map(|d| format!("{}:{}: {}", d.span.begin_line + 1, d.span.begin_column + 1, d.text))
        .collect();
    assert!(report.is_clean(), "{}", text.join("\n"));
    // ...and runs with the same answers on the VM.
    let runtime = l3i::Runtime::from_plan(&plan).unwrap();
    let (n, first, sum, len, names, total): (f64, l3i::convert::Integer, f64, f64, String, f64) =
        runtime.eval(VIEW_SCRIPT).unwrap();
    assert_eq!((n, first.0, sum, len, names.as_str(), total), (3.0, 10, 6.0, 3.0, "ab", 6.0));
}
