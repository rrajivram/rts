//! `rts <file.ts>` — run a TypeScript file.
//! `rts --dump <file.ts>` — print the compiled bytecode.

use std::process::ExitCode;

use rts::runtime::Value;
use rts::vm::Vm;

fn main() -> ExitCode {
    let mut dump = false;
    let mut erase = false;
    let mut path = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--dump" => dump = true,
            "--no-types" => erase = true,
            _ => path = Some(a),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: rts [--dump] [--no-types] <file.ts>");
        return ExitCode::from(2);
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rts: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let prog = match rts::compile_file(std::path::Path::new(&path), src, erase) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if dump {
        print!("{}", rts::disasm::dump(&prog, false));
        return ExitCode::SUCCESS;
    }
    let mut vm = Vm::new(prog);
    match vm.run_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let msg = match &e {
                Value::Obj(_) => vm.inspect(&e),
                v => v.to_display(),
            };
            eprintln!("Uncaught {msg}");
            ExitCode::from(1)
        }
    }
}
