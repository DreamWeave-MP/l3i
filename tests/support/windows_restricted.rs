//! Runs a test child as an ordinary user would, from any account: a restricted copy of this
//! process's token with Administrators deny-only and every privilege but change-notify removed.
//! CI's Windows runner is an administrator, whose rights would hide an access problem.

use std::collections::BTreeMap;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

type Handle = *mut c_void;

const TOKEN_ASSIGN_PRIMARY: u32 = 0x1;
const TOKEN_DUPLICATE: u32 = 0x2;
const TOKEN_QUERY: u32 = 0x8;
const DISABLE_MAX_PRIVILEGE: u32 = 0x1;
const WIN_BUILTIN_ADMINISTRATORS_SID: i32 = 26;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x400;
const INFINITE: u32 = 0xFFFF_FFFF;

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[repr(C)]
struct StartupInfo {
    size: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    geometry: [u32; 7],
    flags: u32,
    show_window: u16,
    reserved2_size: u16,
    reserved2: *mut u8,
    stdin: Handle,
    stdout: Handle,
    stderr: Handle,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn CreateWellKnownSid(kind: i32, domain: *mut c_void, sid: *mut c_void, size: *mut u32) -> i32;
    fn CreateRestrictedToken(
        token: Handle,
        flags: u32,
        disable_count: u32,
        disable: *const SidAndAttributes,
        delete_count: u32,
        delete: *const c_void,
        restrict_count: u32,
        restrict: *const SidAndAttributes,
        restricted: *mut Handle,
    ) -> i32;
    fn CreateProcessAsUserW(
        token: Handle,
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: i32,
        flags: u32,
        environment: *const c_void,
        directory: *const u16,
        startup: *const StartupInfo,
        information: *mut ProcessInformation,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

fn check(status: i32, what: &str) {
    assert_ne!(status, 0, "{what}: {}", std::io::Error::last_os_error());
}

fn restricted_token() -> Handle {
    let mut token: Handle = std::ptr::null_mut();
    let mut sid = [0_u8; 68];
    let mut sid_size = 68_u32;
    let mut restricted: Handle = std::ptr::null_mut();
    // SAFETY: every out pointer is valid for the call, `sid` holds SECURITY_MAX_SID_SIZE bytes,
    // and the disabled-SID array of one outlives `CreateRestrictedToken`.
    unsafe {
        check(
            OpenProcessToken(GetCurrentProcess(), TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY, &raw mut token),
            "OpenProcessToken",
        );
        check(
            CreateWellKnownSid(
                WIN_BUILTIN_ADMINISTRATORS_SID,
                std::ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &raw mut sid_size,
            ),
            "CreateWellKnownSid",
        );
        let administrators = SidAndAttributes { sid: sid.as_mut_ptr().cast(), attributes: 0 };
        check(
            CreateRestrictedToken(
                token,
                DISABLE_MAX_PRIVILEGE,
                1,
                &raw const administrators,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &raw mut restricted,
            ),
            "CreateRestrictedToken",
        );
        CloseHandle(token);
    }
    restricted
}

/// This test binary's `test`, run under the restricted token with `environment` added; its exit
/// code. `directory` is opened to everyone first, because a scratch folder an elevated parent made
/// may only grant access through the group the child has lost.
pub(super) fn run_test(test: &str, directory: &Path, environment: &[(&str, &OsStr)]) -> u32 {
    let granted = std::process::Command::new("icacls")
        .arg(directory)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(granted.success(), "icacls: {granted}");
    let executable = std::env::current_exe().unwrap();
    let mut command_line = wide(OsStr::new(&format!("\"{}\" --exact {test} --nocapture", executable.display())));
    let mut variables: BTreeMap<_, _> = std::env::vars_os().collect();
    for (name, value) in environment {
        variables.insert(name.into(), (*value).to_owned());
    }
    let mut block: Vec<u16> = Vec::new();
    for (name, value) in &variables {
        block.extend(name.encode_wide().chain([u16::from(b'=')]).chain(value.encode_wide()).chain([0]));
    }
    block.push(0);
    let token = restricted_token();
    // SAFETY: both structures are plain data for which all zeroes means "unset".
    let (mut startup, mut information): (StartupInfo, ProcessInformation) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    startup.size = u32::try_from(size_of::<StartupInfo>()).unwrap();
    let mut code = u32::MAX;
    // SAFETY: the strings are null terminated and mutable where the call requires, the block is
    // double-null terminated UTF-16 as the flag declares, and both handles come back open.
    unsafe {
        check(
            CreateProcessAsUserW(
                token,
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr().cast(),
                std::ptr::null(),
                &raw const startup,
                &raw mut information,
            ),
            "CreateProcessAsUserW",
        );
        WaitForSingleObject(information.process, INFINITE);
        check(GetExitCodeProcess(information.process, &raw mut code), "GetExitCodeProcess");
        CloseHandle(information.thread);
        CloseHandle(information.process);
        CloseHandle(token);
    }
    code
}
