//! `rts <file.ts>` — run a TypeScript file.
//! `rts --dump <file.ts>` — print the compiled bytecode.

use std::process::ExitCode;

use rts::runtime::Value;

/// Counts live and peak heap bytes (reported by `--mem-stats`).
#[global_allocator]
static ALLOC: rts::runtime::heap::CountingAlloc = rts::runtime::heap::CountingAlloc;
use rts::vm::Vm;

fn main() -> ExitCode {
    let mut dump = false;
    let mut erase = false;
    let mut mem_stats = false;
    let mut no_arena = false;
    let mut profile = false;
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
            "--no-cycles" => rts::runtime::cycles::disable(),
            "--mem-stats" => mem_stats = true,
            "--no-arena" => {
                no_arena = true;
                rts::jit::no_arenas();
            }
            "--profile" => profile = true,
            "--no-jit" => rts::jit::disable(),
            _ => path = Some(a),
        }
    }
    if let Some(n) = std::env::var("RTS_CC_THRESHOLD").ok().and_then(|v| v.parse().ok()) {
        rts::runtime::cycles::set_threshold(n);
    }
    #[cfg(feature = "gc")]
    if let Some(n) = std::env::var("RTS_GC_MIN_HEAP").ok().and_then(|v| v.parse().ok()) {
        rts::runtime::gc::set_min_heap(n);
    }
    #[cfg(feature = "gc")]
    if let Some(g) = std::env::var("RTS_GC_GROW").ok().and_then(|v| v.parse().ok()) {
        rts::runtime::gc::set_grow(g);
    }
    let Some(path) = path else {
        eprintln!("usage: rts [--dump] [--no-types] [--no-cycles] [--no-arena] [--mem-stats] [--profile] [--no-jit] <file.ts>");
        return ExitCode::from(2);
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rts: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    if dump {
        // `--dump` prints bytecode: no native code needed.
        rts::jit::disable();
    }
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
    if no_arena {
        vm.arenas = false;
    }
    if profile {
        #[cfg(feature = "profile")]
        {
            vm.profile = Some(rts::profile::Profile::start());
        }
        #[cfg(not(feature = "profile"))]
        {
            eprintln!("rts: --profile needs a build with `--features profile`");
            return ExitCode::from(2);
        }
    }
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
    #[cfg(feature = "profile")]
    if let Some(p) = vm.profile.as_mut() {
        p.report();
    }
    if mem_stats {
        rts::runtime::heap::print_stats();
    }
    // The process is exiting: skip tearing down the VM's (large) slot stacks.
    std::mem::forget(vm);
    code
}
