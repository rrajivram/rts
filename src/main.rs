//! `rts <file.ts>` — run a TypeScript file.
//! `rts --dump <file.ts>` — print the compiled bytecode.

use std::process::ExitCode;

use rts::runtime::Value;
use rts::vm::Vm;

fn main() -> ExitCode {
    let mut dump = false;
    let mut erase = false;
    let mut path = None;
    let mut script_args = Vec::new();
    for a in std::env::args().skip(1) {
        if path.is_some() {
            // Everything after the script belongs to it (process.argv).
            script_args.push(a);
            continue;
        }
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
    let abs = std::fs::canonicalize(&path).map(|p| p.to_string_lossy().into_owned()).unwrap_or(path.clone());
    vm.args = std::iter::once(abs).chain(script_args).collect();
    let code = match vm.run_main() {
        Ok(()) => ExitCode::from(vm.exit_code() as u8),
        Err(e) => {
            let msg = match &e {
                Value::Obj(_) => vm.inspect(&e),
                v => v.to_display(),
            };
            eprintln!("Uncaught {msg}");
            ExitCode::from(1)
        }
    };
    // The process is exiting: skip tearing down the VM's (large) slot stacks.
    std::mem::forget(vm);
    code
}
