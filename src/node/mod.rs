//! Node's built-in modules (`node:fs`, `node:path`, …), written in
//! TypeScript on top of rts natives (see `natives.rs`).

pub mod natives;

/// (name, source)
pub const MODULES: &[(&str, &str)] = &[
    ("fs", include_str!("fs.ts")),
    ("fs/promises", include_str!("fs_promises.ts")),
    ("path", include_str!("path.ts")),
    ("events", include_str!("events.ts")),
    ("util", include_str!("util.ts")),
    ("os", include_str!("os.ts")),
    ("assert", include_str!("assert.ts")),
    ("assert/strict", include_str!("assert.ts")),
    ("timers/promises", include_str!("timers_promises.ts")),
    ("process", include_str!("process.ts")),
    ("url", include_str!("url.ts")),
    ("crypto", include_str!("crypto.ts")),
];
