//! The instructions the VM runs.
//!
//! # Slots
//!
//! Every local variable, parameter and temporary value of a function lives in
//! a numbered **slot** of the function's frame. The compiler gives each slot a
//! fixed type, so an instruction always knows what kind of value an operand
//! holds and never has to check at run time:
//!
//! | slot type  | holds                                               | stored as          |
//! |------------|-----------------------------------------------------|--------------------|
//! | `NumSlot`  | a `number`                                          | raw `f64` bits     |
//! | `BoolSlot` | a `boolean`                                         | `0` or `1`         |
//! | `BigSlot`  | a `bigint` (an `i128`, as in rjs)                   | two scalar slots   |
//! | `ValSlot`  | anything else (`string`, objects, `any`, unions, …) | a tagged [`Value`] |
//!
//! Number, boolean and bigint slots share one array per frame (the *scalar* slots),
//! and value slots are a second array (the *value* slots). Keeping numbers out
//! of `Value` is what makes typed arithmetic a plain `f64` add, with no tag
//! check and no reference counting. `NumSlot(3)` and `ValSlot(3)` are
//! different slots: one is index 3 of the scalar array, the other index 3 of
//! the value array.
//!
//! Value slot 0 always holds `this`. Parameters follow, each in a slot of its
//! own type, in order.
//!
//! `rts --dump file.ts` prints a program's instructions with slots shown by
//! the name of the variable they hold (or `num3`, `bool3`, `big3`, `val3` for
//! temporaries, and `#2` for a slot preloaded with the constant 2).
//!
//! # Converting between slot types
//!
//! Where a typed value meets an untyped one (passing a `number` to a function
//! taking `any`, reading an `any` into a `number` variable), the compiler
//! inserts `BoxNumber` / `UnboxNumber` (or the `Bool` versions). Unboxing
//! checks the value's type and throws a `TypeError` if it is wrong, so a
//! wrong `as` cast or `any` can never be misread.

use std::rc::Rc;

use crate::hir::{CmpOp, NumOp};
use crate::runtime::{ClassRt, Native, PropCache, Shape, Value};
use crate::types::Kind;

/// A frame slot holding a `number`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumSlot(pub u16);

/// A frame slot holding a `boolean`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoolSlot(pub u16);

/// A frame slot holding a `bigint`: an `i128` in scalar slots `n` (low 64
/// bits) and `n + 1` (high 64 bits). A BigInt that doesn't fit is a
/// RangeError, never a heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BigSlot(pub u16);

/// A frame slot holding a [`Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValSlot(pub u16);

/// A position in a function's instruction list (shown as a label, `L12`, in dumps).
pub type Target = u32;

