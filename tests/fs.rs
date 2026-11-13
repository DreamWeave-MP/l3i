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
         assert(not pcall(fs.walk, {path}) and not pcall(fs.stat, {path})) \
         assert(not pcall(fs.openPositional, {path} .. '/x'))"
    ))
    .unwrap();
    let read_only = runtime_with(&[READ_CAPABILITY]);
    read_only
        .exec(&format!(
            "assert(fs.readFileString({path} .. '/x') == 'x') \
             assert(buffer.tostring(assert(fs.openPositional({path} .. '/x')):readAt(0, 1)) == 'x') \
             local ok, err = pcall(fs.remove, {path} .. '/x') assert(not ok and err:find('filesystem.write'), err) \
             assert(not pcall(fs.hardLink, {path} .. '/x', {path} .. '/y') and not pcall(fs.openWrite, {path} .. '/y')) \
             assert(not pcall(fs.lock, {path} .. '/y')) \
             ok, err = pcall(fs.chmod, {path} .. '/x', 0) assert(not ok and err:find('filesystem.write'), err)"
        ))
        .unwrap();
    assert!(scratch.0.join("x").exists() && !scratch.0.join("y").exists());
}

/// The mode of `path`, read by the test and not by what it tests, without the set-group-ID bit:
/// a directory takes that from its parent, and the scratch folder's is not the test's to choose.
#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o5777
}

/// Every call that creates gives what it creates exactly the mode it was handed. 666 and 777 are
/// asked for because any umask a test runs under narrows them, so only bits set after creation
/// read back whole; 600 is the mode the option is for.
#[cfg(unix)]
#[test]
fn a_mode_is_exactly_the_bits_of_what_a_call_creates() {
    let scratch = Scratch::new("modes");
    let root = &scratch.0;
    runtime()
        .exec(&format!(
            "local root = {root} \
             local function bits(text) return tonumber(text, 8) end \
             assert(fs.writeFile(root .. '/secret', 'key', {{ mode = bits('600') }}) == 3) \
             assert(fs.stat(root .. '/secret').mode == bits('600'), 'stat reports the bits') \
             assert(fs.writeFile(root .. '/wide', 'w', {{ mode = bits('666') }}) == 1) \
             assert(fs.writeFile(root .. '/only', 'o', {{ exclusive = true, mode = bits('640') }}) == 1) \
             assert(fs.writeFile(root .. '/made/atomic', 'a', {{ atomic = true, createParents = true, mode = bits('600') }}) == 1) \
             local writer = assert(fs.openWrite(root .. '/stream', {{ mode = bits('600') }})) \
             assert(writer:write('s') == 1 and writer:close()) \
             assert(fs.createDurable(root .. '/journal', 'j', {{ mode = bits('600') }}) == 1) \
             assert(fs.mkdir(root .. '/private', {{ mode = bits('700') }})) \
             assert(fs.mkdir(root .. '/open', {{ mode = bits('777') }})) \
             assert(fs.mkdir(root .. '/a/b/c', {{ recursive = true, mode = bits('750') }})) \
             assert(fs.mkdir(root .. '/a/b/c', {{ recursive = true, mode = bits('700') }}), 'one that is there is accepted') \
             assert(fs.mkdir(root .. '/unread', {{ mode = bits('300') }})) \
             assert(fs.mkdir(root .. '/shared', {{ mode = bits('1777') }})) \
             assert(fs.mkdir(root .. '/shut/in/leaf', {{ recursive = true, mode = bits('040') }})) \
             local lock = assert(fs.lock(root .. '/keeper.lock', {{ mode = bits('600') }})) \
             assert(lock:unlock())"
            , root = scratch.lua()
        ))
        .unwrap();
    for (name, mode) in [
        ("secret", 0o600),
        ("wide", 0o666),
        ("only", 0o640),
        ("made/atomic", 0o600),
        ("stream", 0o600),
        ("journal", 0o600),
        ("private", 0o700),
        ("open", 0o777),
        ("a", 0o750),
        ("a/b", 0o750),
        ("a/b/c", 0o750),
        ("unread", 0o300),
        ("shared", 0o1777),
        ("shut", 0o340),
        ("shut/in", 0o340),
        ("shut/in/leaf", 0o040),
        ("keeper.lock", 0o600),
    ] {
        assert_eq!(mode_of(&root.join(name)), mode, "{name} has {:o}", mode_of(&root.join(name)));
    }
    // What its owner cannot read or search, the scratch folder could not be removed around.
    runtime()
        .exec(&format!(
            "for _, name in {{ '/unread', '/shut', '/shut/in', '/shut/in/leaf' }} do \
               assert(fs.chmod({root} .. name, tonumber('700', 8))) \
             end",
            root = scratch.lua()
        ))
        .unwrap();
    let leftovers: Vec<_> =
        std::fs::read_dir(root.join("made")).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(leftovers, ["atomic"], "an atomic write left its temporary");
}

/// A mode is for what a call creates: a file that is already there keeps its bits, except under
/// an atomic write, which always makes the file anew. `chmod` is what changes a file that exists.
#[cfg(unix)]
#[test]
fn a_mode_leaves_a_file_that_exists_alone_and_chmod_changes_it() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("modes-kept");
    let root = &scratch.0;
    // 640 is no umask's default, so an atomic write that keeps it carried it over.
    for (name, mode) in [
        ("kept", 0o640),
        ("stream", 0o640),
        ("lock", 0o640),
        ("atomic", 0o640),
        ("replaced", 0o640),
        ("sealed", 0o400),
        ("special", 0o4750),
    ] {
        std::fs::write(root.join(name), b"old").unwrap();
        std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();
    runtime()
        .exec(&format!(
            "local root = {root} \
             local function bits(text) return tonumber(text, 8) end \
             assert(fs.writeFile(root .. '/kept', 'new', {{ mode = bits('600') }}) == 3) \
             assert(fs.readFileString(root .. '/kept') == 'new', 'the file is still written') \
             local writer = assert(fs.openWrite(root .. '/stream', {{ mode = bits('600') }})) assert(writer:close()) \
             local lock = assert(fs.lock(root .. '/lock', {{ mode = bits('600') }})) assert(lock:unlock()) \
             assert(fs.writeFile(root .. '/atomic', 'new', {{ atomic = true }}) == 3) \
             assert(fs.writeFile(root .. '/sealed', 'new', {{ atomic = true }}) == 3) \
             assert(fs.writeFile(root .. '/special', 'new', {{ atomic = true }}) == 3) \
             assert(fs.stat(root .. '/special').mode == bits('4750'), 'bits beyond the permissions did not carry over') \
             assert(not pcall(fs.writeFile, root .. '/never', 'x', {{ mode = bits('4755') }}), 'a file took set-user-ID') \
             assert(not pcall(fs.openWrite, root .. '/never', {{ mode = bits('1000') }}), 'a file took sticky') \
             assert(fs.writeFile(root .. '/replaced', 'new', {{ atomic = true, mode = bits('600') }}) == 3) \
             local made, message, kind = fs.writeFile(root .. '/dangling', 'x', {{ mode = bits('600') }}) \
             assert(made == nil and kind == 'alreadyExists', tostring(message)) \
             assert(fs.chmod(root .. '/kept', bits('400')) == true and fs.stat(root .. '/kept').mode == bits('400')) \
             local changed, why, what = fs.chmod(root .. '/missing', bits('600')) \
             assert(changed == nil and what == 'notFound' and why:find('dream.fs.chmod'), tostring(why)) \
             for _, mode in {{ -1, 4096, 1.5 }} do \
               assert(not pcall(fs.chmod, root .. '/kept', mode), 'chmod took ' .. mode) \
               assert(not pcall(fs.writeFile, root .. '/never', 'x', {{ mode = mode }}), 'writeFile took ' .. mode) \
               assert(not pcall(fs.mkdir, root .. '/never', {{ mode = mode }}), 'mkdir took ' .. mode) \
             end \
             local ok, err = pcall(fs.writeFile, root .. '/kept', 'x', {{ create = false, mode = bits('600') }}) \
             assert(not ok and err:find('mode needs create'), err) \
             ok, err = pcall(fs.openWrite, root .. '/kept', {{ create = false, mode = bits('600') }}) \
             assert(not ok and err:find('mode needs create'), err)",
            root = scratch.lua()
        ))
        .unwrap();
    assert_eq!(mode_of(&root.join("kept")), 0o400);
    assert_eq!(std::fs::read(root.join("sealed")).unwrap(), b"new");
    for (name, mode) in [
        ("stream", 0o640),
        ("lock", 0o640),
        ("atomic", 0o640),
        ("replaced", 0o600),
        ("sealed", 0o400),
        ("special", 0o4750),
    ] {
        assert_eq!(mode_of(&root.join(name)), mode, "{name} has {:o}", mode_of(&root.join(name)));
    }
    assert!(!root.join("never").exists() && !root.join("nowhere").exists(), "a refused call created something");
}

