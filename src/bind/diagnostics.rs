//! Argument error messages, word for word from `bindfunction.cpp`.

use std::ffi::c_int;

use crate::error::Error;

pub(crate) fn bad_argument(debug_name: &str, position: c_int, expected: &str, cause: &Error) -> Error {
    Error::runtime(format!("{debug_name}: bad argument #{position} (expected {expected}): {cause}"))
}

pub(crate) fn missing_argument(debug_name: &str, position: c_int, expected: &str) -> Error {
    Error::runtime(format!("{debug_name}: bad argument #{position} (expected {expected}): missing argument"))
}

pub(crate) fn too_few_arguments(debug_name: &str, required: usize, got: c_int) -> Error {
    Error::runtime(format!("{debug_name}: bad argument count (expected at least {required}, got {got})"))
}

pub(crate) fn too_many_arguments(debug_name: &str, maximum: usize, got: c_int) -> Error {
    Error::runtime(format!("{debug_name}: bad argument count (expected at most {maximum}, got {got})"))
}

pub(crate) fn unused_arguments(debug_name: &str) -> Error {
    Error::runtime(format!("{debug_name}: bad argument count (unused arguments)"))
}

pub(crate) fn no_matching_overload(debug_name: &str) -> Error {
    Error::runtime(format!("{debug_name}: no matching overload"))
}
