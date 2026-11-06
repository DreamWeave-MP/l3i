//! Experiment: one generated AVX2 loop per elementwise program, from the same validated steps the
//! planner reads. Research only; nothing here reaches L3i or the research library.
//!
//! Scope is deliberately narrow: contiguous inputs of one shape, a whole number of eight-lane
//! vectors, at most five inputs, and whatever fits sixteen `ymm` registers. Anything else is
//! refused, and a real executor would fall back to the tiled kernels. The output of `compile` is
//! written once and then mapped read and execute, never both.
use l3i::compute::{Instruction, Numeric, Program};

const LANES: usize = 8;
/// Registers holding input base addresses. `rdi` is the index, `rsi` the output, `rdx` the end.
const BASES: [u8; 5] = [8, 9, 10, 11, 0]; // r8, r9, r10, r11, rax
const RDI: u8 = 7;
const RSI: u8 = 6;
const RCX: u8 = 1;

/// What a VEX instruction's last operand is.
#[derive(Clone, Copy)]
enum Rm {
    Register(u8),
    /// `[base + rdi]`, the current vector of an input or the output.
    Indexed(u8),
    /// `[rcx + disp8]`, a constant.
    Constant(u8),
}

struct Assembler(Vec<u8>);
impl Assembler {
    /// Three-byte VEX: `map` 1 is `0F`, 2 is `0F38`; `pp` 0 is none, 1 is `66`.
    fn vex(&mut self, map: u8, pp: u8, wide: bool, opcode: u8, reg: u8, vvvv: u8, rm: Rm) {
        let (x_bit, b_bit) = match rm {
            Rm::Register(r) => (false, r >= 8),
            Rm::Indexed(base) => (false, base >= 8), // The index, rdi, needs no extension.
            Rm::Constant(_) => (false, false),
        };
        let inverted = |bit: bool| u8::from(!bit);
        self.0.push(0xC4);
        self.0.push((inverted(reg >= 8) << 7) | (inverted(x_bit) << 6) | (inverted(b_bit) << 5) | map);
        self.0.push((u8::from(wide) << 7) | ((!vvvv & 0xF) << 3) | (1 << 2) | pp);
        self.0.push(opcode);
        let reg_bits = (reg & 7) << 3;
        match rm {
            Rm::Register(r) => self.0.push(0xC0 | reg_bits | (r & 7)),
            Rm::Indexed(base) => {
                // SIB with index rdi and scale 1; none of the bases needs a displacement.
                self.0.push(reg_bits | 0b100);
                self.0.push((RDI << 3) | (base & 7));
            }
            Rm::Constant(index) => {
                self.0.push(0x40 | reg_bits | RCX);
                self.0.push(index * 4);
            }
        }
    }
    fn load(&mut self, dst: u8, rm: Rm) {
        self.vex(1, 0, false, 0x10, dst, 0, rm); // vmovups
    }
    fn store(&mut self, src: u8, rm: Rm) {
        self.vex(1, 0, false, 0x11, src, 0, rm); // vmovups
    }
    fn copy(&mut self, dst: u8, src: u8) {
        self.vex(1, 0, false, 0x28, dst, 0, Rm::Register(src)); // vmovaps
    }
    fn broadcast(&mut self, dst: u8, constant: u8) {
        self.vex(2, 1, false, 0x18, dst, 0, Rm::Constant(constant)); // vbroadcastss
    }
    fn add(&mut self, dst: u8, a: u8, b: Rm) {
        self.vex(1, 0, false, 0x58, dst, a, b); // vaddps
    }
    fn mul(&mut self, dst: u8, a: u8, b: Rm) {
        self.vex(1, 0, false, 0x59, dst, a, b); // vmulps
    }
    /// `dst = dst * multiplier + addend`, rounding once.
    fn fmadd213(&mut self, dst: u8, multiplier: u8, addend: Rm) {
        self.vex(2, 1, false, 0xA8, dst, multiplier, addend);
    }
    /// `dst = x <= 0 ? +0 : x`: not-less-or-equal keeps NaN, and the mask clears both zeros.
    fn relu(&mut self, dst: u8, x: u8, zero: u8) {
        self.vex(1, 0, false, 0xC2, dst, x, Rm::Register(zero)); // vcmpps
        self.0.push(6); // _CMP_NLE_UQ
        self.vex(1, 0, false, 0x54, dst, dst, Rm::Register(x)); // vandps
    }
    fn zero(&mut self, dst: u8) {
        self.vex(1, 0, false, 0x57, dst, dst, Rm::Register(dst)); // vxorps
    }
}

