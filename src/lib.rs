//! rts: a TypeScript runtime without a garbage collector.
//!
//! Pipeline: lexer → parser → checker (typed HIR) → codegen (typed slot
//! bytecode) → VM. See DESIGN.md.

pub mod ast;
pub mod bytecode;
pub mod check;
pub mod codegen;
pub mod disasm;
pub mod error;
pub mod hir;
pub mod lexer;
pub mod module;
pub mod node;
pub mod parser;
pub mod runtime;
pub mod types;
pub mod vm;

use error::CompileError;

pub const PRELUDE: &str = include_str!("prelude.ts");

/// Compile the program whose entry module is `path` (with source `src`).
/// Imported modules are loaded from disk. Errors are rendered with their
/// file and line.
pub fn compile_file(path: &std::path::Path, src: String, erase_types: bool) -> Result<bytecode::Program, String> {
    let mut sources = error::Sources::default();
    let base = sources.add("<prelude>".into(), PRELUDE.into());
    let r = (|| {
        let prelude = parser::parse_program(lexer::tokenize_at(PRELUDE, base)?)?;
        let graph = module::load(path, src, &mut sources)?;
        let hir = check::check_program(&prelude, &graph, erase_types)?;
        Ok(codegen::compile(&hir))
    })();
    r.map_err(|e: CompileError| sources.render(&e))
}