/// Index of a slot within its array, whatever its type.
pub trait SlotIndex: Copy {
    fn index(self) -> usize;
}
impl SlotIndex for NumSlot {
    #[inline(always)]
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl SlotIndex for BoolSlot {
    #[inline(always)]
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl SlotIndex for BigSlot {
    #[inline(always)]
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl SlotIndex for ValSlot {
    #[inline(always)]
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl SlotIndex for u16 {
    #[inline(always)]
    fn index(self) -> usize {
        self as usize
    }
}

/// One VM instruction. Operands name slots of the current frame; `output` is
/// where the result goes. Jump targets (`to`, `catch`) are instruction indices.
///
/// Instructions are `Copy` and 16 bytes, so the VM can fetch them cheaply.
#[derive(Debug, Clone, Copy)]
pub enum Instruction {
    // ── numbers and booleans ──
    /// `output = value`
    LoadNumber { value: f64, output: NumSlot },
    /// `output = value`
    LoadBool { value: bool, output: BoolSlot },
    /// `to = from`
    CopyNumber { from: NumSlot, to: NumSlot },
    /// `to = from`
    CopyBool { from: BoolSlot, to: BoolSlot },
    /// `output = left op right` on numbers (`+ - * / % **` and bitwise operators).
    Arithmetic { op: NumOp, left: NumSlot, right: NumSlot, output: NumSlot },
    /// `output = left + constant` (also `x - 1` with a negative constant).
    AddConstant { left: NumSlot, constant: f64, output: NumSlot },
    /// `output = left op right` on numbers (`< <= > >= === !==`).
    Compare { op: CmpOp, left: NumSlot, right: NumSlot, output: BoolSlot },
    /// `output = left === right` (or `!==` when `equal` is false) on booleans.
    CompareBools { equal: bool, left: BoolSlot, right: BoolSlot, output: BoolSlot },
    /// `output = -value`
    Negate { value: NumSlot, output: NumSlot },
    /// `output = !value`
    Not { value: BoolSlot, output: BoolSlot },
    /// `output = ~value`
    BitNot { value: NumSlot, output: NumSlot },
    /// `output = Boolean(value)`: false for 0 and NaN.
    NumberIsTruthy { value: NumSlot, output: BoolSlot },
    /// `output = Math.function(value)`, e.g. `Math.sqrt`.
    MathUnary { function: Native, value: NumSlot, output: NumSlot },
    /// `output = Math.function(left, right)`, e.g. `Math.max`.
    MathBinary { function: Native, left: NumSlot, right: NumSlot, output: NumSlot },

    // ── bigints ──
    /// `output = constants[constant]`, a BigInt literal.
    LoadBigInt { constant: u32, output: BigSlot },
    /// `to = from`
    CopyBigInt { from: BigSlot, to: BigSlot },
    /// `output = left op right` on BigInts. Overflowing 128 bits or dividing
    /// by zero throws a `RangeError`.
    BigIntArithmetic { op: NumOp, left: BigSlot, right: BigSlot, output: BigSlot },
    /// `output = left op right` on BigInts (`< <= > >= === !==`).
    CompareBigInts { op: CmpOp, left: BigSlot, right: BigSlot, output: BoolSlot },
    /// `output = -value`
    NegateBigInt { value: BigSlot, output: BigSlot },
    /// `output = ~value`
    BitNotBigInt { value: BigSlot, output: BigSlot },
    /// `output = Boolean(value)`: false for 0n.
    BigIntIsTruthy { value: BigSlot, output: BoolSlot },

    // ── jumps ──
    /// Continue at `to`.
    Jump { to: Target },
    /// Jump to `to` if `condition == when`.
    JumpIf { when: bool, condition: BoolSlot, to: Target },
    /// Jump to `to` if `(left op right) == when`: a number comparison and a
    /// branch in one instruction, as at the top of most loops.
    JumpIfCompare { op: CmpOp, when: bool, left: NumSlot, right: NumSlot, to: Target },
    /// Jump to `to` if the value's truthiness equals `when`.
    JumpIfTruthy { when: bool, value: ValSlot, to: Target },
    /// Jump to `to` if "value is null or undefined" equals `when` (for `??` and `?.`).
    JumpIfNullish { when: bool, value: ValSlot, to: Target },

    // ── values ──
    /// `output = constants[constant]` (strings and other literals).
    LoadConstant { constant: u32, output: ValSlot },
    /// `output = undefined`
    LoadUndefined { output: ValSlot },
    /// `output = null`
    LoadNull { output: ValSlot },
    /// `to = from`
    CopyValue { from: ValSlot, to: ValSlot },
    /// Drop the references held by `count` slots starting at `first`, as a
    /// block's variables go out of scope. This is where memory is freed: there
    /// is no garbage collector.
    Release { first: ValSlot, count: u16 },
    /// `output = value` as a `Value`.
    BoxNumber { value: NumSlot, output: ValSlot },
    /// `output = value` as a `Value`.
    BoxBool { value: BoolSlot, output: ValSlot },
    /// `output = value` as a `Value`.
    BoxBigInt { value: BigSlot, output: ValSlot },
    /// `output = value`, which must be a BigInt (else `TypeError`).
    UnboxBigInt { value: ValSlot, output: BigSlot },
    /// `output = value`, which must be a number (else `TypeError`).
    UnboxNumber { value: ValSlot, output: NumSlot },
    /// `output = value`, which must be a boolean (else `TypeError`).
    UnboxBool { value: ValSlot, output: BoolSlot },
    /// `output = Boolean(value)`
    IsTruthy { value: ValSlot, output: BoolSlot },
    /// `output = String(value)` for a number.
    NumberToString { value: NumSlot, output: ValSlot },
    /// `output = String(value)`
    ToString { value: ValSlot, output: ValSlot },
    /// `output = +value` (a BigInt is a `TypeError`)
    ToNumber { value: ValSlot, output: NumSlot },
    /// `output = left + right` on two strings.
    Concat { left: ValSlot, right: ValSlot, output: ValSlot },
    /// `output = left op right` on two strings.
    CompareStrings { op: CmpOp, left: ValSlot, right: ValSlot, output: BoolSlot },
    /// `output = left === right` (or `!==` when `equal` is false).
    StrictEquals { equal: bool, left: ValSlot, right: ValSlot, output: BoolSlot },
    /// `output = left == right` (or `!=` when `equal` is false).
    LooseEquals { equal: bool, left: ValSlot, right: ValSlot, output: BoolSlot },
    /// `output = left + right` when the types aren't known: adds numbers or
    /// concatenates strings, decided at run time.
    DynamicAdd { left: ValSlot, right: ValSlot, output: ValSlot },
    /// `output = left op right` on untyped values (converted to numbers; a
    /// BigInt operand is a `TypeError`, since the result must be a number).
    DynamicArithmetic { op: NumOp, left: ValSlot, right: ValSlot, output: NumSlot },
    /// `output = left op right` on untyped values, stored as a `Value`: two
    /// BigInts give a BigInt, a BigInt and a number a `TypeError`.
    DynamicArithmeticToValue { op: NumOp, left: ValSlot, right: ValSlot, output: ValSlot },
    /// `output = value + constant` as a `Value` (`i++` on an `any`; a BigInt adds `constant` as a BigInt).
    DynamicAddConstant { value: ValSlot, constant: f64, output: ValSlot },
    /// `output = left op right` on untyped values (numbers or strings).
    DynamicCompare { op: CmpOp, left: ValSlot, right: ValSlot, output: BoolSlot },
    /// `output = -value` on an untyped value (a BigInt stays a BigInt).
    DynamicNegate { value: ValSlot, output: ValSlot },
    /// `output = ~value` on an untyped value (a BigInt stays a BigInt).
    DynamicBitNot { value: ValSlot, output: ValSlot },
    /// `output = typeof value`
    TypeOf { value: ValSlot, output: ValSlot },
    /// `output = key in object`
    In { key: ValSlot, object: ValSlot, output: BoolSlot },
    /// `output = value instanceof classes[class]`
    InstanceOf { value: ValSlot, class: u32, output: BoolSlot },

    // ── captured variables and globals ──
    /// `output = a new cell`. A variable captured by a closure lives in a
    /// cell, so the closure and the function share it.
    NewCell { output: ValSlot },
    /// `output = the value inside cell`
    ReadCell { cell: ValSlot, output: ValSlot },
    /// Put `value` inside `cell`.
    WriteCell { cell: ValSlot, value: ValSlot },
    /// `output = captures[capture]`: a cell the running closure captured.
    ReadCapture { capture: u16, output: ValSlot },
    /// Put `value` in the running closure's captured cell `capture`.
    WriteCapture { capture: u16, value: ValSlot },
    /// `output = global number variable`
    ReadGlobalNumber { global: u32, output: NumSlot },
    /// `output = global boolean variable`
    ReadGlobalBool { global: u32, output: BoolSlot },
    /// `global number variable = value`
    WriteGlobalNumber { global: u32, value: NumSlot },
    /// `global boolean variable = value`
    WriteGlobalBool { global: u32, value: BoolSlot },
    /// `output = global bigint variable`
    ReadGlobalBigInt { global: u32, output: BigSlot },
    /// `global bigint variable = value`
    WriteGlobalBigInt { global: u32, value: BigSlot },
    /// `output = global variable`
    ReadGlobal { global: u32, output: ValSlot },
    /// `global variable = value`
    WriteGlobal { global: u32, value: ValSlot },

    // ── functions ──
    /// `output = a closure of functions[function]`, capturing the cells listed
    /// in `capture_lists[captures]`.
    MakeClosure { function: u32, captures: u32, output: ValSlot },
    /// Call as described by `call_sites[site]` (callee, `this`, arguments, output).
    Call { site: u32 },
    /// Return a number.
    ReturnNumber { value: NumSlot },
    /// Return a boolean.
    ReturnBool { value: BoolSlot },
    /// Return a bigint.
    ReturnBigInt { value: BigSlot },
    /// Return a value.
    ReturnValue { value: ValSlot },
    /// Return `undefined`.
    ReturnUndefined,

    // ── arrays and strings ──
    /// `output = []`; `numbers` starts it with `f64` storage (for `number[]`).
    NewArray { numbers: bool, output: ValSlot },
    /// `array.push(value)`
    PushNumber { array: ValSlot, value: NumSlot },
    /// `array.push(value)`
    PushValue { array: ValSlot, value: ValSlot },
    /// `array.push(...source)`
    PushSpread { array: ValSlot, source: ValSlot },
    /// `output = array[index]` for a `number[]`.
    ReadElementNumber { array: ValSlot, index: NumSlot, output: NumSlot },
    /// `array[index] = value` for a `number[]`.
    WriteElementNumber { array: ValSlot, index: NumSlot, value: NumSlot },
    /// `output = array[index]`
    ReadElement { array: ValSlot, index: NumSlot, output: ValSlot },
    /// `array[index] = value`
    WriteElement { array: ValSlot, index: NumSlot, value: ValSlot },
    /// `output = object[key]` when the key's type isn't known.
    ReadDynamic { object: ValSlot, key: ValSlot, output: ValSlot },
    /// `object[key] = value` when the key's type isn't known.
    WriteDynamic { object: ValSlot, key: ValSlot, value: ValSlot },
    /// `delete object[key]`
    DeleteDynamic { object: ValSlot, key: ValSlot },
    /// `output = array.length`
    ArrayLength { array: ValSlot, output: NumSlot },
    /// `array.length = length`
    SetArrayLength { array: ValSlot, length: NumSlot },
    /// `output = string.length`
    StringLength { string: ValSlot, output: NumSlot },
    /// `output = string.charCodeAt(index)`
    CharCodeAt { string: ValSlot, index: NumSlot, output: NumSlot },
    /// `output = the elements a for-of loop over value visits`, as an array.
    ToIterationArray { value: ValSlot, output: ValSlot },
    /// `output = the keys a for-in loop over object visits`, as an array.
    KeysArray { object: ValSlot, output: ValSlot },

    // ── objects ──
    /// `output = a new object with shapes[shape]`'s keys, all undefined.
    NewObject { shape: u32, output: ValSlot },
    /// `{ ...source }` into `object`.
    SpreadIntoObject { object: ValSlot, source: ValSlot },
    /// `output = object.<field>`, where the field's position is known (class
    /// instances): no lookup by name.
    ReadField { object: ValSlot, field: u16, output: ValSlot },
    /// `output = object.<field>` for a number field.
    ReadFieldNumber { object: ValSlot, field: u16, output: NumSlot },
    /// `object.<field> = value`
    WriteField { object: ValSlot, field: u16, value: ValSlot },
    /// `object.<field> = value` for a number field.
    WriteFieldNumber { object: ValSlot, field: u16, value: NumSlot },
    /// `output = object.name`, looked up by name with a cache
    /// (`property_sites[property]`), for objects whose layout isn't known.
    ReadProperty { object: ValSlot, property: u32, output: ValSlot },
    /// `object.name = value`, looked up by name with a cache.
    WriteProperty { object: ValSlot, property: u32, value: ValSlot },
    /// `output = a new instance of classes[class]`, fields undefined (the
    /// constructor is a separate `Call`).
    NewInstance { class: u32, output: ValSlot },

    // ── scope arenas (see escape.rs and runtime/arena.rs) ──
    /// A block with scope-bound allocations starts: push a region. On exit
    /// the region's `count` value slots from `first` are cleared.
    EnterRegion { first: ValSlot, count: u16 },
    /// The block ends: clear its slots, free everything in its arena.
    ExitRegion,
    /// Leave regions until this frame has `depth` of them (`break`,
    /// `continue`, `catch`, and `return` from an inlined call).
    ExitRegionsTo { depth: u16 },
    /// `NewInstance` in the arena of the region `up` levels out from the innermost.
    NewInstanceIn { class: u32, output: ValSlot, up: u16 },
    /// `NewObject` in a region's arena.
    NewObjectIn { shape: u32, output: ValSlot, up: u16 },
    /// `NewArray` in a region's arena.
    NewArrayIn { numbers: bool, output: ValSlot, up: u16 },
    /// `output = the class object classes[class]`
    LoadClass { class: u32, output: ValSlot },

    // ── exceptions ──
    /// `throw value`
    Throw { value: ValSlot },
    /// Start a `try` block: an exception jumps to `catch` with the thrown value in `exception`.
    TryBegin { catch: Target, exception: ValSlot },
    /// End the innermost `try` block.
    TryEnd,

    /// Define accessor property `property` on a plain object (`getter` and
    /// `setter` may be undefined).
    DefineAccessor { object: ValSlot, property: u32, getter: ValSlot, setter: ValSlot },

    // ── iteration ──
    /// `output = value[Symbol.iterator]()` (generators and arrays iterate natively).
    GetIterator { value: ValSlot, output: ValSlot },
    /// `for await`: `output = value[Symbol.asyncIterator]()`, or its sync
    /// iterator (then `is_sync` is set and each value must be awaited too).
    GetAsyncIterator { value: ValSlot, output: ValSlot, is_sync: BoolSlot },
    /// Advance `iterator`: the next value into `output`, or jump to `to` when done.
    IteratorStep { iterator: ValSlot, output: ValSlot, to: Target },
    /// `output = iterator.next()`, unexamined (`for await` awaits it).
    IteratorNextResult { iterator: ValSlot, output: ValSlot },
    /// Read an iterator result object: its `value` into `output`; jump to `to` if `done`.
    UnpackIteratorResult { result: ValSlot, output: ValSlot, to: Target },
    /// Leaving a `for-of` early: call `iterator.return()` if it has one.
    IteratorClose { iterator: ValSlot },
    /// `yield*`: resume `iterator` the way this generator was resumed
    /// (`mode`: 0 next, 1 throw, 2 return, with `value`). The result's value
    /// goes to `output`; `done` is set when the inner iterator finished.
    IteratorResume { iterator: ValSlot, mode: NumSlot, value: ValSlot, output: ValSlot, done: BoolSlot },

    // ── generators and async functions ──
    /// First instruction of a generator body (after parameter setup): suspend
    /// the frame into a new generator object and return that to the caller.
    CreateGenerator,
    /// First instruction of an async function body: give the frame a promise
    /// to settle when it returns or throws, and keep running.
    StartAsync,
    /// Suspend this generator, handing `value` to whoever resumed it. When
    /// resumed, the value passed in arrives in `received` and how it was
    /// resumed in `mode` (0 `next`, 1 `throw`, 2 `return`).
    Yield { value: ValSlot, received: ValSlot, mode: NumSlot },
    /// Suspend until `value` (made a promise) settles: its value arrives in
    /// `received`, with `mode` 0 if it fulfilled and 1 if it rejected.
    Await { value: ValSlot, received: ValSlot, mode: NumSlot },
}

const _: () = assert!(std::mem::size_of::<Instruction>() == 16);

/// Number of instruction kinds (`Instruction::kind` is below this).
pub const INSTRUCTION_KINDS: usize = 123;

/// Instruction names by kind, for per-instruction profiles.
pub const INSTRUCTION_NAMES: [&str; INSTRUCTION_KINDS] = [
            "LoadNumber",
            "LoadBool",
            "CopyNumber",
            "CopyBool",
            "Arithmetic",
            "AddConstant",
            "Compare",
            "CompareBools",
            "Negate",
            "Not",
            "BitNot",
            "NumberIsTruthy",
            "MathUnary",
            "MathBinary",
            "LoadBigInt",
            "CopyBigInt",
            "BigIntArithmetic",
            "CompareBigInts",
            "NegateBigInt",
            "BitNotBigInt",
            "BigIntIsTruthy",
            "Jump",
            "JumpIf",
            "JumpIfCompare",
            "JumpIfTruthy",
            "JumpIfNullish",
            "LoadConstant",
            "LoadUndefined",
            "LoadNull",
            "CopyValue",
            "Release",
            "BoxNumber",
            "BoxBool",
            "BoxBigInt",
            "UnboxBigInt",
            "UnboxNumber",
            "UnboxBool",
            "IsTruthy",
            "NumberToString",
            "ToString",
            "ToNumber",
            "Concat",
            "CompareStrings",
            "StrictEquals",
            "LooseEquals",
            "DynamicAdd",
            "DynamicArithmetic",
            "DynamicArithmeticToValue",
            "DynamicAddConstant",
            "DynamicCompare",
            "DynamicNegate",
            "DynamicBitNot",
            "TypeOf",
            "In",
            "InstanceOf",
            "NewCell",
            "ReadCell",
            "WriteCell",
            "ReadCapture",
            "WriteCapture",
            "ReadGlobalNumber",
            "ReadGlobalBool",
            "WriteGlobalNumber",
            "WriteGlobalBool",
            "ReadGlobalBigInt",
            "WriteGlobalBigInt",
            "ReadGlobal",
            "WriteGlobal",
            "MakeClosure",
            "Call",
            "ReturnNumber",
            "ReturnBool",
            "ReturnBigInt",
            "ReturnValue",
            "ReturnUndefined",
            "NewArray",
            "PushNumber",
            "PushValue",
            "PushSpread",
            "ReadElementNumber",
            "WriteElementNumber",
            "ReadElement",
            "WriteElement",
            "ReadDynamic",
            "WriteDynamic",
            "DeleteDynamic",
            "ArrayLength",
            "SetArrayLength",
            "StringLength",
            "CharCodeAt",
            "ToIterationArray",
            "KeysArray",
            "NewObject",
            "SpreadIntoObject",
            "ReadField",
            "ReadFieldNumber",
            "WriteField",
            "WriteFieldNumber",
            "ReadProperty",
            "WriteProperty",
            "NewInstance",
            "EnterRegion",
            "ExitRegion",
            "ExitRegionsTo",
            "NewInstanceIn",
            "NewObjectIn",
            "NewArrayIn",
            "LoadClass",
            "Throw",
            "TryBegin",
            "TryEnd",
            "DefineAccessor",
            "GetIterator",
            "GetAsyncIterator",
            "IteratorStep",
            "IteratorNextResult",
            "UnpackIteratorResult",
            "IteratorClose",
            "IteratorResume",
            "CreateGenerator",
            "StartAsync",
            "Yield",
            "Await",
];

impl Instruction {
    /// The variant's index in declaration order (an exhaustive match: a new
    /// variant must be added here and to `INSTRUCTION_NAMES`).
    #[inline(always)]
    pub fn kind(&self) -> usize {
        match self {
            Instruction::LoadNumber { .. } => 0,
            Instruction::LoadBool { .. } => 1,
            Instruction::CopyNumber { .. } => 2,
            Instruction::CopyBool { .. } => 3,
            Instruction::Arithmetic { .. } => 4,
            Instruction::AddConstant { .. } => 5,
            Instruction::Compare { .. } => 6,
            Instruction::CompareBools { .. } => 7,
            Instruction::Negate { .. } => 8,
            Instruction::Not { .. } => 9,
            Instruction::BitNot { .. } => 10,
            Instruction::NumberIsTruthy { .. } => 11,
            Instruction::MathUnary { .. } => 12,
            Instruction::MathBinary { .. } => 13,
            Instruction::LoadBigInt { .. } => 14,
            Instruction::CopyBigInt { .. } => 15,
            Instruction::BigIntArithmetic { .. } => 16,
            Instruction::CompareBigInts { .. } => 17,
            Instruction::NegateBigInt { .. } => 18,
            Instruction::BitNotBigInt { .. } => 19,
            Instruction::BigIntIsTruthy { .. } => 20,
            Instruction::Jump { .. } => 21,
            Instruction::JumpIf { .. } => 22,
            Instruction::JumpIfCompare { .. } => 23,
            Instruction::JumpIfTruthy { .. } => 24,
            Instruction::JumpIfNullish { .. } => 25,
            Instruction::LoadConstant { .. } => 26,
            Instruction::LoadUndefined { .. } => 27,
            Instruction::LoadNull { .. } => 28,
            Instruction::CopyValue { .. } => 29,
            Instruction::Release { .. } => 30,
            Instruction::BoxNumber { .. } => 31,
            Instruction::BoxBool { .. } => 32,
            Instruction::BoxBigInt { .. } => 33,
            Instruction::UnboxBigInt { .. } => 34,
            Instruction::UnboxNumber { .. } => 35,
            Instruction::UnboxBool { .. } => 36,
            Instruction::IsTruthy { .. } => 37,
            Instruction::NumberToString { .. } => 38,
            Instruction::ToString { .. } => 39,
            Instruction::ToNumber { .. } => 40,
            Instruction::Concat { .. } => 41,
            Instruction::CompareStrings { .. } => 42,
            Instruction::StrictEquals { .. } => 43,
            Instruction::LooseEquals { .. } => 44,
            Instruction::DynamicAdd { .. } => 45,
            Instruction::DynamicArithmetic { .. } => 46,
            Instruction::DynamicArithmeticToValue { .. } => 47,
            Instruction::DynamicAddConstant { .. } => 48,
            Instruction::DynamicCompare { .. } => 49,
            Instruction::DynamicNegate { .. } => 50,
            Instruction::DynamicBitNot { .. } => 51,
            Instruction::TypeOf { .. } => 52,
            Instruction::In { .. } => 53,
            Instruction::InstanceOf { .. } => 54,
            Instruction::NewCell { .. } => 55,
            Instruction::ReadCell { .. } => 56,
            Instruction::WriteCell { .. } => 57,
            Instruction::ReadCapture { .. } => 58,
            Instruction::WriteCapture { .. } => 59,
            Instruction::ReadGlobalNumber { .. } => 60,
            Instruction::ReadGlobalBool { .. } => 61,
            Instruction::WriteGlobalNumber { .. } => 62,
            Instruction::WriteGlobalBool { .. } => 63,
            Instruction::ReadGlobalBigInt { .. } => 64,
            Instruction::WriteGlobalBigInt { .. } => 65,
            Instruction::ReadGlobal { .. } => 66,
            Instruction::WriteGlobal { .. } => 67,
            Instruction::MakeClosure { .. } => 68,
            Instruction::Call { .. } => 69,
            Instruction::ReturnNumber { .. } => 70,
            Instruction::ReturnBool { .. } => 71,
            Instruction::ReturnBigInt { .. } => 72,
            Instruction::ReturnValue { .. } => 73,
            Instruction::ReturnUndefined { .. } => 74,
            Instruction::NewArray { .. } => 75,
            Instruction::PushNumber { .. } => 76,
            Instruction::PushValue { .. } => 77,
            Instruction::PushSpread { .. } => 78,
            Instruction::ReadElementNumber { .. } => 79,
            Instruction::WriteElementNumber { .. } => 80,
            Instruction::ReadElement { .. } => 81,
            Instruction::WriteElement { .. } => 82,
            Instruction::ReadDynamic { .. } => 83,
            Instruction::WriteDynamic { .. } => 84,
            Instruction::DeleteDynamic { .. } => 85,
            Instruction::ArrayLength { .. } => 86,
            Instruction::SetArrayLength { .. } => 87,
            Instruction::StringLength { .. } => 88,
            Instruction::CharCodeAt { .. } => 89,
            Instruction::ToIterationArray { .. } => 90,
            Instruction::KeysArray { .. } => 91,
            Instruction::NewObject { .. } => 92,
            Instruction::SpreadIntoObject { .. } => 93,
            Instruction::ReadField { .. } => 94,
            Instruction::ReadFieldNumber { .. } => 95,
            Instruction::WriteField { .. } => 96,
            Instruction::WriteFieldNumber { .. } => 97,
            Instruction::ReadProperty { .. } => 98,
            Instruction::WriteProperty { .. } => 99,
            Instruction::NewInstance { .. } => 100,
            Instruction::EnterRegion { .. } => 101,
            Instruction::ExitRegion { .. } => 102,
            Instruction::ExitRegionsTo { .. } => 103,
            Instruction::NewInstanceIn { .. } => 104,
            Instruction::NewObjectIn { .. } => 105,
            Instruction::NewArrayIn { .. } => 106,
            Instruction::LoadClass { .. } => 107,
            Instruction::Throw { .. } => 108,
            Instruction::TryBegin { .. } => 109,
            Instruction::TryEnd { .. } => 110,
            Instruction::DefineAccessor { .. } => 111,
            Instruction::GetIterator { .. } => 112,
            Instruction::GetAsyncIterator { .. } => 113,
            Instruction::IteratorStep { .. } => 114,
            Instruction::IteratorNextResult { .. } => 115,
            Instruction::UnpackIteratorResult { .. } => 116,
            Instruction::IteratorClose { .. } => 117,
            Instruction::IteratorResume { .. } => 118,
            Instruction::CreateGenerator { .. } => 119,
            Instruction::StartAsync { .. } => 120,
            Instruction::Yield { .. } => 121,
            Instruction::Await { .. } => 122,
        }
    }
}

/// Where a closure's captured cell comes from when the closure is made.
#[derive(Debug, Clone, Copy)]
pub enum CaptureFrom {
    /// A value slot of the function making the closure, holding the cell.
    Slot(ValSlot),
    /// A cell the function making the closure captured itself.
    Capture(u16),
}

/// What a `Call` calls.
#[derive(Debug)]
pub enum CallTarget {
    /// `functions[i]`, known at compile time (a top-level function that is never reassigned).
    Direct(u32),
    /// The function value in a slot.
    Value(ValSlot),
    /// A method of the receiver's class, by its position (`slot`) in the
    /// class's method table. `class` is the receiver's static class (for
    /// `--dump`; the receiver may be a subclass, whose table is used).
    Method { class: u32, slot: u32 },
    /// A method looked up by name on the receiver (`property_sites[i]`).
    Property(u32),
    /// A built-in function.
    Native(Native),
}

/// Everything about one call: what to call, its arguments and where the result goes.
#[derive(Debug)]
pub struct CallSite {
    pub target: CallTarget,
    /// The receiver, for method calls.
    pub this: Option<ValSlot>,
    /// Each argument: its slot type and slot index.
    pub args: Vec<(Kind, u16)>,
    /// Which arguments are spread (`f(...xs)`).
    pub spread: Vec<bool>,
    pub has_spread: bool,
    /// Where the result goes: its slot type and slot index (`None`: discarded).
    pub output: Option<(Kind, u16)>,
    /// The caller already packed the rest arguments into an array.
    pub packed_rest: bool,
    /// The region the callee allocates its (fresh) result in, as `up` levels
    /// out from the caller's innermost region; `RESULT_REGION` passes on the
    /// caller's own result region. None: the heap.
    pub region: Option<u16>,
}

/// `up` value meaning "the region the caller was given for its own result".
pub const RESULT_REGION: u16 = u16::MAX;

/// A property accessed by name, with its lookup cache.
#[derive(Debug)]
pub struct PropertySite {
    pub name: Rc<str>,
    pub key: Value,
    pub cache: PropCache,
    /// The name is `length` (arrays answer it without a lookup).
    pub is_length: bool,
}

impl PropertySite {
    pub fn new(name: &str) -> PropertySite {
        PropertySite {
            name: name.into(),
            key: Value::str(name),
            cache: PropCache::default(),
            is_length: name == "length",
        }
    }
}

/// What a slot holds from one instruction on, for `--dump`.
#[derive(Debug, Clone)]
pub struct SlotName {
    /// `Kind::Val` for a value slot; otherwise a scalar slot.
    pub kind: Kind,
    pub index: u16,
    /// The variable's name, or `#2` for a slot preloaded with the constant 2;
    /// `None` when the slot becomes a temporary.
    pub name: Option<Rc<str>>,
    /// The first instruction where the slot holds it.
    pub from: u32,
}

/// A compiled function.
#[derive(Debug, Default)]
pub struct FunctionCode {
    /// Index in `Program::funcs`.
    pub id: u32,
    pub name: Rc<str>,
    pub code: Vec<Instruction>,
    /// How many number/boolean slots and value slots a frame needs.
    pub scalar_slots: usize,
    pub value_slots: usize,
    /// Slot types of the parameters (after `this`, which is value slot 0).
    pub params: Vec<Kind>,
    pub rest: bool,
    /// An async function or async generator.
    pub is_async: bool,
    /// Native code for this function (see `jit/`).
    pub jit: Option<Box<crate::jit::JitFn>>,
    /// Value slots holding *borrowed* references (bit i = slot i): `this` and
    /// parameters the function never reassigns. The caller's slots keep those
    /// values alive for the whole call, so the callee's copies are not counted:
    /// no increment at the call, no decrement at the return.
    pub borrowed: u64,
    pub constants: Vec<Value>,
    pub call_sites: Vec<CallSite>,
    pub property_sites: Vec<PropertySite>,
    pub capture_lists: Vec<Vec<CaptureFrom>>,
    /// Names of the cells this function captures (for `--dump`).
    pub capture_names: Vec<Rc<str>>,
    /// Which variable each slot holds, and from where (for `--dump`).
    pub slot_names: Vec<SlotName>,
    /// The field name of each field instruction, by instruction index (for `--dump`).
    pub field_names: Vec<(u32, Rc<str>)>,
    /// For `--dump`: the name to show (`Class.method` for methods), the
    /// parameter names, and whether it belongs to the built-in prelude.
    pub dump_name: Rc<str>,
    pub param_names: Vec<Rc<str>>,
    pub prelude: bool,
}

#[derive(Debug)]
pub struct Program {
    pub funcs: Vec<Rc<FunctionCode>>,
    pub classes: Vec<Rc<ClassRt>>,
    pub shapes: Vec<Rc<Shape>>,
    /// How many global number/boolean variables and global value variables there are.
    pub global_scalars: usize,
    pub global_values: usize,
    /// Their names (for `--dump`).
    pub global_scalar_names: Vec<Rc<str>>,
    pub global_value_names: Vec<Rc<str>>,
    pub main: u32,
    pub error_class: Option<u32>,
    pub type_error_class: Option<u32>,
    pub range_error_class: Option<u32>,
    pub syntax_error_class: Option<u32>,
}