/// Where a step's value lives while the loop runs.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    Unset,
    /// Read straight from memory at its single use.
    Input(u8),
    Register(u8),
}

/// Sixteen `ymm` registers, some pinned for the whole loop by constants.
struct Registers {
    free: Vec<u8>,
}
impl Registers {
    fn take(&mut self) -> Result<u8, String> {
        self.free.pop().ok_or_else(|| "needs more than sixteen registers".to_string())
    }
}

type Entry = unsafe extern "sysv64" fn(*const *const u8, *mut u8, usize, *const f32);

/// One generated loop, in its own read-and-execute mapping.
pub struct Compiled {
    map: *mut libc::c_void,
    size: usize,
    entry: Entry,
    constants: Vec<f32>,
    pub code_bytes: usize,
}
impl Drop for Compiled {
    fn drop(&mut self) {
        // SAFETY: `map` and `size` are the mapping `compile` made, and nothing runs it now.
        unsafe { libc::munmap(self.map, self.size) };
    }
}
impl Compiled {
    /// Runs the loop. `inputs` are the program's slots in order, each exactly `out.len()` bytes,
    /// which the assertions establish before any generated code reads them.
    pub fn run(&self, inputs: &[&[u8]], out: &mut [u8]) {
        assert!(!out.is_empty() && out.len().is_multiple_of(LANES * 4));
        assert!(inputs.iter().all(|input| input.len() == out.len()));
        let bases: Vec<*const u8> = inputs.iter().map(|input| input.as_ptr()).collect();
        // SAFETY: the code reads each base and writes `out` over `out.len()` bytes, in whole
        // vectors with unaligned accesses, and `compile` required AVX2 and FMA on this host.
        unsafe { (self.entry)(bases.as_ptr(), out.as_mut_ptr(), out.len(), self.constants.as_ptr()) }
    }
}

