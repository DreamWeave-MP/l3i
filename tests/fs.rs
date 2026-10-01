//! `@dream/fs` through Luau: byte paths round-trip, readers and writers keep their positions,
//! metadata with and without following links, the walk's columns, links and identity, and every
//! function refused by name when the policy does not grant its capability.

use std::path::{Path, PathBuf};

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::fs::{FsExtension, READ_CAPABILITY, WRITE_CAPABILITY};

/// A fresh folder of the test's own under the integration tests' scratch folder.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dream-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn lua(&self) -> String {
        format!("{:?}", self.0.display().to_string())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime_with(capabilities: &[&str]) -> Runtime {
    let mut policy = RuntimePolicy::new().compat_global("@dream/fs", "fs");
    for capability in capabilities {
        policy = policy.capability(capability);
    }
    let plan = RuntimePlan::builder().policy(policy).extension(FsExtension).finalize().unwrap();
    Runtime::from_plan(&plan).unwrap()
}

fn runtime() -> Runtime {
    runtime_with(&[READ_CAPABILITY, WRITE_CAPABILITY])
}

#[test]
fn files_read_whole_positionally_and_through_a_reader() {
    let scratch = Scratch::new("read");
    std::fs::write(scratch.0.join("data.bin"), b"0123456789").unwrap();
    std::fs::write(scratch.0.join("empty"), b"").unwrap();
    runtime()
        .exec(&format!(
            "local root = {root} \
             local path = root .. '/data.bin' \
             assert(buffer.tostring(fs.readFile(path)) == '0123456789') \
             assert(fs.readFileString(path) == '0123456789') \
             assert(buffer.tostring(fs.readAt(path, 3, 4)) == '3456') \
             assert(buffer.tostring(fs.readAt(path, 8, 100)) == '89', 'short at the end') \
             assert(buffer.len(fs.readAt(path, 10, 4)) == 0, 'empty at the end') \
             local ok, err = pcall(fs.readAt, path, 11, 1) assert(not ok and err:find('offset 11 past the end %(size 10%)'), err) \
             local reader = fs.open(path) \
             assert(reader:size() == 10 and reader:tell() == 0) \
             assert(buffer.tostring(reader:read(3)) == '012' and reader:tell() == 3) \
             local into = buffer.create(4) \
             assert(reader:readInto(into, 1, 2) == 2 and buffer.readstring(into, 1, 2) == '34') \
             assert(buffer.tostring(reader:readAt(8, 5)) == '89' and reader:tell() == 5, 'positional reads keep the position') \
             assert(reader:readAtInto(into, 0) == 4 and buffer.tostring(into) == '0123') \
             reader:seek(9) assert(buffer.tostring(reader:read(5)) == '9') \
             reader:seek(0) reader:skip(10) assert(buffer.len(reader:read(1)) == 0) \
             ok, err = pcall(reader.skip, reader, 1) assert(not ok and err:find('past the end'), err) \
             reader:close() reader:close() \
             ok, err = pcall(reader.read, reader, 1) assert(not ok and err:find('closed'), err) \
             assert(fs.open(root .. '/empty'):size() == 0 and buffer.len(fs.readFile(root .. '/empty')) == 0) \
             local missing, message, kind = fs.open(root .. '/missing') \
             assert(missing == nil and kind == 'notFound' and message:find('^dream.fs.open: .*/missing: '), message) \
             missing, message, kind = fs.open(root) assert(missing == nil and kind == 'isADirectory' and message:find('dream.fs.open'), message)",
            root = scratch.lua()
        ))
        .unwrap();
}

#[test]
fn writes_append_overwrite_in_place_and_truncate() {
    let scratch = Scratch::new("write");
    runtime()
        .exec(&format!(
            "local path = {root} .. '/out.txt' \
             assert(fs.writeFile(path, 'hello world') == 11) \
             fs.writeFile(path, buffer.fromstring('!!'), {{ append = true }}) \
             fs.writeFile(path, 'J', {{ offset = 0 }}) \
             assert(fs.readFileString(path) == 'Jello world!!') \
             local ok, err = pcall(fs.writeFile, path, 'x', {{ offset = 1, append = true }}) assert(not ok and err:find('cannot be combined'), err) \
             local refused, _, kind = fs.writeFile({root} .. '/new', 'x', {{ create = false }}) assert(refused == nil and kind == 'notFound', 'create = false') \
             ok, err = pcall(fs.writeFile, path, 'x', {{ appendd = true }}) assert(not ok and err:find('appendd'), err) \
             local writer = fs.openWrite(path) \
             assert(writer:write('abcdef') == 6 and writer:tell() == 6) \
             writer:writeAt(0, 'XY') assert(writer:tell() == 6) \
             writer:write('0123456789', 2, 3) writer:truncate(7) writer:seek(7) writer:write('Z') \
             writer:close() writer:close() \
             assert(fs.readFileString(path) == 'XYcdef2Z', fs.readFileString(path)) \
             local appender = fs.openWrite(path, {{ append = true }}) \
             appender:write('+') ok, err = pcall(appender.writeAt, appender, 0, 'x') assert(not ok and err:find('appends'), err) \
             appender:close() \
             local keep = fs.openWrite(path, {{ truncate = false }}) keep:write('__') keep:close() \
             assert(fs.readFileString(path) == '__cdef2Z+')",
            root = scratch.lua()
        ))
        .unwrap();
}

