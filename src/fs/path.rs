//! `@dream/fs`'s path functions: taking byte paths apart and joining them, with Rust's `Path`
//! rules and the platform's separator. They touch no disk, so they need no capability.

use std::path::{Component, Path, PathBuf};

use crate::bind::VarArgs;
use crate::error::Result;

use super::{host_path, path_bytes};

pub(crate) fn join(parts: VarArgs<&[u8]>) -> Result<Vec<u8>> {
    let mut joined = PathBuf::new();
    for part in parts.values {
        joined.push(host_path("join", part)?);
    }
    Ok(path_bytes(&joined).to_vec())
}

pub(crate) fn parent(path: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(host_path("parent", path)?.parent().map(|parent| path_bytes(parent).to_vec()))
}

pub(crate) fn file_name(path: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(host_path("fileName", path)?.file_name().map(|name| name.as_encoded_bytes().to_vec()))
}

pub(crate) fn extension(path: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(host_path("extension", path)?.extension().map(|extension| extension.as_encoded_bytes().to_vec()))
}

pub(crate) fn is_file_name(name: &[u8]) -> Result<bool> {
    let name = host_path("isFileName", name)?;
    let mut components = Path::new(&name).components();
    Ok(matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none())
}
