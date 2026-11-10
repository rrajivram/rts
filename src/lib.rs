//! rts: a TypeScript runtime without a garbage collector.
//!
//! Pipeline: lexer → parser → checker (typed HIR) → codegen (typed register
//! bytecode) → VM. See DESIGN.md.

pub mod ast;
pub mod bytecode;
pub mod check;
pub mod codegen;
pub mod error;
pub mod hir;
pub mod lexer;
pub mod parser;
pub mod runtime;
pub mod types;
pub mod vm;

use error::CompileError;

pub const PRELUDE: &str = include_str!("prelude.ts");

/// Compile TypeScript source to a bytecode program.
pub fn compile(src: &str) -> Result<bytecode::Program, (CompileError, bool)> {
    let prelude = parser::parse_program(lexer::tokenize(PRELUDE).map_err(|e| (e, true))?).map_err(|e| (e, true))?;
    let toks = lexer::tokenize(src).map_err(|e| (e, false))?;
    let program = parser::parse_program(toks).map_err(|e| (e, false))?;
    let hir = check::check_program(&prelude, &program).map_err(|e| (e, false))?;
    Ok(codegen::compile(&hir))
}