#[test]
fn directories_metadata_listing_and_the_walk() {
    let scratch = Scratch::new("walk");
    let root = &scratch.0;
    std::fs::create_dir_all(root.join("Meshes/x")).unwrap();
    std::fs::write(root.join("Meshes/x/Rock.NIF"), b"nif").unwrap();
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    runtime()
        .exec(&format!(
            "local root = {root} \
             local stat = fs.stat(root .. '/a.txt') \
             assert(stat and stat.kind == 'file' and stat.isFile and not stat.isDir and stat.size == 1 and not stat.readonly, 'file stat') \
             assert(stat.modified and stat.modifiedSeconds and stat.modifiedNanoseconds and stat.modified >= stat.modifiedSeconds, 'modified times') \
             local dir = fs.stat(root .. '/Meshes') assert(dir and dir.kind == 'dir' and dir.size == 0, 'directory stat') \
             assert(fs.stat(root .. '/missing') == nil and fs.lstat(root .. '/missing') == nil, 'missing stat') \
             assert(fs.exists(root .. '/a.txt') and not fs.exists(root .. '/missing'), 'exists') \
             local names = fs.list(root) \
             assert(#names == 3 and names[1] == 'Meshes' and names[2] == 'a.txt' and names[3] == 'empty', table.concat(names, ',')) \
             local walk = fs.walk(root, {{ sort = true }}) \
             assert(table.concat(walk.paths, ',') == 'Meshes,Meshes/x,Meshes/x/Rock.NIF,a.txt,empty', table.concat(walk.paths, ',')) \
             assert(table.concat(walk.kinds, ',') == 'dir,dir,file,file,dir', table.concat(walk.kinds, ',')) \
             assert(walk.sizes == nil and #walk.errors == 0, 'plain walk') \
             local files = fs.walk(root, {{ include = 'files', metadata = true, sort = true }}) \
             assert(#files.paths == 2 and files.sizes[1] == 3 and files.sizes[2] == 1 and #files.modifiedSeconds == 2, 'files with metadata') \
             local top = fs.walk(root, {{ maxDepth = 1, include = 'dirs', sort = true }}) \
             assert(table.concat(top.paths, ',') == 'Meshes,empty', table.concat(top.paths, ',')) \
             local missing, message, kind = fs.walk(root .. '/missing') assert(missing == nil and kind == 'notFound' and message:find('dream.fs.walk'), message) \
             local skipped = fs.walk(root .. '/missing', {{ skipErrors = true }}) \
             assert(#skipped.paths == 0 and #skipped.errors == 1, 'skipped errors') \
             local ok, err = pcall(fs.walk, root, {{ include = 'everything' }}) assert(not ok and err:find('include'), err) \
             assert(fs.mkdir(root .. '/new') == true, 'mkdir') \
             local made, _, why = fs.mkdir(root .. '/new') assert(made == nil and why == 'alreadyExists', 'exists') \
             fs.mkdir(root .. '/deep/er/still', {{ recursive = true }}) fs.mkdir(root .. '/deep', {{ recursive = true }}) \
             local removed, _, because = fs.remove(root .. '/deep') assert(removed == nil and because == 'directoryNotEmpty', 'not empty') \
             fs.remove(root .. '/deep', {{ recursive = true }}) fs.remove(root .. '/new') \
             assert(not fs.exists(root .. '/deep') and not fs.exists(root .. '/new'), 'removed') \
             fs.rename(root .. '/a.txt', root .. '/b.txt') assert(fs.readFileString(root .. '/b.txt') == 'a') \
             assert(fs.copy(root .. '/b.txt', root .. '/c.txt') == 1 and fs.readFileString(root .. '/c.txt') == 'a') \
             fs.remove(root .. '/c.txt') assert(not fs.exists(root .. '/c.txt'), 'removed copy') \
             local gone, said, reason = fs.remove(root .. '/c.txt') assert(gone == nil and reason == 'notFound' and said:find('dream.fs.remove'), said) \
             assert(fs.canonicalize(root .. '/Meshes/../b.txt') == fs.canonicalize(root .. '/b.txt'), 'canonicalize resolves ..') \
             assert(fs.absolute('x'):find('[/\\\\]x$') and fs.cwd() ~= '', 'absolute joins the working folder')",
            root = scratch.lua()
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn links_and_identity() {
    let scratch = Scratch::new("links");
    let root = &scratch.0;
    std::fs::write(root.join("source"), b"payload").unwrap();
    std::fs::create_dir_all(root.join("dir")).unwrap();
    runtime()
        .exec(&format!(
            "local root = {root} \
             fs.hardLink(root .. '/source', root .. '/hard') \
             assert(fs.sameFile(root .. '/source', root .. '/hard')) \
             local again, _, kind = fs.hardLink(root .. '/source', root .. '/hard') assert(again == nil and kind == 'alreadyExists', kind) \
             fs.writeFile(root .. '/hard', 'changed', {{ offset = 0 }}) \
             assert(fs.readFileString(root .. '/source') == 'changed', 'one file under two names') \
             fs.symlink(root .. '/source', root .. '/soft') \
             assert(fs.sameFile(root .. '/soft', root .. '/source') and not fs.sameFile(root .. '/soft', root .. '/missing')) \
             assert(fs.readLink(root .. '/soft') == root .. '/source' and fs.readLink(root .. '/missing') == nil) \
             local link = fs.lstat(root .. '/soft') assert(link and link.kind == 'symlink' and link.isSymlink and not link.isFile) \
             local target = fs.stat(root .. '/soft') assert(target and target.kind == 'file' and not target.isSymlink) \
             assert(fs.canonicalize(root .. '/soft') == fs.canonicalize(root .. '/source')) \
             fs.symlink('dir', root .. '/dirlink', {{ directory = true }}) \
             local followed = fs.walk(root, {{ followLinks = true, include = 'dirs', sort = true }}) \
             assert(table.concat(followed.paths, ',') == 'dir,dirlink', table.concat(followed.paths, ',')) \
             local plain = fs.walk(root, {{ sort = true }}) \
             local kinds = {{}} for index, path in plain.paths do kinds[path] = plain.kinds[index] end \
             assert(kinds.soft == 'symlink' and kinds.dirlink == 'symlink' and kinds.hard == 'file') \
             fs.remove(root .. '/dirlink') assert(fs.exists(root .. '/dir'), 'removing a link leaves its target') \
             fs.remove(root .. '/soft') assert(fs.exists(root .. '/source'))",
            root = scratch.lua()
        ))
        .unwrap();
}

// APFS refuses file names that are not UTF-8, so the byte-path round trip runs on Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_comes_back_byte_for_byte() {
    use std::os::unix::ffi::OsStrExt;
    let scratch = Scratch::new("bytes");
    std::fs::write(scratch.0.join(std::ffi::OsStr::from_bytes(b"caf\xe9")), b"latin1").unwrap();
    runtime()
        .exec(&format!(
            "local root = {root} \
             local listed = fs.walk(root, {{ sort = true }}) \
             assert(listed.paths[1] == 'caf\\xe9' and listed.kinds[1] == 'file', 'a name that is not UTF-8 comes back byte for byte') \
             assert(fs.readFileString(root .. '/caf\\xe9') == 'latin1')",
            root = scratch.lua()
        ))
        .unwrap();
}

#[test]
fn every_function_needs_its_capability() {
    let scratch = Scratch::new("denied");
    std::fs::write(scratch.0.join("x"), b"x").unwrap();
    let path = scratch.lua();
    let none = runtime_with(&[]);
    none.exec(&format!(
        "local ok, err = pcall(fs.readFile, {path} .. '/x') \
         assert(not ok and err:find(\"dream.fs.readFile: needs the 'filesystem.read' capability\"), err) \
         ok, err = pcall(fs.writeFile, {path} .. '/y', 'y') \
         assert(not ok and err:find(\"needs the 'filesystem.write' capability\"), err) \
         assert(not pcall(fs.walk, {path}) and not pcall(fs.stat, {path}))"
    ))
    .unwrap();
    let read_only = runtime_with(&[READ_CAPABILITY]);
    read_only
        .exec(&format!(
            "assert(fs.readFileString({path} .. '/x') == 'x') \
             local ok, err = pcall(fs.remove, {path} .. '/x') assert(not ok and err:find('filesystem.write'), err) \
             assert(not pcall(fs.hardLink, {path} .. '/x', {path} .. '/y') and not pcall(fs.openWrite, {path} .. '/y'))"
        ))
        .unwrap();
    assert!(scratch.0.join("x").exists() && !scratch.0.join("y").exists());
}
