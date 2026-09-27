//! Native lowering of `vector:writef32x3(buffer, offset)` (the codegen half of OpenMW's
//! `nativevectorbuffer.cpp`), written against the Rust [`IrBuilder`].
//!
//! A statement call `v:writef32x3(buffer, offset)` becomes three native f32 buffer stores. Any
//! other shape (results wanted, wrong arity, a non-buffer or non-number argument, an offset that
//! is not an in-range integer) exits to the interpreter, whose `__namecall` shim
//! ([`crate::Runtime::install_vector_buffer_writer`]) applies the exact library semantics.

use super::hooks::{NamecallSite, NativeCodeHooks, NativeContext};
use super::ir::{IrBuilder, IrCmd, bytecode_type};
use crate::raw::ffi::{LUA_TBUFFER, LUA_TNUMBER};

const WRITER_NAME: &str = "writef32x3";
const VECTOR_BYTES: i32 = 3 * size_of::<f32>() as i32;

/// The `writef32x3` lowering; part of the default hook set.
#[derive(Clone, Copy, Debug, Default)]
pub struct VectorBufferWriter;

impl NativeCodeHooks for VectorBufferWriter {
    fn vector_namecall_type(&self, member: &str) -> u8 {
        if member == WRITER_NAME { bytecode_type::NIL } else { bytecode_type::ANY }
    }

    fn vector_namecall(
        &self,
        _: &NativeContext<'_>,
        build: &mut IrBuilder<'_>,
        member: &str,
        site: NamecallSite,
    ) -> bool {
        if member != WRITER_NAME || site.params != 3 || site.results != 0 {
            return false;
        }
        let buffer_location = build.vm_reg(site.arg_res_reg + 2);
        let offset_location = build.vm_reg(site.arg_res_reg + 3);
        let exit = build.vm_exit(site.pcpos);
        build.load_and_check_tag(buffer_location, LUA_TBUFFER as u8, exit);
        build.load_and_check_tag(offset_location, LUA_TNUMBER as u8, exit);

        let buffer = build.inst(IrCmd::LOAD_POINTER, &[buffer_location]);
        let offset_number = build.inst(IrCmd::LOAD_DOUBLE, &[offset_location]);
        let offset = build.inst(IrCmd::NUM_TO_INT, &[offset_number]);
        let zero = build.const_int(0);
        let size = build.const_int(VECTOR_BYTES);
        build.inst(IrCmd::CHECK_BUFFER_LEN, &[buffer, offset, zero, size, offset_number, exit]);

        // EXTRACT_VEC reads a loaded TValue, not a VM register; the caller has already checked
        // the vector tag of the receiver.
        let source = build.vm_reg(site.source_reg);
        let vector = build.inst(IrCmd::LOAD_TVALUE, &[source]);
        let buffer_tag = build.const_tag(LUA_TBUFFER as u8);
        for component in 0..3i32 {
            let index = build.const_int(component);
            let value = build.inst(IrCmd::EXTRACT_VEC, &[vector, index]);
            let destination = if component == 0 {
                offset
            } else {
                let step = build.const_int(component * size_of::<f32>() as i32);
                build.inst(IrCmd::ADD_INT, &[offset, step])
            };
            build.inst(IrCmd::BUFFER_WRITEF32, &[buffer, destination, value, buffer_tag]);
        }
        true
    }
}
