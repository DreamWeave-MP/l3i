//! Lower a validated program into existing JSL/Luau and Jess shader frontends.
//! These strings are compiler artifacts, not a new user-authored expression language.
use crate::Result;
use crate::compute::{Instruction, Program, Shape, error};
use std::fmt::Write;

impl Program {
    fn elementwise_slots(&self) -> Result<(Shape, usize)> {
        let shape = self.output_shape();
        for step in &self.steps {
            match step.op {
                Instruction::MatMul(..)
                | Instruction::SumRows(_)
                | Instruction::Softmax(_)
                | Instruction::Transpose(_)
                | Instruction::Normalize(..) => {
                    return Err(error("backend supports elementwise input slots only"));
                }
                Instruction::Slot(_) if step.shape != shape => {
                    return Err(error("backend requires equal contiguous slot shapes"));
                }
                _ => (),
            }
        }
        Ok((shape, shape.elements()?))
    }
    /// One scalar Luau loop over Luau buffers, compiled by L3i's native code generator. Strict
    /// f32: every add and multiply rounds through a typed `vector` temporary, which the JIT
    /// lowers to a single conversion instruction.
    pub fn native_source(&self) -> Result<String> {
        let (_, n) = self.elementwise_slots()?;
        let mut source = String::from("--!native\nreturn function(inputs, output: buffer)\n");
        writeln!(source, "assert(buffer.len(output) == {})", n * 4).unwrap();
        for slot in 0..self.slots.len() {
            writeln!(
                source,
                "local s{slot}: buffer = inputs[{}]\nassert(buffer.len(s{slot}) == {} and s{slot} ~= output)",
                slot + 1,
                n * 4
            )
            .unwrap();
        }
        writeln!(source, "for i = 0, {} do", i64::try_from(n).expect("bounded") - 1).unwrap();
        for (id, step) in self.steps.iter().enumerate() {
            let value = match step.op {
                Instruction::Slot(slot) => format!("buffer.readf32(s{slot},i*4)"),
                Instruction::Constant(v) => {
                    if v.is_nan() {
                        "(0/0)".into()
                    } else if v == f32::INFINITY {
                        "math.huge".into()
                    } else if v == f32::NEG_INFINITY {
                        "-math.huge".into()
                    } else {
                        format!("{:?}", f64::from(v))
                    }
                }
                Instruction::Add(a, b) => {
                    writeln!(source, "local t{id}: vector = vector.create(v{a}+v{b},0,0)").unwrap();
                    format!("t{id}.x")
                }
                Instruction::Mul(a, b) => {
                    writeln!(source, "local t{id}: vector = vector.create(v{a}*v{b},0,0)").unwrap();
                    format!("t{id}.x")
                }
                Instruction::Relu(a) => format!("if v{a} <= 0 then 0 else v{a}"),
                Instruction::MatMul(..)
                | Instruction::SumRows(_)
                | Instruction::Softmax(_)
                | Instruction::Transpose(_)
                | Instruction::Normalize(..) => {
                    unreachable!("validated lowering subset")
                }
            };
            writeln!(source, "local v{id}: number = {value}").unwrap();
        }
        writeln!(source, "buffer.writef32(output,i*4,v{})\nend\nend", self.steps.len() - 1).unwrap();
        Ok(source)
    }
    /// Restricted exact-integer f32 mode: every input is integral in [-64,64], and static
    /// interval bounds prove every operation stays within exact f32 integer range.
    /// Strict arbitrary f32 is explicitly unsupported by this WGSL adapter.
    pub fn shader_source(&self, mode: &str) -> Result<String> {
        if mode != "integer-exact" {
            return Err(error("unsupported shader numerical mode: use integer-exact"));
        }
        let (_, n) = self.elementwise_slots()?;
        let mut bounds = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            let bound = match step.op {
                Instruction::Slot(_) => 64.0f64,
                Instruction::Constant(v) if v.is_finite() && v.fract() == 0.0 => f64::from(v.abs()),
                Instruction::Add(a, b) => bounds[a] + bounds[b],
                Instruction::Mul(a, b) => bounds[a] * bounds[b],
                Instruction::Relu(a) => bounds[a],
                _ => return Err(error("unsupported shader integer-exact graph")),
            };
            if bound > 16_777_216.0 {
                return Err(error("shader integer-exact range exceeded"));
            }
            bounds.push(bound);
        }
        let mut s = String::from(
            "-- Generated from the validated tensor plan. Do not edit.\nlocal shader=require('@jess/shader')\nreturn function(device)\nlocal program=shader.compute({workgroupSize=64,pass={\n",
        );
        for slot in 0..self.slots.len() {
            writeln!(s, "s{slot}=shader.storage(shader.float),").unwrap();
        }
        s.push_str("out=shader.storage(shader.float,'readWrite'),\n},compute=function(input)\nlocal index=input.builtin.globalInvocationId.x\nlocal p=input.pass\nshader.when(shader.less(index,shader.arrayLength(p.out)),function()\n");
        for (id, step) in self.steps.iter().enumerate() {
            let value = match step.op {
                Instruction::Slot(slot) => format!("p.s{slot}[index]"),
                Instruction::Constant(v) => format!("shader.float({v:?})"),
                Instruction::Add(a, b) => format!("v{a}+v{b}"),
                Instruction::Mul(a, b) => format!("v{a}*v{b}"),
                Instruction::Relu(a) => format!("shader.max(v{a},shader.float(0))"),
                Instruction::MatMul(..)
                | Instruction::SumRows(_)
                | Instruction::Softmax(_)
                | Instruction::Transpose(_)
                | Instruction::Normalize(..) => {
                    unreachable!("validated subset")
                }
            };
            writeln!(s, "local v{id}={value}").unwrap();
        }
        writeln!(
            s,
            "shader.write(p.out,index,v{})\nend)\nend}})\nlocal pipeline=device:pipeline(program)",
            self.steps.len() - 1
        )
        .unwrap();
        // Existing GPU APIs require positive physical allocations. An empty logical tensor
        // retains one padding element, never dispatches and reads back as an empty Lua buffer.
        let bytes = (n * 4).max(4);
        for slot in 0..self.slots.len() {
            writeln!(s, "local g{slot}=device:buffer({bytes},'storage')").unwrap();
        }
        writeln!(s, "local output=device:buffer({bytes},'storage')\nlocal pass=pipeline:bind('pass',{{").unwrap();
        // Jess pass binding order follows sorted declaration keys, not source insertion order:
        // 'out' precedes 's0', 's1', ... . Construct keys in the same lexical order.
        writeln!(s, "[0]=output,").unwrap();
        let mut slots: Vec<usize> = (0..self.slots.len()).collect();
        slots.sort_by_key(|id| format!("s{id}"));
        for (binding, slot) in slots.iter().enumerate() {
            writeln!(s, "[{}]=g{slot},", binding + 1).unwrap();
        }
        s.push_str("})\nreturn function(inputs)\n");
        // Validate every source before enqueueing any writes; failures leave no partial upload.
        for slot in 0..self.slots.len() {
            writeln!(s,"local s{slot}=inputs[{}]\nassert(buffer.len(s{slot})=={},'slot shape mismatch')\nfor i=0,{} do local x=buffer.readf32(s{slot},i*4); assert(x==math.floor(x) and math.abs(x)<=64,'unsupported integer-exact input') end",slot+1,n*4,i64::try_from(n).expect("bounded")-1).unwrap();
        }
        if n == 0 {
            s.push_str("return buffer.create(0)\nend\nend\n");
            return Ok(s);
        }
        for slot in 0..self.slots.len() {
            writeln!(s, "g{slot}:write(0,s{slot})").unwrap();
        }
        writeln!(s,"device:dispatch({{pipeline=pipeline,pass=pass,workgroups={}}})\nreturn output:readLater():wait()\nend\nend",n.div_ceil(64)).unwrap();
        Ok(s)
    }
}
