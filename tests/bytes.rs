//! `@dream/bytes` through Luau: searching, record strings, varints, the widths and orders
//! `buffer` lacks, the codecs, the digests, the text codecs, and (jit) the native lowering.

use l3i::Runtime;
use l3i::bytes::BytesExtension;
use l3i::extension::{RuntimePlan, RuntimePolicy};

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/bytes", "bytes"))
        .extension(BytesExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn searching_comparing_and_slicing_work_on_buffers_and_strings() {
    runtime()
        .exec(
            "local hay = 'the quick brown fox jumps over the lazy dog' \
             assert(bytes.find(hay, 'quick') == 4, 'find') \
             assert(bytes.find(hay, 'the', 1) == 31, 'find from start') \
             assert(bytes.find(hay, 'cat') == nil, 'absent') \
             assert(bytes.find(hay, '') == 0 and bytes.find(hay, '', 5) == 5, 'empty needle') \
             assert(bytes.find(hay, 'dog', 100) == nil, 'start past the end') \
             assert(bytes.rfind(hay, 'the') == 31 and bytes.rfind(hay, 'the', 20) == 0, 'rfind') \
             assert(bytes.count('aaaa', 'aa') == 2 and bytes.count(hay, '') == 0, 'count') \
             local buf = buffer.fromstring(hay) \
             assert(bytes.find(buf, buffer.fromstring('fox')) == 16, 'buffers on both sides') \
             assert(bytes.equals(buf, hay) and not bytes.equals(buf, 'the'), 'equals') \
             assert(bytes.compare('abc', 0, 'abd', 0, 3) == -1 and bytes.compare('abc', 1, 'xbc', 1, 2) == 0 and bytes.compare('b', 0, 'a', 0, 1) == 1, 'compare') \
             local ok, err = pcall(bytes.compare, 'abc', 2, 'abc', 0, 2) assert(not ok and err:find('past the end'), err) \
             assert(bytes.startsWith(hay, 'the') and bytes.startsWith(hay, 'quick', 4) and not bytes.startsWith(hay, 'quick'), 'startsWith') \
             local piece = bytes.slice(hay, 16, 3) \
             assert(type(piece) == 'buffer' and buffer.tostring(piece) == 'fox', 'slice') \
             local ok2, err2 = pcall(bytes.slice, hay, 40, 10) assert(not ok2 and err2:find('10 bytes at offset 40'), err2) \
             assert(bytes.toHex('\\0\\255ab') == '00ff6162', 'toHex') \
             assert(buffer.tostring(bytes.fromHex('00 FF 61 62')) == '\\0\\255ab', 'fromHex') \
             local ok3, err3 = pcall(bytes.fromHex, 'abc') assert(not ok3 and err3:find('odd'), err3) \
             local ok4, err4 = pcall(bytes.fromHex, 'zz') assert(not ok4 and err4:find('not a hex digit'), err4)",
        )
        .unwrap();
}

#[test]
fn record_strings_and_varints_round_trip() {
    runtime()
        .exec(
            "local buf = buffer.create(64) \
             local next = bytes.writeCString(buf, 0, 'name') assert(next == 5, 'terminator counted') \
             local text, after = bytes.readCString(buf, 0) assert(text == 'name' and after == 5, 'readCString') \
             next = bytes.writeCString(buf, 8, 'ab', 8) assert(next == 16, 'fixed field') \
             assert(buffer.readu8(buf, 10) == 0 and buffer.readu8(buf, 15) == 0, 'padded with NUL') \
             text, after = bytes.readCString(buf, 8, 8) assert(text == 'ab' and after == 16, 'fixed field read') \
             text, after = bytes.readCString('abc', 0) assert(text == 'abc' and after == 3, 'unterminated runs to the end') \
             text, after = bytes.readCString('abcdef', 1, 3) assert(text == 'bcd' and after == 4, 'full field, no NUL') \
             local ok, err = pcall(bytes.writeCString, buf, 0, 'toolong', 4) assert(not ok and err:find('do not fit'), err) \
             local ok2, err2 = pcall(bytes.writeCString, buf, 0, 'a\\0b') assert(not ok2 and err2:find('contains a NUL'), err2) \
             local ok3, err3 = pcall(bytes.readCString, buf, 64, 1) assert(not ok3 and err3:find('past the end'), err3) \
             -- Varints: the encodings of 300 and -1 are the protobuf ones. \
             next = bytes.writeVarint(buf, 0, 300) assert(next == 2, 'two bytes') \
             assert(buffer.readu8(buf, 0) == 0xAC and buffer.readu8(buf, 1) == 0x02, 'LEB128 bytes') \
             local value, after2 = bytes.readVarint(buf, 0) assert(value == 300i and after2 == 2, 'readVarint') \
             next = bytes.writeVarint(buf, 0, -1i) assert(next == 10, 'the full 64 bits take ten bytes') \
             value = bytes.readVarint(buf, 0) assert(value == -1i, 'all ones round trip') \
             next = bytes.writeSignedVarint(buf, 0, -1) assert(next == 1 and buffer.readu8(buf, 0) == 0x7F, 'signed -1') \
             next = bytes.writeSignedVarint(buf, 0, -123456) \
             value, after2 = bytes.readSignedVarint(buf, 0) assert(value == -123456i and after2 == next, 'signed round trip') \
             next = bytes.writeSignedVarint(buf, 0, 64) assert(next == 2, 'sign bit forces a second byte') \
             assert(bytes.readSignedVarint(buf, 0) == 64i) \
             buffer.fill(buf, 0, 0xFF, 11) \
             local ok4, err4 = pcall(bytes.readVarint, buf, 0) assert(not ok4 and err4:find('overflows'), err4) \
             local ok5, err5 = pcall(bytes.readVarint, buffer.fromstring('\\128'), 0) assert(not ok5 and err5:find('unterminated'), err5)",
        )
        .unwrap();
}

#[test]
fn the_widths_and_orders_buffer_lacks_agree_with_buffer_and_with_the_receiver() {
    runtime()
        .exec(
            "local buf = buffer.create(32) \
             local B = bytes.math() \
             -- Big-endian 16 and 32: the same bits buffer writes little-endian, reversed. \
             bytes.writeu16be(buf, 0, 0x1234) assert(buffer.readu8(buf, 0) == 0x12 and buffer.readu8(buf, 1) == 0x34, 'be16 bytes') \
             assert(bytes.readu16be(buf, 0) == 0x1234 and B:readu16be(buf, 0) == 0x1234, 'readu16be') \
             bytes.writei16be(buf, 0, -2) assert(bytes.readi16be(buf, 0) == -2 and bytes.readu16be(buf, 0) == 0xFFFE, 'signed 16') \
             bytes.writeu32be(buf, 0, 0xDEADBEEF) assert(bytes.readu32be(buf, 0) == 0xDEADBEEF and buffer.readu32(buf, 0) == 0xEFBEADDE, 'be32') \
             assert(bytes.readi32be(buf, 0) == -559038737 and B:readi32be(buf, 0) == -559038737, 'signed 32') \
             bytes.writei32be(buf, 0, -1) assert(bytes.readu32be(buf, 0) == 0xFFFFFFFF, 'i32be of -1') \
             -- 24-bit, both orders. \
             bytes.writeu24(buf, 0, 0x010203) assert(buffer.readu8(buf, 0) == 3 and buffer.readu8(buf, 2) == 1, 'u24 little-endian bytes') \
             assert(bytes.readu24(buf, 0) == 0x010203 and B:readu24(buf, 0) == 0x010203, 'readu24') \
             bytes.writeu24be(buf, 0, 0x010203) assert(buffer.readu8(buf, 0) == 1 and buffer.readu8(buf, 2) == 3, 'u24be bytes') \
             assert(bytes.readu24be(buf, 0) == 0x010203 and B:readu24be(buf, 0) == 0x010203, 'readu24be') \
             bytes.writei24(buf, 0, -5) assert(bytes.readi24(buf, 0) == -5 and bytes.readu24(buf, 0) == 0xFFFFFB, 'signed 24') \
             bytes.writei24be(buf, 0, -70000) assert(bytes.readi24be(buf, 0) == -70000 and B:readi24be(buf, 0) == -70000, 'signed 24 be') \
             -- 64-bit integers and floats. \
             bytes.writei64be(buf, 0, 0x0102030405060708i) assert(buffer.readu8(buf, 0) == 1 and buffer.readu8(buf, 7) == 8, 'i64be bytes') \
             assert(bytes.readi64be(buf, 0) == 0x0102030405060708i and B:readi64be(buf, 0) == 0x0102030405060708i, 'readi64be') \
             assert(buffer.readinteger(buf, 0) == 0x0807060504030201i, 'buffer sees the reverse') \
             bytes.writef32be(buf, 0, 1.5) assert(bytes.readf32be(buf, 0) == 1.5 and buffer.readu8(buf, 0) == 0x3F and buffer.readu8(buf, 1) == 0xC0, 'f32be') \
             bytes.writef64be(buf, 0, -0.125) assert(bytes.readf64be(buf, 0) == -0.125 and B:readf64be(buf, 0) == -0.125, 'f64be') \
             -- Half floats. \
             bytes.writef16(buf, 0, 1.0) assert(buffer.readu16(buf, 0) == 0x3C00 and bytes.readf16(buf, 0) == 1.0, 'f16 one') \
             bytes.writef16be(buf, 0, -2.0) assert(buffer.readu8(buf, 0) == 0xC0 and bytes.readf16be(buf, 0) == -2.0, 'f16be') \
             bytes.writef16(buf, 0, 65504) assert(bytes.readf16(buf, 0) == 65504, 'largest half') \
             bytes.writef16(buf, 0, 1e6) assert(bytes.readf16(buf, 0) == math.huge, 'overflow to infinity') \
             bytes.writef16(buf, 0, 0 / 0) local nan = bytes.readf16(buf, 0) assert(nan ~= nan, 'NaN survives') \
             B:writef16(buf, 2, 0.5) assert(B:readf16(buf, 2) == 0.5, 'receiver halves') \
             -- Strings are readable sources too. \
             assert(bytes.readu16be('\\1\\2', 0) == 0x0102 and bytes.readf32be(buffer.tostring(buf), 0) ~= nil, 'string sources') \
             -- Truncation matches buffer: the low bits of the integer form. \
             bytes.writeu16be(buf, 0, 0x12345) assert(bytes.readu16be(buf, 0) == 0x2345, 'truncated like buffer.writeu16') \
             -- Bounds: every width is checked with the name of the call. \
             local ok, err = pcall(bytes.readu32be, buf, 30) assert(not ok and err:find('bytes.readu32be: 4 bytes at offset 30 past the end %(length 32%)'), err) \
             local ok2, err2 = pcall(B.readi64be, B, buf, -1) assert(not ok2 and err2:find('negative offset'), err2) \
             local ok3, err3 = pcall(bytes.writef16, buf, 31, 1) assert(not ok3 and err3:find('bytes.writef16: 2 bytes at offset 31'), err3) \
             local ok4 = pcall(bytes.readu16be, buf, 1.5) assert(not ok4, 'a fractional offset is not exact')",
        )
        .unwrap();
}

#[test]
fn a_plan_runtime_lists_its_modules_as_registered() {
    assert!(runtime().registered_require_modules().iter().any(|module| module == "@dream/bytes"));
}

#[cfg(feature = "bytes-codecs")]
#[test]
fn codecs_round_trip_and_decode_foreign_fixtures() {
    // Fixtures from Python 3.14: zstd.compress, lzma FORMAT_ALONE, lzma FORMAT_XZ (CRC32) and
    // gzip.compress, all of the same 144-byte payload.
    runtime()
        .exec(
            "local payload = string.rep('hello zstd, hello lzma, ', 6) \
             local function hex(h) return bytes.fromHex(h) end \
             assert(buffer.tostring(bytes.zstdDecompress(hex('28b52ffd2090dd00008068656c6c6f207a7374642c206c7a6d6103006f6702e091632509'))) == payload, 'zstd') \
             assert(buffer.tostring(bytes.lzmaDecompress(hex('5d00008000ffffffffffffffff00341949ee8de9185b6a698b936424a433162674d2afbdc1f189fffff8116000'))) == payload, 'lzma') \
             assert(buffer.tostring(bytes.lzmaDecompress(hex('fd377a585a0000016922de360200210116000000742fe5a3e0008f001a5d00341949ee8de9185b6a698b936424a433162674d2afbdb10e200000000084c8873b00013290010000000e325de33e300d8b020000000001595a'), { format = 'xz' })) == payload, 'xz') \
             assert(buffer.tostring(bytes.inflate(hex('1f8b08000000000002ffcb48cdc9c957a82a2e49d151c800b373aa7213616cfa8b030084c8873b90000000'), { format = 'gzip' })) == payload, 'gzip') \
             assert(buffer.tostring(bytes.inflate(hex('789ccb48cdc9c957a82a2e49d151c800b373aa7213616cfa8b0300868632d7'))) == payload, 'zlib from Python') \
             -- Round trips through our own encoders, every framing and a few levels. \
             local big = string.rep('The quick brown fox jumps over the lazy dog. ', 400) \
             for _, format in { 'zlib', 'raw', 'gzip' } do \
                 for _, level in { 0, 1, 6, 10 } do \
                     local packed = bytes.deflate(big, { format = format, level = level }) \
                     assert(buffer.tostring(bytes.inflate(packed, { format = format })) == big, format .. ' level ' .. level) \
                 end \
             end \
             assert(buffer.len(bytes.deflate(big)) < #big / 10, 'it compresses') \
             assert(buffer.tostring(bytes.inflate(bytes.deflate(big))) == big, 'defaults agree') \
             local block = bytes.lz4Compress(big) \
             assert(buffer.tostring(bytes.lz4Decompress(block, #big)) == big, 'lz4 block') \
             assert(buffer.tostring(bytes.lz4FrameDecompress(bytes.lz4FrameCompress(big))) == big, 'lz4 frame') \
             assert(buffer.tostring(bytes.lz4FrameDecompress(bytes.lz4FrameCompress(''))) == '', 'empty frame') \
             local zstd = bytes.zstdCompress(big) \
             assert(buffer.len(zstd) < #big / 10, 'zstd compresses') \
             assert(buffer.tostring(bytes.zstdDecompress(zstd)) == big, 'zstd round trip') \
             assert(buffer.tostring(bytes.zstdDecompress(bytes.zstdCompress(big, { level = 1 }))) == big, 'zstd level 1') \
             assert(buffer.tostring(bytes.zstdDecompress(bytes.zstdCompress(''))) == '', 'empty zstd frame') \
             assert(buffer.tostring(bytes.zstdCompress(big)) == buffer.tostring(zstd), 'zstd is deterministic') \
             local ok12, err12 = pcall(bytes.zstdCompress, big, { level = 3 }) assert(not ok12 and err12:find('bytes.zstdCompress: level 3 is not implemented'), err12) \
             -- Limits and corruption are errors in the call's name, never a truncated result. \
             local ok, err = pcall(bytes.inflate, bytes.deflate(big), { maxSize = 100 }) assert(not ok and err:find('bytes.inflate: output exceeds maxSize'), err) \
             local ok2, err2 = pcall(bytes.inflate, 'not deflate at all') assert(not ok2 and err2:find('corrupt'), err2) \
             local ok3, err3 = pcall(bytes.inflate, 'abc', { format = 'gzip' }) assert(not ok3 and err3:find('not a gzip stream'), err3) \
             local ok4, err4 = pcall(bytes.inflate, big, { format = 'bzip2' }) assert(not ok4 and err4:find('not zlib, raw or gzip'), err4) \
             local ok5, err5 = pcall(bytes.deflate, big, { level = 11 }) assert(not ok5 and err5:find('outside 0..=10'), err5) \
             local ok6, err6 = pcall(bytes.lz4Decompress, block, 10) assert(not ok6 and err6:find('bytes.lz4Decompress'), err6) \
             -- Caps hold while producing: a block size past maxSize allocates nothing, an LZMA stream stops at the byte that would exceed it. \
             local ok9, err9 = pcall(bytes.lz4Decompress, block, #big, { maxSize = 100 }) assert(not ok9 and err9:find('decompressedSize %d+ exceeds maxSize'), err9) \
             local ok10, err10 = pcall(bytes.lzmaDecompress, hex('5d00008000ffffffffffffffff00341949ee8de9185b6a698b936424a433162674d2afbdc1f189fffff8116000'), { maxSize = 100 }) assert(not ok10 and err10:find('bytes.lzmaDecompress: output exceeds maxSize'), err10) \
             local ok11, err11 = pcall(bytes.lzmaDecompress, hex('fd377a585a0000016922de360200210116000000742fe5a3e0008f001a5d00341949ee8de9185b6a698b936424a433162674d2afbdb10e200000000084c8873b00013290010000000e325de33e300d8b020000000001595a'), { format = 'xz', maxSize = 100 }) assert(not ok11 and err11:find('bytes.lzmaDecompress: output exceeds maxSize'), err11) \
             local ok7, err7 = pcall(bytes.zstdDecompress, 'garbage') assert(not ok7 and err7:find('bytes.zstdDecompress'), err7) \
             local ok8, err8 = pcall(bytes.deflate, big, { format = 'zlib', bogus = 1 }) assert(not ok8 and err8:find('bogus'), err8)",
        )
        .unwrap();
}

#[cfg(feature = "bytes-digests")]
#[test]
fn digests_match_their_published_vectors_and_the_hasher_matches_one_shot() {
    runtime()
        .exec(
            "local fox = 'The quick brown fox jumps over the lazy dog' \
             assert(bytes.crc32(fox) == 0x414FA339, 'crc32') \
             assert(bytes.crc32('') == 0 and bytes.crc32('') == bytes.crc32('', 0), 'crc32 empty') \
             assert(bytes.crc32(fox) == bytes.crc32(' jumps over the lazy dog', bytes.crc32('The quick brown fox')), 'crc32 seed continues') \
             assert(bytes.adler32('Wikipedia') == 0x11E60398, 'adler32') \
             assert(bytes.fnv1a32('') == 0x811C9DC5 and bytes.fnv1a32('a') == 0xE40C292C, 'fnv1a32') \
             assert(bytes.fnv1a64('a') == 0xAF63DC4C8601EC8Ci, 'fnv1a64') \
             assert(bytes.xxh32('') == 0x02CC5D05 and bytes.xxh32('a', 0) == 0x550D7456, 'xxh32') \
             assert(bytes.xxh64('') == 0xEF46DB3751D8E999i, 'xxh64') \
             assert(bytes.xxh3('') == 0x2D06800538D394C2i, 'xxh3') \
             assert(bytes.md5('') == 'd41d8cd98f00b204e9800998ecf8427e', 'md5') \
             assert(bytes.sha1('abc') == 'a9993e364706816aba3e25717850c26c9cd0d89d', 'sha1') \
             assert(bytes.sha256('abc') == 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad', 'sha256') \
             assert(bytes.blake3('') == 'af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262', 'blake3') \
             assert(bytes.toHex(bytes.digest('abc', 'sha256')) == bytes.sha256('abc'), 'digest bytes') \
             assert(bytes.toHex(bytes.digest(fox, 'crc32')) == '414fa339', 'checksum digests are big-endian') \
             assert(bytes.sha256(buffer.fromstring('abc')) == bytes.sha256('abc'), 'buffers hash the same') \
             local h = bytes.hasher('sha256') \
             h:update('a') h:update(buffer.fromstring('b')) h:update('c') \
             assert(h:finish() == bytes.sha256('abc') and h:algorithm() == 'sha256', 'incremental') \
             assert(h:finish() == bytes.sha256('abc'), 'finish leaves the state alone') \
             h:update('d') assert(h:finish() == bytes.sha256('abcd'), 'and it continues') \
             h:reset() h:update('abc') assert(bytes.toHex(h:finishBytes()) == bytes.sha256('abc'), 'reset') \
             local c = bytes.hasher('crc32') c:update(fox) assert(c:value() == 0x414FA339i, 'checksum value') \
             local x = bytes.hasher('xxh3') x:update('') assert(x:value() == bytes.xxh3(''), 'xxh3 value') \
             local ok, err = pcall(h.value, h) assert(not ok and err:find('sha256 is a digest'), err) \
             local ok2, err2 = pcall(bytes.hasher, 'sha512') assert(not ok2 and err2:find('sha512'), err2) \
             local ok3, err3 = pcall(bytes.crc32, fox, -1) assert(not ok3 and err3:find('seed'), err3)",
        )
        .unwrap();
}

#[cfg(feature = "bytes-text")]
#[test]
fn text_decodes_and_encodes_the_codepages_old_formats_use() {
    runtime()
        .exec(
            "assert(bytes.decode('caf\\233', 'windows-1252') == 'caf\\u{e9}', 'cp1252') \
             assert(bytes.decode('caf\\233', 'latin1') == 'caf\\u{e9}', 'the latin1 label') \
             assert(bytes.encodingName('cp1252') == 'windows-1252' and bytes.encodingName('sjis') == 'Shift_JIS', 'labels') \
             assert(bytes.decode('\\130\\160', 'shift_jis') == '\\u{3042}', 'shift_jis') \
             assert(buffer.tostring(bytes.encode('\\u{3042}', 'shift_jis')) == '\\130\\160', 'encode shift_jis') \
             assert(buffer.tostring(bytes.encode('caf\\u{e9}', 'windows-1252')) == 'caf\\233', 'encode cp1252') \
             local utf16 = bytes.encode('hi\\u{20ac}', 'utf-16le') \
             assert(bytes.toHex(utf16) == '68006900ac20', 'utf-16le is real UTF-16') \
             assert(bytes.decode(utf16, 'utf-16le') == 'hi\\u{20ac}', 'utf-16le round trip') \
             assert(bytes.toHex(bytes.encode('\\u{1f600}', 'utf-16be')) == 'd83dde00', 'surrogate pairs') \
             assert(bytes.decode(buffer.fromstring('ok'), 'utf-8') == 'ok', 'buffers decode') \
             assert(bytes.decode('\\255', 'utf-8') == '\\u{fffd}', 'malformed becomes U+FFFD') \
             local ok, err = pcall(bytes.decode, '\\255', 'utf-8', { strict = true }) assert(not ok and err:find('not valid UTF%-8'), err) \
             local ok2, err2 = pcall(bytes.decode, 'x', 'klingon') assert(not ok2 and err2:find('not a known encoding label'), err2) \
             local ok3, err3 = pcall(bytes.encode, '\\u{3042}', 'windows-1252') assert(not ok3 and err3:find('cannot represent'), err3) \
             assert(bytes.isUtf8('caf\\u{e9}') and not bytes.isUtf8('caf\\233'), 'isUtf8')",
        )
        .unwrap();
}

/// Native lowering: the receiver's integer reads and writes compile to IR with no C call and
/// agree with the module functions; a bad offset exits to the interpreter, which raises.
#[cfg(feature = "jit")]
#[test]
fn integer_reads_and_writes_lower_to_native_code() {
    use l3i::bytes::lowering::lowered_sites;
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::{NativeCodeMode, NativeCodeStatus};
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let policy = RuntimePolicy::new().compat_global("@dream/bytes", "bytes").native_code(NativeCodePolicy {
        mode: NativeCodeMode::Eager,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let plan = RuntimePlan::builder().policy(policy).extension(BytesExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = lowered_sites();
    let template = sandbox
        .load_template(
            &runtime,
            "bytes.lua",
            "--!native\n\
             local B: dream_bytes_Math = bytes.math()\n\
             local buf = buffer.create(64)\n\
             local worst = 0\n\
             for i = 0, 199 do\n\
                 local v = (i * 7919) % 65536\n\
                 B:writeu16be(buf, 0, v)\n\
                 worst = math.max(worst, math.abs(B:readu16be(buf, 0) - bytes.readu16be(buf, 0)))\n\
                 worst = math.max(worst, math.abs(B:readi16be(buf, 0) - bytes.readi16be(buf, 0)))\n\
                 local w = (i * 104729) % 16777216\n\
                 B:writeu24(buf, 4, w)\n\
                 worst = math.max(worst, math.abs(B:readu24(buf, 4) - w))\n\
                 worst = math.max(worst, math.abs(B:readi24(buf, 4) - bytes.readi24(buf, 4)))\n\
                 B:writeu24be(buf, 8, w)\n\
                 worst = math.max(worst, math.abs(B:readu24be(buf, 8) - w))\n\
                 worst = math.max(worst, math.abs(B:readi24be(buf, 8) - bytes.readi24be(buf, 8)))\n\
                 local x = (i * 2654435761) % 4294967296\n\
                 B:writeu32be(buf, 12, x)\n\
                 worst = math.max(worst, math.abs(B:readu32be(buf, 12) - x))\n\
                 worst = math.max(worst, math.abs(B:readi32be(buf, 12) - bytes.readi32be(buf, 12)))\n\
                 local big = integer.mul(integer.create(i), 0x0123456789ABCDEFi)\n\
                 B:writei64be(buf, 16, big)\n\
                 assert(B:readi64be(buf, 16) == big and bytes.readi64be(buf, 16) == big, 'i64be')\n\
             end\n\
             -- A read past the end exits to the interpreter, whose method raises the error.\n\
             local ok, err = pcall(function() local r = B:readu32be(buf, 62) return r end)\n\
             assert(not ok and string.find(err, 'past the end'), err)\n\
             return worst",
        )
        .unwrap();
    let native = template.native_code().expect("compiled");
    assert_eq!(native.status, NativeCodeStatus::Success, "{native:?}");
    assert_eq!(lowered_sites() - before, 15, "fourteen loop sites and the closure");
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "b", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let worst: f64 = results[0].push_to(&runtime.stack().frame()).map(|v| v.read::<f64>().unwrap()).unwrap();
    assert_eq!(worst, 0.0, "lowered results differ from the binder");
    let stats = generator.execution_stats(&runtime.stack());
    assert!(stats.regular_blocks_executed > 0, "{stats:?}");
    assert_eq!(stats.vm_exits_taken, 1, "only the out-of-bounds read exits: {stats:?}");
}
