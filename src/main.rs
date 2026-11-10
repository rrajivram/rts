//! `rts <file.ts>` — run a TypeScript file.
//! `rts --dump <file.ts>` — print the compiled bytecode.

use std::process::ExitCode;

use rts::runtime::Value;
use rts::vm::Vm;

fn main() -> ExitCode {
    let mut dump = false;
    let mut path = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--dump" => dump = true,
            _ => path = Some(a),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: rts [--dump] <file.ts>");
        return ExitCode::from(2);
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rts: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let prog = match rts::compile(&src) {
        Ok(p) => p,
        Err((e, in_prelude)) => {
            if in_prelude {
                eprintln!("{}", e.render("<prelude>", rts::PRELUDE));
            } else {
                eprintln!("{}", e.render(&path, &src));
            }
            return ExitCode::from(1);
        }
    };
    if dump {
        for (i, f) in prog.funcs.iter().enumerate() {
            println!("fn #{i} {} (ns={}, nv={}, params={:?})", f.name, f.ns, f.nv, f.params);
            for (pc, ins) in f.code.iter().enumerate() {
                println!("  {pc:4}  {ins:?}");
            }
        }
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