/// Where there are no permission bits a mode is read, checked and changes nothing, a `stat` has
/// none to report, and `chmod` says so.
#[cfg(not(unix))]
#[test]
fn a_mode_changes_nothing_where_there_are_no_permission_bits() {
    let scratch = Scratch::new("modes-none");
    runtime()
        .exec(&format!(
            "local root = {root} \
             assert(fs.writeFile(root .. '/file', 'x', {{ mode = 384 }}) == 1) \
             assert(fs.writeFile(root .. '/atomic', 'x', {{ atomic = true, mode = 384 }}) == 1) \
             assert(fs.mkdir(root .. '/dir', {{ mode = 448 }})) \
             local writer = assert(fs.openWrite(root .. '/stream', {{ mode = 384 }})) assert(writer:close()) \
             local lock = assert(fs.lock(root .. '/lock', {{ mode = 384 }})) assert(lock:unlock()) \
             assert(fs.stat(root .. '/file').mode == nil and fs.readFileString(root .. '/atomic') == 'x') \
             local changed, message, kind = fs.chmod(root .. '/file', 384) \
             assert(changed == nil and kind == 'unsupported' and message:find('dream.fs.chmod'), tostring(message)) \
             assert(not pcall(fs.writeFile, root .. '/never', 'x', {{ mode = 4096 }}), 'an unchecked mode')",
            root = scratch.lua()
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn fifos_and_devices_are_refused_instead_of_blocking_or_never_ending() {
    let scratch = Scratch::new("fifo");
    let fifo = scratch.0.join("pipe");
    let name = std::ffi::CString::new(fifo.display().to_string()).unwrap();
    // SAFETY: a NUL-terminated path in a folder this test owns.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    runtime()
        .exec(&format!(
            "local root = {} \
             for _, read in {{ fs.readFile, fs.readFileString, fs.open, fs.openPositional }} do \
               local value, message, kind = read(root .. '/pipe') \
               assert(value == nil and kind == 'invalidInput' and message:find('not a regular file'), message) \
             end \
             local value, message, kind = fs.readFile('/dev/zero') \
             assert(value == nil and kind == 'invalidInput', message)",
            scratch.lua()
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn directory_barriers_refuse_a_fifo_instead_of_blocking() {
    let scratch = Scratch::new("fifo-directory");
    let fifo = scratch.0.join("pipe");
    let name = std::ffi::CString::new(fifo.display().to_string()).unwrap();
    // SAFETY: a NUL-terminated path in a folder this test owns.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    runtime()
        .exec(&format!(
            "local root = {} \
             local value, message, kind = fs.syncDirectory(root .. '/pipe') \
             assert(value == nil and kind == 'notADirectory' and message:find('syncDirectory'), message) \
             value, message, kind = fs.createDurable(root .. '/pipe/file', 'data') \
             assert(value == nil and kind == 'notADirectory' and message:find('createDurable'), message)",
            scratch.lua()
        ))
        .unwrap();
}

#[test]
fn locks_are_the_operating_systems_and_end_with_their_holder() {
    let scratch = Scratch::new("lock");
    let runtime = runtime();
    runtime
        .exec(&format!(
            "local path = {} .. '/state/owner.lock' \
             local first = assert(fs.lock(path, {{ createParents = true }})) \
             assert(first:held() and not first:shared() and tostring(first):find('exclusive')) \
             local second, message, kind = fs.lock(path) \
             assert(second == nil and kind == 'wouldBlock' and message:find('held by another lock'), message) \
             assert(fs.lock(path, {{ shared = true }}) == nil, 'a shared lock waits for an exclusive one') \
             assert(first:unlock() and not first:held() and not first:unlock(), 'unlocking twice is harmless') \
             local reader = assert(fs.lock(path, {{ shared = true }})) \
             local another = assert(fs.lock(path, {{ shared = true }}), 'shared locks share') \
             assert(reader:shared() and fs.lock(path) == nil, 'an exclusive lock waits for shared ones') \
             reader:unlock() another:unlock() \
             held = assert(fs.lock(path))",
            scratch.lua()
        ))
        .unwrap();

    // The lock is the operating system's: anything else asking it for the file sees it held.
    let file = std::fs::OpenOptions::new().read(true).write(true).open(scratch.0.join("state/owner.lock")).unwrap();
    assert!(matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    runtime.shutdown();
    // The process's exit would reclaim the heap; nothing is dropped.
    std::mem::forget(runtime);
    assert!(file.try_lock().is_ok(), "shutdown releases a lock a script never unlocked");
}

#[test]
fn shutdown_flushes_writers_a_script_never_closed() {
    let scratch = Scratch::new("shutdown");
    let runtime = runtime();
    runtime
        .exec(&format!(
            "kept = fs.openWrite({} .. '/kept.txt') kept:write(string.rep('x', 100)) \
             local dropped = fs.openWrite({} .. '/dropped.txt') dropped:write('y')",
            scratch.lua(),
            scratch.lua()
        ))
        .unwrap();
    assert_eq!(std::fs::read(scratch.0.join("kept.txt")).unwrap().len(), 0, "the writer buffers");
    runtime.shutdown();
    // The process's exit would reclaim the heap; nothing is dropped.
    std::mem::forget(runtime);
    assert_eq!(std::fs::read(scratch.0.join("kept.txt")).unwrap(), vec![b'x'; 100]);
    assert_eq!(std::fs::read(scratch.0.join("dropped.txt")).unwrap(), b"y");
}

#[test]
fn paths_join_and_come_apart_without_a_capability() {
    // Windows' separator is a backslash, which a Luau string literal needs escaped.
    let separator = std::path::MAIN_SEPARATOR.escape_default();
    runtime_with(&[])
        .exec(&format!(
            "local sep = '{separator}' \
             assert(fs.join('a', 'b', 'c.txt') == 'a' .. sep .. 'b' .. sep .. 'c.txt') \
             assert(fs.join() == '' and fs.join('a') == 'a') \
             assert(fs.parent('a/b/c.txt') == 'a/b' and fs.parent('c.txt') == '' and fs.parent('') == nil) \
             assert(fs.fileName('a/b/c.txt') == 'c.txt' and fs.fileName('a/..') == nil) \
             assert(fs.extension('a/b.tar.gz') == 'gz' and fs.extension('.hidden') == nil and fs.extension('x') == nil) \
             assert(fs.isFileName('Morrowind.esm') and not fs.isFileName('a/b') and not fs.isFileName('..') \
               and not fs.isFileName('') and not fs.isFileName('/abs')) \
             local ok, err = pcall(fs.isFile, 'x') assert(not ok and err:find('filesystem.read'), err)"
        ))
        .unwrap();
    #[cfg(unix)]
    runtime_with(&[]).exec("assert(fs.join('a', '/abs', 'b') == '/abs/b')").unwrap();
}

#[test]
fn files_are_written_exclusively_atomically_and_under_new_parents() {
    let scratch = Scratch::new("write-modes");
    runtime()
        .exec(&format!(
            "local root = {} \
             assert(fs.isDir(root) and not fs.isFile(root) and not fs.isDir(root .. '/missing')) \
             assert(fs.writeFile(root .. '/new/deep/file.txt', 'one', {{ createParents = true }}) == 3) \
             assert(fs.isFile(root .. '/new/deep/file.txt')) \
             local count, message, kind = fs.writeFile(root .. '/new/deep/file.txt', 'two', {{ exclusive = true }}) \
             assert(count == nil and kind == 'alreadyExists', message) \
             assert(fs.writeFile(root .. '/new/deep/fresh.txt', 'two', {{ exclusive = true }}) == 3) \
             assert(fs.writeFile(root .. '/new/deep/file.txt', 'replaced', {{ atomic = true }}) == 8) \
             assert(fs.readFileString(root .. '/new/deep/file.txt') == 'replaced') \
             assert(fs.writeFile(root .. '/made/atomic.txt', 'x', {{ atomic = true, createParents = true }}) == 1) \
             local names = fs.list(root .. '/new/deep') \
             assert(#names == 2 and names[1] == 'file.txt' and names[2] == 'fresh.txt', table.concat(names, ',')) \
             for _, bad in {{ {{ atomic = true, append = true }}, {{ atomic = true, offset = 0 }}, {{ exclusive = true, create = false }} }} do \
               assert(not pcall(fs.writeFile, root .. '/x', 'y', bad)) \
             end",
            scratch.lua()
        ))
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch.0.join("kept-mode");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        runtime()
            .exec(&format!("assert(fs.writeFile({} .. '/kept-mode', 'new', {{ atomic = true }}))", scratch.lua()))
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn durable_file_and_directory_operations() {
    let scratch = Scratch::new("durable");
    runtime().exec(&format!(
        "local root = {root} local path = root .. '/journal' \
         local w = assert(fs.openWrite(path, {{ exclusive = true }})) \
         assert(w:write('first') == 5) assert(w:syncAll()) assert(w:write('!') == 1) assert(w:syncData()) assert(w:close()) \
         assert(fs.syncDirectory(root)) assert(fs.readFileString(path) == 'first!') \
         local duplicate, message, kind = fs.openWrite(path, {{ exclusive = true }}) \
         assert(duplicate == nil and kind == 'alreadyExists' and message:find('openWrite')) \
         w = assert(fs.openWrite(path, {{ append = true }})) assert(w:write('more') == 4) assert(w:syncData()) assert(w:close()) \
         assert(fs.readFileString(path) == 'first!more') \
         w = assert(fs.openWrite(path, {{ truncate = false }})) assert(w:truncate(3)) assert(w:syncAll()) assert(w:close()) \
         assert(fs.readFileString(path) == 'fir') \
         local tmp = root .. '/temp' w = assert(fs.openWrite(tmp, {{ exclusive = true }})) \
         assert(w:write('replacement') == 11) assert(w:syncAll()) assert(w:close()) \
         assert(fs.replace(tmp, path)) assert(fs.syncDirectory(root)) assert(fs.readFileString(path) == 'replacement') \
         local ok, err = pcall(w.syncAll, w) assert(not ok and err:find('closed')) \
         local failed, msg, code = fs.replace(tmp, path) assert(failed == nil and code == 'notFound' and msg:find('replace')) \
         assert(fs.readFileString(path) == 'replacement') \
         failed, msg, code = fs.syncDirectory(root .. '/missing') assert(failed == nil and code == 'notFound' and msg:find('syncDirectory'))",
        root = scratch.lua()
    )).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn failed_sync_is_reported_again_by_close() {
    runtime()
        .exec(
            "local w = assert(fs.openWrite('/dev/full', { truncate = false })) \
         assert(w:write('x') == 1) \
         local ok, message, kind = w:syncAll() \
         assert(ok == nil and kind == 'storageFull' and message:find('Writer.syncAll') and message:find('os error')) \
         for _, action in {function() return w:write('later') end, function() return w:writeAt(0, 'later') end, \
             function() return w:seek(0) end, function() return w:truncate(0) end, \
             function() return w:flush() end, function() return w:syncData() end, function() return w:syncAll() end} do \
             local value, why, category = action() assert(value == nil and why == message and category == kind) end \
         local closed, closeMessage, closeKind = w:close() \
         assert(closed == nil and closeKind == kind and closeMessage == message) \
         closed, closeMessage, closeKind = w:close() \
         assert(closed == nil and closeKind == kind and closeMessage == message)",
        )
        .unwrap();
}

// Invoked in a separate test process by the parent below, which kills it after a named
// boundary. The marker is deliberately outside the protocol under test.
#[cfg(any(unix, windows))]
#[test]
fn durability_process_child() {
    let Ok(root) = std::env::var("L3I_DURABILITY_CHILD_DIR") else { return };
    let boundary = std::env::var("L3I_DURABILITY_CHILD_BOUNDARY").unwrap();
    if boundary == "verify_framed" {
        let path = std::path::Path::new(&root).join("stream");
        let recovered = fixture_recover(&std::fs::read(path).unwrap()).unwrap();
        let count: u64 = std::env::var("L3I_DURABILITY_COUNT").unwrap().parse().unwrap();
        let torn = std::env::var("L3I_DURABILITY_TORN").unwrap() == "true";
        assert_eq!(recovered, ((1..=count).collect(), torn));
        return;
    }
    let root = format!("{root:?}");
    if boundary == "verify" {
        let expected = std::env::var("L3I_DURABILITY_EXPECTED").unwrap();
        runtime().exec(&format!("assert(fs.readFileString({root} .. '/journal') == {expected:?})")).unwrap();
        return;
    }
    if boundary == "relative_paths" {
        runtime()
            .exec(
                "assert(fs.mkdir('data')) assert(fs.createDurable('data/matrix.db', 'nested') == 6) \
                 assert(fs.createDurable('bare', 'bare') == 4) assert(fs.syncDirectory('.')) \
                 assert(fs.syncDirectory('data')) assert(fs.replace('bare', 'data/matrix.db')) \
                 assert(fs.syncDirectory('data')) assert(fs.readFileString('data/matrix.db') == 'bare') \
                 assert(not fs.exists('bare'))",
            )
            .unwrap();
        std::fs::write("done", b"done").unwrap();
        return;
    }
    #[cfg(windows)]
    if boundary == "ordinary_user" {
        let probe = std::path::Path::new(r"C:\Windows\l3i-restricted-token-probe");
        assert!(std::fs::write(probe, b"probe").is_err(), "the child still has administrator rights");
        runtime()
            .exec(&format!(
                "local root = {root} local journal = root .. '/journal' local temp = root .. '/temp' \
                 assert(fs.createDurable(journal, 'header') == 6) assert(fs.syncDirectory(root)) \
                 local w = assert(fs.openWrite(temp, {{ exclusive = true }})) \
                 assert(w:write('replacement') == 11) assert(w:syncAll()) assert(w:close()) \
                 assert(fs.replace(temp, journal)) assert(fs.syncDirectory(root)) \
                 assert(fs.readFileString(journal) == 'replacement')"
            ))
            .unwrap();
        let done = std::path::Path::new(&std::env::var("L3I_DURABILITY_CHILD_DIR").unwrap()).join("done");
        std::fs::write(done, b"done").unwrap();
        return;
    }
    let tx = fixture_lua_bytes(&fixture_frame(2, b"transaction two"));
    let checkpoint = fixture_lua_bytes(&fixture_frame(3, b"checkpoint"));
    let marker = fixture_lua_bytes(&fixture_frame(4, b"checkpoint commit"));
    let next = fixture_lua_bytes(&fixture_frame(5, b"transaction three"));
    let script = match boundary.as_str() {
        "single_tx" | "single_checkpoint" | "single_marker" | "single_torn" => {
            let suffix = match boundary.as_str() {
                "single_tx" => "",
                "single_checkpoint" => "assert(held:write(checkpoint) == #checkpoint) assert(held:syncAll())",
                "single_marker" => {
                    "assert(held:write(checkpoint) == #checkpoint) assert(held:syncAll()) assert(held:write(marker) == #marker) assert(held:syncData())"
                }
                "single_torn" => {
                    "assert(held:write(checkpoint) == #checkpoint) assert(held:syncAll()) assert(held:write(marker) == #marker) assert(held:syncData()) assert(held:write(next, 0, #next - 1) == #next - 1) assert(held:flush())"
                }
                _ => unreachable!(),
            };
            format!(
                "local tx, checkpoint, marker, next = {tx}, {checkpoint}, {marker}, {next} \
                held_lock = assert(fs.lock({root} .. '/lock')) \
                held = assert(fs.openWrite({root} .. '/stream', {{ append = true, create = false, writeThrough = true }})) \
                assert(held:write(tx) == #tx) assert(held:syncData()) {suffix}"
            )
        }
        "lock" => format!("held = assert(fs.lock({root} .. '/lock'))"),
        "create" => format!("held = assert(fs.openWrite({root} .. '/journal', {{ exclusive = true }}))"),
        "durable_create" => format!("assert(fs.createDurable({root} .. '/journal', 'header') == 6)"),
        "write" => format!(
            "held = assert(fs.openWrite({root} .. '/journal', {{ exclusive = true }})) assert(held:write('old') == 3)"
        ),
        "file_sync" => format!(
            "held = assert(fs.openWrite({root} .. '/journal', {{ exclusive = true }})) assert(held:write('old') == 3) assert(held:syncAll())"
        ),
        "dir_sync" => format!(
            "held = assert(fs.openWrite({root} .. '/journal', {{ exclusive = true }})) assert(held:write('old') == 3) assert(held:syncAll()) assert(fs.syncDirectory({root}))"
        ),
        "append" => format!(
            "held = assert(fs.openWrite({root} .. '/journal', {{ append = true }})) assert(held:write('new') == 3)"
        ),
        "append_sync" => format!(
            "held = assert(fs.openWrite({root} .. '/journal', {{ append = true }})) assert(held:write('new') == 3) assert(held:syncData())"
        ),
        "temp_write" => format!(
            "held = assert(fs.openWrite({root} .. '/temp', {{ exclusive = true }})) assert(held:write('replacement') == 11)"
        ),
        "temp_sync" => format!(
            "held = assert(fs.openWrite({root} .. '/temp', {{ exclusive = true }})) assert(held:write('replacement') == 11) assert(held:syncAll()) assert(held:close())"
        ),
        "replace" => format!(
            "held = assert(fs.openWrite({root} .. '/temp', {{ exclusive = true }})) assert(held:write('replacement') == 11) assert(held:syncAll()) assert(held:close()) assert(fs.replace({root} .. '/temp', {root} .. '/journal'))"
        ),
        "replace_sync" => format!(
            "held = assert(fs.openWrite({root} .. '/temp', {{ exclusive = true }})) assert(held:write('replacement') == 11) assert(held:syncAll()) assert(held:close()) assert(fs.replace({root} .. '/temp', {root} .. '/journal')) assert(fs.syncDirectory({root}))"
        ),
        _ => panic!("unknown boundary"),
    };
    let runtime = runtime();
    runtime.exec(&script).unwrap();
    std::fs::write(std::path::Path::new(&std::env::var("L3I_DURABILITY_CHILD_DIR").unwrap()).join("ready"), b"ready")
        .unwrap();
    loop {
        std::thread::park();
    }
}

#[cfg(any(unix, windows))]
#[test]
fn killed_processes_leave_reopenable_files_and_release_locks() {
    use std::time::{Duration, Instant};
    for boundary in [
        "create",
        "write",
        "file_sync",
        "dir_sync",
        "durable_create",
        "append",
        "append_sync",
        "temp_write",
        "temp_sync",
        "replace",
        "replace_sync",
        "lock",
    ] {
        let scratch = Scratch::new(&format!("crash-{boundary}"));
        let journal = scratch.0.join("journal");
        if matches!(boundary, "append" | "append_sync" | "temp_write" | "temp_sync" | "replace" | "replace_sync") {
            std::fs::write(&journal, b"old").unwrap();
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fs::durability_process_child", "--nocapture"])
            .env("L3I_DURABILITY_CHILD_DIR", &scratch.0)
            .env("L3I_DURABILITY_CHILD_BOUNDARY", boundary)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !scratch.0.join("ready").exists() {
            assert!(child.try_wait().unwrap().is_none(), "child exited at {boundary}");
            assert!(Instant::now() < deadline, "child timed out at {boundary}");
            std::thread::sleep(Duration::from_millis(5));
        }
        if boundary == "lock" {
            runtime()
                .exec(&format!(
                    "local lock, _, kind = fs.lock({} .. '/lock') assert(lock == nil and kind == 'wouldBlock')",
                    scratch.lua()
                ))
                .unwrap();
        }
        child.kill().unwrap();
        child.wait().unwrap();
        if boundary == "lock" {
            runtime()
                .exec(&format!("local lock = assert(fs.lock({} .. '/lock')) assert(lock:unlock())", scratch.lua()))
                .unwrap();
            assert!(scratch.0.join("lock").exists());
        } else {
            let data = std::fs::read(&journal).unwrap();
            match boundary {
                "file_sync" | "dir_sync" | "temp_write" | "temp_sync" => assert_eq!(data, b"old"),
                "append_sync" => assert_eq!(data, b"oldnew"),
                "durable_create" => assert_eq!(data, b"header"),
                "replace" | "replace_sync" => assert_eq!(data, b"replacement"),
                "create" | "write" => assert!(data.is_empty() || data == b"old"),
                "append" => assert!(data == b"old" || data == b"oldnew"),
                _ => unreachable!(),
            }
            // Reopen in another fresh process, independent of the killed process and this runtime.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fs::durability_process_child", "--nocapture"])
                .env("L3I_DURABILITY_CHILD_DIR", &scratch.0)
                .env("L3I_DURABILITY_CHILD_BOUNDARY", "verify")
                .env("L3I_DURABILITY_EXPECTED", std::str::from_utf8(&data).unwrap())
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "fresh process could not reopen after {boundary}");
        }
    }
}

#[test]
#[ignore = "durability benchmark; writes hundreds of megabytes to the test scratch filesystem"]
fn durability_benchmark() {
    let scratch = Scratch::new("durability-bench");
    let source = include_str!("../benches/durable-fs.jsl")
        .replace("local root = assert((...), 'pass a scratch directory')", &format!("local root = {}", scratch.lua()));
    runtime().exec(&source).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn macos_file_sync_persists_and_remains_usable() {
    let scratch = Scratch::new("macos-sync");
    runtime()
        .exec(&format!(
            "local path = {} .. '/file' local w = assert(fs.openWrite(path)) \
         assert(w:write('a') == 1) assert(w:syncData()) assert(w:write('b') == 1) \
         assert(w:syncAll()) assert(w:close()) assert(fs.readFileString(path) == 'ab')",
            scratch.lua()
        ))
        .unwrap();
}

/// Run under `strace -e inject=fdatasync:error=EIO:when=1`; this tests the real syscall path.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires external fdatasync fault injection"]
fn syscall_sync_failure_reaches_script() {
    let scratch = Scratch::new("syscall-injected-sync");
    runtime().exec(&format!(
        "local w = assert(fs.openWrite({} .. '/journal')) assert(w:write('record') == 6) \
         local ok, message, kind = w:syncData() \
         assert(ok == nil and kind == 'other' and message:find('Writer.syncData') and message:find('os error 5'), message) \
         local closed, closeMessage = w:close() assert(closed == nil and closeMessage == message)", scratch.lua()
    )).unwrap();
}

/// Run with EINTR or a short write injected for `L3I_DURABILITY_FAULT_PATH`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires external write syscall fault injection"]
fn injected_write_is_retried() {
    let path = std::env::var("L3I_DURABILITY_FAULT_PATH").unwrap();
    runtime()
        .exec(&format!(
            "local path = {path:?} local w = assert(fs.openWrite(path)) local data = string.rep('x', 16384) \
         assert(w:write(data) == #data) assert(w:syncAll()) assert(w:close()) \
         assert(fs.readFileString(path) == data)"
        ))
        .unwrap();
}

#[cfg(windows)]
#[test]
fn windows_file_sync_append_and_truncate() {
    let scratch = Scratch::new("windows-sync");
    runtime()
        .exec(&format!(
            "local path = {} .. '/file' local w = assert(fs.openWrite(path, {{ exclusive = true }})) \
         assert(w:write('one') == 3) assert(w:syncAll()) assert(w:close()) \
         w = assert(fs.openWrite(path, {{ append = true }})) assert(w:write('two') == 3) \
         assert(w:syncData()) assert(w:close()) assert(fs.readFileString(path) == 'onetwo') \
         w = assert(fs.openWrite(path, {{ truncate = false }})) assert(w:truncate(3)) \
         assert(w:syncAll()) assert(w:close()) assert(fs.readFileString(path) == 'one')",
            scratch.lua()
        ))
        .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn failed_close_remains_failed_on_repeated_close() {
    runtime()
        .exec(
            "local w = assert(fs.openWrite('/dev/full', { truncate = false })) assert(w:write('x') == 1) \
         local ok, message, kind = w:close() assert(ok == nil and kind == 'storageFull') \
         local again, againMessage, againKind = w:close() \
         assert(again == nil and againMessage == message and againKind == kind)",
        )
        .unwrap();
}

// A child, because the working directory belongs to the whole process and tests run in parallel.
#[cfg(any(unix, windows))]
#[test]
fn namespace_barriers_accept_relative_paths() {
    let scratch = Scratch::new("namespace-relative");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fs::durability_process_child", "--nocapture"])
        .current_dir(&scratch.0)
        .env("L3I_DURABILITY_CHILD_DIR", &scratch.0)
        .env("L3I_DURABILITY_CHILD_BOUNDARY", "relative_paths")
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
    // A filter that matched no test would also exit successfully.
    assert!(scratch.0.join("done").exists());
    assert_eq!(std::fs::read(scratch.0.join("data/matrix.db")).unwrap(), b"bare");
}

/// Through a junction on Windows, whose volume checks must describe where the handle landed.
#[cfg(any(unix, windows))]
#[test]
fn namespace_barriers_work_through_a_directory_link() {
    let scratch = Scratch::new("namespace-link");
    let (real, link) = (scratch.0.join("real"), scratch.0.join("link"));
    std::fs::create_dir(&real).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&real)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(made.success(), "{made}");
    }
    runtime()
        .exec(&format!(
            "local link = {} .. '/link' assert(fs.createDurable(link .. '/file', 'one') == 3) \
             assert(fs.syncDirectory(link)) assert(fs.writeFile(link .. '/next', 'two') == 3) \
             assert(fs.replace(link .. '/next', link .. '/file')) assert(fs.syncDirectory(link))",
            scratch.lua()
        ))
        .unwrap();
    assert_eq!(std::fs::read(real.join("file")).unwrap(), b"two");
    assert!(!real.join("next").exists());
}

#[cfg(windows)]
#[path = "support/windows_restricted.rs"]
mod windows_restricted;

#[cfg(windows)]
#[test]
fn namespace_barriers_work_without_administrator_rights() {
    let scratch = Scratch::new("namespace-ordinary-user");
    let code = windows_restricted::run_test(
        "fs::durability_process_child",
        &scratch.0,
        &[
            ("L3I_DURABILITY_CHILD_DIR", scratch.0.as_os_str()),
            ("L3I_DURABILITY_CHILD_BOUNDARY", std::ffi::OsStr::new("ordinary_user")),
        ],
    );
    assert_eq!(code, 0);
    assert!(scratch.0.join("done").exists());
    assert_eq!(std::fs::read(scratch.0.join("journal")).unwrap(), b"replacement");
}

#[cfg(any(unix, windows))]
#[test]
fn namespace_barriers_leave_nothing_behind_and_refuse_a_file() {
    let scratch = Scratch::new("namespace-barriers");
    runtime()
        .exec(&format!(
            "local root = {} local source = root .. '/source' local target = root .. '/target' \
         assert(fs.createDurable(target, 'old') == 3) \
         local names = assert(fs.list(root)) assert(#names == 1 and names[1] == 'target') \
         local ok, message, kind = fs.createDurable(target, 'overwrite') \
         assert(ok == nil and kind == 'alreadyExists' and fs.readFileString(target) == 'old') \
         names = assert(fs.list(root)) assert(#names == 1, 'a refused creation left a temporary') \
         ok, message, kind = fs.syncDirectory(target) \
         assert(ok == nil and kind == 'notADirectory' and message:find('syncDirectory')) \
         assert(fs.syncDirectory(root)) \
         assert(fs.writeFile(source, 'new') == 3) \
         local held = assert(fs.openPositional(target)) \
         ok, message = fs.replace(source, target) \
         if ok then \
           assert(fs.readFileString(target) == 'new' and not fs.exists(source)) \
         else \
           assert(message:find('replace') and fs.readFileString(target) == 'old') \
           held:close() assert(fs.replace(source, target)) assert(fs.readFileString(target) == 'new') \
         end \
         held:close() assert(fs.syncDirectory(root))",
            scratch.lua()
        ))
        .unwrap();
}

// A deliberately small test protocol, not a database format. Its frames are length, sequence,
// payload and CRC-32; recovery tolerates only an incomplete trailing frame.
#[cfg(any(unix, windows))]
fn fixture_crc(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(any(unix, windows))]
fn fixture_frame(sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = sequence.to_le_bytes().to_vec();
    body.extend_from_slice(payload);
    let mut frame = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&fixture_crc(&body).to_le_bytes());
    frame
}

#[cfg(any(unix, windows))]
fn fixture_recover(mut bytes: &[u8]) -> std::result::Result<(Vec<u64>, bool), &'static str> {
    let mut sequences = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Ok((sequences, true));
        }
        let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        if !(8..=4096).contains(&length) {
            return Err("invalid length");
        }
        if bytes.len() < length + 8 {
            return Ok((sequences, true));
        }
        let body = &bytes[4..4 + length];
        let expected_crc = u32::from_le_bytes(bytes[4 + length..8 + length].try_into().unwrap());
        if fixture_crc(body) != expected_crc {
            return Err("checksum");
        }
        let sequence = u64::from_le_bytes(body[..8].try_into().unwrap());
        if sequence != u64::try_from(sequences.len()).unwrap() + 1 {
            return Err("sequence");
        }
        sequences.push(sequence);
        bytes = &bytes[8 + length..];
    }
    Ok((sequences, false))
}

#[cfg(any(unix, windows))]
fn fixture_lua_bytes(bytes: &[u8]) -> String {
    let numbers = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("string.char({numbers})")
}

#[cfg(any(unix, windows))]
#[test]
fn framed_checkpoint_fixture_handles_interrupted_operations() {
    let first = fixture_frame(1, b"first");
    let mut second = first.clone();
    second.extend(fixture_frame(2, b"second"));
    let mut third = second.clone();
    third.extend(fixture_frame(3, b"third"));
    assert_eq!(fixture_recover(&third), Ok((vec![1, 2, 3], false)));
    assert_eq!(fixture_recover(&third[..third.len() - 1]), Ok((vec![1, 2], true)));
    let mut corrupt = third.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(fixture_recover(&corrupt), Err("checksum"));
    let mut gap = first.clone();
    gap.extend(fixture_frame(3, b"out of order"));
    assert_eq!(fixture_recover(&gap), Err("sequence"));

    // Stop after each operation of a second checkpoint. The first checkpoint already replaced
    // the original and was synchronized, so this covers two consecutive generations.
    for phase in 0..=5 {
        let scratch = Scratch::new(&format!("framed-checkpoint-{phase}"));
        let script = format!(
            "local root = {root} local path = root .. '/journal' local temp = root .. '/temp' \
             local first, second, third = {first}, {second}, {third} \
             local w = assert(fs.openWrite(path, {{ exclusive = true }})) \
             assert(w:write(first) == #first) assert(w:syncAll()) assert(w:close()) assert(fs.syncDirectory(root)) \
             w = assert(fs.openWrite(temp, {{ exclusive = true }})) assert(w:write(second) == #second) \
             assert(w:syncAll()) assert(w:close()) assert(fs.replace(temp, path)) assert(fs.syncDirectory(root)) \
             w = assert(fs.openWrite(temp, {{ exclusive = true }})) if {phase} == 0 then return end \
             local half = math.floor(#third / 2) assert(w:write(third, 0, half) == half) assert(w:flush()) \
             if {phase} == 1 then return end \
             assert(w:write(third, half) == #third - half) assert(w:syncAll()) if {phase} == 2 then return end \
             assert(w:close()) if {phase} == 3 then return end \
             local failed, _, kind = fs.replace(temp, root .. '/missing/destination') \
             assert(failed == nil and kind == 'notFound') assert(fs.readFileString(path) == second) \
             assert(fs.replace(temp, path)) if {phase} == 4 then \
                 local ok = fs.syncDirectory(root .. '/missing') assert(ok == nil) return end \
             assert(fs.syncDirectory(root))",
            root = scratch.lua(),
            first = fixture_lua_bytes(&first),
            second = fixture_lua_bytes(&second),
            third = fixture_lua_bytes(&third),
        );
        runtime().exec(&script).unwrap();
        let recovered = fixture_recover(&std::fs::read(scratch.0.join("journal")).unwrap()).unwrap();
        let expected = if phase < 4 { vec![1, 2] } else { vec![1, 2, 3] };
        assert_eq!(recovered, (expected, false), "phase {phase}");
    }
}

#[cfg(unix)]
#[test]
fn framed_journal_fixture_handles_interrupted_truncation() {
    let mut complete = fixture_frame(1, b"first");
    complete.extend(fixture_frame(2, b"second"));
    let mut torn = fixture_frame(3, b"third");
    torn.pop();
    for phase in 0..=2 {
        let scratch = Scratch::new(&format!("framed-truncation-{phase}"));
        let script = format!(
            "local path = {root} .. '/journal' local complete, torn = {complete}, {torn} \
             local w = assert(fs.openWrite(path, {{ exclusive = true }})) assert(w:write(complete) == #complete) \
             assert(w:syncAll()) assert(w:close()) \
             w = assert(fs.openWrite(path, {{ append = true }})) assert(w:write(torn) == #torn) \
             assert(w:syncData()) assert(w:close()) if {phase} == 0 then return end \
             w = assert(fs.openWrite(path, {{ truncate = false }})) assert(w:truncate(#complete)) \
             if {phase} == 2 then assert(w:syncAll()) end assert(w:close())",
            root = scratch.lua(),
            complete = fixture_lua_bytes(&complete),
            torn = fixture_lua_bytes(&torn),
        );
        runtime().exec(&script).unwrap();
        let recovered = fixture_recover(&std::fs::read(scratch.0.join("journal")).unwrap()).unwrap();
        assert_eq!(recovered, (vec![1, 2], phase == 0), "phase {phase}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apfs_checkpoint_directory_sync_is_exercised_on_native_ci() {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let scratch = Scratch::new("apfs-checkpoint");
    let name = CString::new(scratch.0.as_os_str().as_bytes()).unwrap();
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: name is NUL-terminated, and stats points to writable storage for one statfs.
    let result = unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) };
    assert_eq!(result, 0, "statfs: {}", std::io::Error::last_os_error());
    // SAFETY: successful statfs initialized the entire output structure.
    let stats = unsafe { stats.assume_init() };
    // SAFETY: statfs defines f_fstypename as a NUL-terminated filesystem type name.
    let filesystem = unsafe { CStr::from_ptr(stats.f_fstypename.as_ptr()) }.to_str().unwrap();
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        assert_eq!(filesystem, "apfs", "native CI must exercise APFS");
    }
    runtime()
        .exec(&format!(
            "local root = {} local path = root .. '/current' local temp = root .. '/next' \
         local w = assert(fs.openWrite(path, {{ exclusive = true }})) assert(w:write('old') == 3) \
         assert(w:syncAll()) assert(w:close()) assert(fs.syncDirectory(root)) \
         w = assert(fs.openWrite(temp, {{ exclusive = true }})) assert(w:write('new') == 3) \
         assert(w:syncAll()) assert(w:close()) assert(fs.replace(temp, path)) \
         assert(fs.syncDirectory(root)) assert(fs.readFileString(path) == 'new')",
            scratch.lua()
        ))
        .unwrap();
}

// This is an explicit failure model, not a claim about any particular filesystem. A successful
// file barrier means the new image is complete; before the namespace barrier, recovery must
// accept either the old or new name binding. Initial namespace persistence is an assumption.
#[cfg(any(unix, windows))]
#[test]
fn checkpoint_failure_model_validates_complete_generations() {
    let old = fixture_frame(1, b"old checkpoint");
    let mut new = old.clone();
    new.extend(fixture_frame(2, b"new checkpoint"));
    let valid_checkpoint = |bytes: &[u8]| -> std::result::Result<Vec<u64>, &'static str> {
        let (sequences, torn) = fixture_recover(bytes)?;
        if torn {
            return Err("incomplete checkpoint");
        }
        Ok(sequences)
    };

    // A failed or interrupted namespace barrier may leave either complete generation visible.
    for (image, expected) in [(&old, vec![1]), (&new, vec![1, 2])] {
        assert_eq!(valid_checkpoint(image), Ok(expected));
    }
    // If file data was not durably synchronized, a newly visible torn or damaged image must
    // never be mistaken for a completed checkpoint, even if its older prefix is valid.
    assert_eq!(valid_checkpoint(&new[..new.len() - 1]), Err("incomplete checkpoint"));
    let mut corrupt = new.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(valid_checkpoint(&corrupt), Err("checksum"));

    // The new image is acceptable only after it has been completely validated.
    assert_eq!(valid_checkpoint(&new), Ok(vec![1, 2]));
}

#[cfg(any(unix, windows))]
#[test]
fn durable_create_is_exclusive_and_initializes_contents() {
    let scratch = Scratch::new("durable-create");
    runtime()
        .exec(&format!(
            "local root = {} local path = root .. '/journal' \
         assert(fs.createDurable(path, 'header') == 6) assert(fs.readFileString(path) == 'header') \
         local ok, message, kind = fs.createDurable(path, 'overwrite') \
         assert(ok == nil and kind == 'alreadyExists' and message:find('createDurable')) \
         assert(fs.readFileString(path) == 'header') \
         local w = assert(fs.openWrite(path, {{ append = true, create = false }})) \
         assert(w:write('record') == 6) assert(w:syncData()) assert(w:close()) \
         assert(fs.readFileString(path) == 'headerrecord') \
         ok, message, kind = fs.createDurable(root .. '/missing/file', 'x') \
         assert(ok == nil and kind == 'notFound' and message:find('createDurable'))",
            scratch.lua()
        ))
        .unwrap();
}

#[cfg(not(any(unix, windows)))]
#[test]
fn durable_create_fails_before_creating_when_directory_sync_is_unsupported() {
    let scratch = Scratch::new("durable-create-unsupported");
    runtime()
        .exec(&format!(
            "local path = {} .. '/journal' local ok, message, kind = fs.createDurable(path, 'header') \
         assert(ok == nil and kind == 'unsupported' and message:find('createDurable')) \
         assert(not fs.exists(path))",
            scratch.lua()
        ))
        .unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn write_through_writer_remains_usable_after_sync() {
    let scratch = Scratch::new("write-through");
    runtime().exec(&format!(
        "local path = {} .. '/stream' local w = assert(fs.openWrite(path, {{ exclusive = true, writeThrough = true }})) \
         assert(w:write('one') == 3) assert(w:syncAll()) assert(w:write('two') == 3) \
         assert(w:syncData()) assert(w:close()) assert(fs.readFileString(path) == 'onetwo')",
        scratch.lua()
    )).unwrap();
}

// A protocol fixture over the general filesystem API. Windows uses an already provisioned file:
// its first durable name publication remains unsupported, so this test makes no bootstrap claim.
#[cfg(any(unix, windows))]
#[test]
fn retained_single_file_checkpoint_fixture() {
    let records = [
        fixture_frame(1, b"transaction one"),
        fixture_frame(2, b"transaction two"),
        fixture_frame(3, b"checkpoint"),
        fixture_frame(4, b"checkpoint commit"),
        fixture_frame(5, b"transaction three"),
    ];
    for phase in 0..=4 {
        let scratch = Scratch::new(&format!("single-file-{phase}"));
        let path = scratch.0.join("stream");
        let script = format!(
            "local root = {root} local path = root .. '/stream' local first = {first} \
             local tx = {tx} local checkpoint = {checkpoint} local marker = {marker} local last = {last} \
             local lock = assert(fs.lock(root .. '/lock')) assert(fs.createDurable(path, first) == #first) \
             local w = assert(fs.openWrite(path, {{ append = true, create = false, writeThrough = true }})) \
             if {phase} >= 1 then assert(w:write(tx) == #tx) assert(w:syncData()) end \
             if {phase} >= 2 then assert(w:write(checkpoint) == #checkpoint) assert(w:syncAll()) end \
             if {phase} >= 3 then assert(w:write(marker) == #marker) assert(w:syncData()) end \
             if {phase} >= 4 then assert(w:write(last) == #last) assert(w:syncData()) end \
             assert(w:close()) assert(lock:unlock())",
            root = scratch.lua(),
            first = fixture_lua_bytes(&records[0]),
            tx = fixture_lua_bytes(&records[1]),
            checkpoint = fixture_lua_bytes(&records[2]),
            marker = fixture_lua_bytes(&records[3]),
            last = fixture_lua_bytes(&records[4]),
        );
        runtime().exec(&script).unwrap();
        let expected: Vec<u8> = records[..=phase].iter().flatten().copied().collect();
        let read = std::fs::read(&path).unwrap();
        assert_eq!(read, expected, "phase {phase}");
        let recovered = fixture_recover(&read).unwrap();
        assert_eq!(recovered, ((1..=u64::try_from(phase + 1).unwrap()).collect(), false));
        let expected_lua = fixture_lua_bytes(&expected);
        runtime()
            .exec(&format!(
                "local path = {} .. '/stream' local expected = {expected_lua} \
             assert(buffer.tostring(fs.readAt(path, 0, #expected)) == expected)",
                scratch.lua()
            ))
            .unwrap();
    }
}

// Process termination exercises record boundaries without treating a process kill as power loss.
#[cfg(any(unix, windows))]
#[test]
fn retained_single_file_survives_process_kills_at_record_boundaries() {
    use std::time::{Duration, Instant};
    for (boundary, count, torn) in [
        ("single_tx", 2, false),
        ("single_checkpoint", 3, false),
        ("single_marker", 4, false),
        ("single_torn", 4, true),
    ] {
        let scratch = Scratch::new(&format!("stream-crash-{boundary}"));
        let first = fixture_frame(1, b"transaction one");
        runtime()
            .exec(&format!(
                "assert(fs.createDurable({} .. '/stream', {}) == {})",
                scratch.lua(),
                fixture_lua_bytes(&first),
                first.len()
            ))
            .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fs::durability_process_child", "--nocapture"])
            .env("L3I_DURABILITY_CHILD_DIR", &scratch.0)
            .env("L3I_DURABILITY_CHILD_BOUNDARY", boundary)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !scratch.0.join("ready").exists() {
            assert!(child.try_wait().unwrap().is_none(), "child exited at {boundary}");
            assert!(Instant::now() < deadline, "child timed out at {boundary}");
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fs::durability_process_child", "--nocapture"])
            .env("L3I_DURABILITY_CHILD_DIR", &scratch.0)
            .env("L3I_DURABILITY_CHILD_BOUNDARY", "verify_framed")
            .env("L3I_DURABILITY_COUNT", count.to_string())
            .env("L3I_DURABILITY_TORN", torn.to_string())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "recovery failed after {boundary}");
    }
}

// This tiny test protocol distinguishes physical frame validity from checkpoint commitment.
// It deliberately belongs to the test suite rather than defining an application record format.
#[cfg(any(unix, windows))]
fn fixture_checkpoint_recover(bytes: &[u8]) -> std::result::Result<(Option<u64>, Vec<u64>, bool), &'static str> {
    let (sequences, torn) = fixture_recover(bytes)?;
    let mut cursor = 0;
    let mut candidate = None;
    let mut committed = None;
    let mut replay = Vec::new();
    for sequence in sequences {
        let length = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        let payload = &bytes[cursor + 12..cursor + 4 + length];
        let Some((&kind, rest)) = payload.split_first() else { return Err("missing record type") };
        match kind {
            b'T' => {
                candidate = None;
                replay.push(sequence);
            }
            b'C' | b'M' => {
                let generation = u64::from_le_bytes(rest.try_into().map_err(|_| "invalid generation")?);
                if kind == b'C' {
                    candidate = Some(generation);
                } else if candidate == Some(generation) {
                    committed = Some(generation);
                    candidate = None;
                    replay.clear();
                } else {
                    return Err("unmatched checkpoint marker");
                }
            }
            _ => return Err("invalid record type"),
        }
        cursor += length + 8;
    }
    Ok((committed, replay, torn))
}

#[cfg(any(unix, windows))]
#[test]
fn checkpoint_recovery_requires_a_valid_commit_marker() {
    let tx = fixture_frame(1, b"Tfirst transaction");
    let checkpoint = fixture_frame(2, &[b'C', 7, 0, 0, 0, 0, 0, 0, 0]);
    let marker = fixture_frame(3, &[b'M', 7, 0, 0, 0, 0, 0, 0, 0]);
    let next_tx = fixture_frame(4, b"Tnext transaction");
    let mut image = tx.clone();
    let mut images = vec![image.clone()];
    image.extend_from_slice(&checkpoint);
    images.push(image.clone());
    let mut partial_marker = image.clone();
    partial_marker.extend_from_slice(&marker[..marker.len() - 1]);
    images.push(partial_marker);
    image.extend_from_slice(&marker);
    images.push(image.clone());
    image.extend_from_slice(&next_tx);
    images.push(image);
    let expected = [
        (None, vec![1], false),
        (None, vec![1], false),
        (None, vec![1], true),
        (Some(7), vec![], false),
        (Some(7), vec![4], false),
    ];
    for (phase, (image, expected)) in images.iter().zip(expected).enumerate() {
        let scratch = Scratch::new(&format!("checkpoint-semantics-{phase}"));
        let script = format!(
            "local path = {} .. '/stream' local w = assert(fs.openWrite(path, {{ exclusive = true }})) \
             local image = {} assert(w:write(image) == #image) assert(w:syncAll()) assert(w:close())",
            scratch.lua(),
            fixture_lua_bytes(image),
        );
        runtime().exec(&script).unwrap();
        let reopened = std::fs::read(scratch.0.join("stream")).unwrap();
        assert_eq!(fixture_checkpoint_recover(&reopened), Ok(expected), "phase {phase}");
    }
    let mut wrong_marker = tx.clone();
    wrong_marker.extend_from_slice(&checkpoint);
    wrong_marker.extend(fixture_frame(3, &[b'M', 8, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(fixture_checkpoint_recover(&wrong_marker), Err("unmatched checkpoint marker"));
    let mut damaged = images[3].clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert_eq!(fixture_checkpoint_recover(&damaged), Err("checksum"));
}

#[test]
fn positional_reader_tracks_file_changes_without_mapping() {
    let scratch = Scratch::new("positional-reader");
    runtime()
        .exec(&format!(
            "local path = {} .. '/data' assert(fs.writeFile(path, 'abc') == 3) \
         local missing, refused, kind = fs.openPositional(path .. '.missing') \
         assert(missing == nil and kind == 'notFound' and refused:find('openPositional')) \
         local r = assert(fs.openPositional(path)) assert(r:size() == 3) \
         assert(tostring(r):find('dream.fs.PositionalReader', 1, true)) \
         assert(buffer.tostring(r:readAt(1, 2)) == 'bc') \
         local w = assert(fs.openWrite(path, {{ append = true }})) \
         assert(w:write('def') == 3) assert(w:flush()) assert(r:size() == 6) \
         assert(buffer.tostring(r:readAt(2, 4)) == 'cdef') assert(w:close()) \
         w = assert(fs.openWrite(path, {{ truncate = false }})) assert(w:truncate(2)) assert(w:close()) \
         assert(r:size() == 2 and buffer.tostring(r:readAt(0, 9)) == 'ab') \
         local ok = pcall(r.readAt, r, 3, 1) assert(not ok) r:close() r:close() \
         ok = pcall(r.readAt, r, 0, 1) assert(not ok) ok = pcall(r.size, r) assert(not ok)",
            scratch.lua()
        ))
        .unwrap();
}