/// Generates one loop for an elementwise `program`, or says why this prototype refuses it.
pub fn compile(program: &Program, numeric: Numeric) -> Result<Compiled, String> {
    if !(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")) {
        return Err("needs AVX2 and FMA".to_string());
    }
    let steps = program.steps();
    let inputs = program.signature().len();
    if inputs > BASES.len() {
        return Err("too many inputs".to_string());
    }
    // How many times each step is read, and by whom last.
    let mut reads = vec![0usize; steps.len()];
    let mut last_read = vec![0usize; steps.len()];
    for (id, step) in steps.iter().enumerate() {
        let operands = match step.op {
            Instruction::Add(a, b) | Instruction::Mul(a, b) => [Some(a), Some(b)],
            Instruction::Relu(a) => [Some(a), None],
            Instruction::Slot(_) | Instruction::Constant(_) => [None, None],
            Instruction::MatMul(..)
            | Instruction::SumRows(_)
            | Instruction::Softmax(_)
            | Instruction::Transpose(_)
            | Instruction::Normalize(..) => return Err("only elementwise programs generate a loop".to_string()),
        };
        for operand in operands.into_iter().flatten() {
            reads[operand] += 1;
            last_read[operand] = id;
        }
    }
    let fast = numeric == Numeric::Fast;
    let is_add = |id: usize| matches!(steps[id].op, Instruction::Add(..));
    // A product is fused when its only reader is the very next step, and that step is an add.
    let fused_into = |id: usize| {
        fast && matches!(steps[id].op, Instruction::Mul(..))
            && reads[id] == 1
            && last_read[id] == id + 1
            && is_add(id + 1)
    };

    let mut asm = Assembler(Vec::with_capacity(256));
    // Prologue: input bases into their registers, then rdi becomes the index.
    for (index, base) in BASES.iter().enumerate().take(inputs) {
        // mov base, [rdi + disp8]
        asm.0.extend_from_slice(&[0x48 | (u8::from(*base >= 8) << 2), 0x8B, 0x47 | ((base & 7) << 3)]);
        asm.0.push(u8::try_from(index * 8).expect("at most five inputs"));
    }
    asm.0.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi

    let mut registers = Registers { free: (0..16).rev().collect() };
    let mut places = vec![Place::Unset; steps.len()];
    let mut constants: Vec<f32> = Vec::new();
    // Constants and the ReLU zero are pinned for the whole loop, outside it.
    let needs_zero = steps.iter().any(|step| matches!(step.op, Instruction::Relu(_)));
    let zero = if needs_zero {
        let register = registers.take()?;
        asm.zero(register);
        register
    } else {
        0
    };
    let mut pinned = [false; 16];
    if needs_zero {
        pinned[zero as usize] = true;
    }
    for (id, step) in steps.iter().enumerate() {
        if let Instruction::Constant(value) = step.op {
            let register = registers.take()?;
            let index = u8::try_from(constants.len()).map_err(|_| "too many constants".to_string())?;
            if usize::from(index) * 4 > 127 {
                return Err("too many constants".to_string());
            }
            constants.push(value);
            asm.broadcast(register, index);
            places[id] = Place::Register(register);
            pinned[register as usize] = true;
        }
    }
    let loop_start = asm.0.len();
    let mut pending_product: Option<(u8, Rm)> = None;
    for id in 0..steps.len() {
        let rm_of = |place: Place| match place {
            Place::Input(base) => Rm::Indexed(base),
            Place::Register(r) => Rm::Register(r),
            Place::Unset => unreachable!("topological"),
        };
        match steps[id].op {
            Instruction::Constant(_) => {}
            Instruction::Slot(slot) => {
                let base = BASES[slot];
                // A slot read once is a memory operand at that read; otherwise it gets a register.
                if reads[id] <= 1 && id + 1 != steps.len() {
                    places[id] = Place::Input(base);
                } else {
                    let register = registers.take()?;
                    asm.load(register, Rm::Indexed(base));
                    places[id] = Place::Register(register);
                }
            }
            Instruction::Mul(a, b) if fused_into(id) => {
                // Deferred into the next step's fused instruction, reading its multiplicands there.
                let (first, second) = (places[a], places[b]);
                let multiplicand = match (first, second) {
                    (Place::Register(x), other) => {
                        let target = if reads[a] == 1 && !pinned[x as usize] && last_read[a] == id {
                            x
                        } else {
                            let target = registers.take()?;
                            asm.copy(target, x);
                            target
                        };
                        (target, other)
                    }
                    (other, Place::Register(y)) => {
                        let target = if reads[b] == 1 && !pinned[y as usize] && last_read[b] == id {
                            y
                        } else {
                            let target = registers.take()?;
                            asm.copy(target, y);
                            target
                        };
                        (target, other)
                    }
                    (Place::Input(x), other) => {
                        let target = registers.take()?;
                        asm.load(target, Rm::Indexed(x));
                        (target, other)
                    }
                    _ => unreachable!("topological"),
                };
                // The multiplier must be a register for the 213 form.
                let (target, other) = multiplicand;
                let multiplier = match other {
                    Place::Register(r) => r,
                    Place::Input(base) => {
                        let r = registers.take()?;
                        asm.load(r, Rm::Indexed(base));
                        r
                    }
                    Place::Unset => unreachable!("topological"),
                };
                pending_product = Some((target, Rm::Register(multiplier)));
                // Retire the multiplier's register after the add if nothing else reads it.
                places[id] = Place::Register(target);
            }
            Instruction::Add(a, b)
                if pending_product.is_some()
                    && (last_read[a] == id || last_read[b] == id)
                    && (a + 1 == id || b + 1 == id) =>
            {
                let (target, multiplier) = pending_product.take().expect("checked");
                let addend = if a + 1 == id { b } else { a };
                let Rm::Register(multiplier) = multiplier else { unreachable!("always a register") };
                asm.fmadd213(target, multiplier, rm_of(places[addend]));
                places[id] = Place::Register(target);
            }
            Instruction::Add(a, b) | Instruction::Mul(a, b) => {
                // A register first operand; the second may be memory.
                let (x, y) = match (places[a], places[b]) {
                    (Place::Register(x), y) => (x, y),
                    (y, Place::Register(x)) => (x, y),
                    (Place::Input(x), y) => {
                        let register = registers.take()?;
                        asm.load(register, Rm::Indexed(x));
                        (register, y)
                    }
                    _ => unreachable!("topological"),
                };
                let dst = registers.take()?;
                if matches!(steps[id].op, Instruction::Add(..)) {
                    asm.add(dst, x, rm_of(y));
                } else {
                    asm.mul(dst, x, rm_of(y));
                }
                places[id] = Place::Register(dst);
            }
            Instruction::Relu(a) => {
                let x = match places[a] {
                    Place::Register(x) => x,
                    Place::Input(base) => {
                        let register = registers.take()?;
                        asm.load(register, Rm::Indexed(base));
                        register
                    }
                    Place::Unset => unreachable!("topological"),
                };
                let dst = registers.take()?;
                asm.relu(dst, x, zero);
                places[id] = Place::Register(dst);
            }
            Instruction::MatMul(..)
            | Instruction::SumRows(_)
            | Instruction::Softmax(_)
            | Instruction::Transpose(_)
            | Instruction::Normalize(..) => unreachable!("rejected above"),
        }
        // Registers whose value has had its last read here go back to the pool, once per value.
        // A register a fused product took over keeps living as the product.
        let mut in_use = [false; 16];
        for (other, place) in places.iter().enumerate() {
            if let Place::Register(r) = *place
                && (other == id || last_read[other] > id)
            {
                in_use[r as usize] = true;
            }
        }
        if let Some((target, Rm::Register(multiplier))) = pending_product {
            in_use[target as usize] = true;
            in_use[multiplier as usize] = true;
        }
        registers.free = (0..16u8).rev().filter(|r| !in_use[*r as usize] && !pinned[*r as usize]).collect();
    }
    let root = steps.len() - 1;
    let Place::Register(result) = places[root] else {
        return Err("a constant or bare input root needs no loop".to_string());
    };
    asm.store(result, Rm::Indexed(RSI));
    // add rdi, 32; cmp rdi, rdx; jb loop_start
    asm.0.extend_from_slice(&[0x48, 0x83, 0xC7, 32, 0x48, 0x39, 0xD7]);
    let after_jump = asm.0.len() + 6;
    let displacement = i32::try_from(loop_start).expect("small") - i32::try_from(after_jump).expect("small");
    asm.0.extend_from_slice(&[0x0F, 0x82]);
    asm.0.extend_from_slice(&displacement.to_le_bytes());
    asm.0.extend_from_slice(&[0xC5, 0xF8, 0x77, 0xC3]); // vzeroupper; ret

    let code_bytes = asm.0.len();
    let size = code_bytes.next_multiple_of(4096);
    // SAFETY: a fresh anonymous private mapping, written while writable and then made executable.
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if map == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        std::ptr::copy_nonoverlapping(asm.0.as_ptr(), map.cast::<u8>(), code_bytes);
        if libc::mprotect(map, size, libc::PROT_READ | libc::PROT_EXEC) != 0 {
            let error = std::io::Error::last_os_error();
            libc::munmap(map, size);
            return Err(format!("mprotect: {error}"));
        }
        let entry = std::mem::transmute::<*mut libc::c_void, Entry>(map);
        Ok(Compiled { map, size, entry, constants, code_bytes })
    }
}
