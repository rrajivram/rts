//! Natives behind Node's globals and built-in modules (`process`, `node:fs`,
//! `node:os`, `util.format`). The modules themselves are TypeScript
//! (`src/node/*.ts`) calling these through `__rts_*` functions.

use crate::runtime::Gc;
use std::rc::Rc;

use crate::runtime::natives::{make_array, Native};
use crate::runtime::*;
use crate::vm::Vm;

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undef)
}

fn obj(keys: &[&str], vals: Vec<Value>) -> Value {
    Value::Obj(Object::new(Shape::new(keys.iter().map(|k| Rc::from(*k)).collect()), vals, None))
}

fn s(v: &Value) -> String {
    v.to_js_string().s.to_string()
}

/// A Node-style system error: `ENOENT: no such file or directory, open 'x'`.
fn sys_error(vm: &mut Vm, e: std::io::Error, syscall: &str, path: &str) -> Value {
    let (code, text) = match e.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        _ if e.raw_os_error() == Some(21) => ("EISDIR", "illegal operation on a directory"),
        _ if e.raw_os_error() == Some(20) => ("ENOTDIR", "not a directory"),
        _ if e.raw_os_error() == Some(66) || e.raw_os_error() == Some(39) => ("ENOTEMPTY", "directory not empty"),
        _ => ("EIO", "i/o error"),
    };
    let err = vm.make_error_pub(format!("{code}: {text}, {syscall} '{path}'"));
    if let Value::Obj(o) = &err {
        o.set(&"code".into(), Value::str(code));
        o.set(&"syscall".into(), Value::str(syscall));
        o.set(&"path".into(), Value::str(path));
    }
    err
}

pub fn call(vm: &mut Vm, n: Native, _this: Value, args: Vec<Value>) -> Result<Value, Value> {
    use Native::*;
    let path = || s(&arg(&args, 0));
    Ok(match n {
        ProcessObject => vm.process_object(),
        ProcessExit => {
            vm.flush();
            let code = match arg(&args, 0) {
                Value::Undef => vm.exit_code(),
                v => v.to_number() as i32,
            };
            std::process::exit(code);
        }
        ProcessCwd => Value::str(std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()),
        ProcessChdir => {
            if let Err(e) = std::env::set_current_dir(path()) {
                return Err(sys_error(vm, e, "chdir", &path()));
            }
            Value::Undef
        }
        StdoutWrite | StderrWrite => {
            let text = s(&arg(&args, 0));
            if n == StdoutWrite {
                vm.write_out(&text);
            } else {
                vm.flush();
                eprint!("{text}");
            }
            Value::Bool(true)
        }
        HrtimeBigint => Value::bigint(vm.start.elapsed().as_nanos() as i128 + 1_000_000_000),
        Hrtime => {
            // [seconds, nanoseconds], relative to `prev` when given.
            let mut t = vm.start.elapsed().as_nanos() as i128 + 1_000_000_000;
            if let Value::Arr(ref a) = arg(&args, 0) {
                let get = |i| a.get(i).to_number();
                t -= get(0) as i128 * 1_000_000_000 + get(1) as i128;
            }
            Value::Arr(crate::runtime::natives::make_array(vec![
                Value::Num((t / 1_000_000_000) as f64),
                Value::Num((t % 1_000_000_000) as f64),
            ]))
        }
        FsReadFile => match std::fs::read_to_string(path()) {
            Ok(t) => Value::str(t),
            Err(e) => return Err(sys_error(vm, e, "open", &path())),
        },
        FsWriteFile | FsAppendFile => {
            let data = s(&arg(&args, 1));
            let r = if n == FsWriteFile {
                std::fs::write(path(), data)
            } else {
                use std::io::Write;
                std::fs::OpenOptions::new().append(true).create(true).open(path()).and_then(|mut f| f.write_all(data.as_bytes()))
            };
            if let Err(e) = r {
                return Err(sys_error(vm, e, "open", &path()));
            }
            Value::Undef
        }
        FsExists => Value::Bool(std::path::Path::new(&path()).exists()),
        FsMkdir => {
            let r = if arg(&args, 1).truthy() { std::fs::create_dir_all(path()) } else { std::fs::create_dir(path()) };
            if let Err(e) = r {
                return Err(sys_error(vm, e, "mkdir", &path()));
            }
            Value::Undef
        }
        FsReaddir => match std::fs::read_dir(path()) {
            Ok(rd) => {
                let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                names.sort();
                Value::Arr(make_array(names.into_iter().map(Value::str).collect()))
            }
            Err(e) => return Err(sys_error(vm, e, "scandir", &path())),
        },
        FsStat => match std::fs::metadata(path()) {
            Ok(m) => {
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64() * 1000.0);
                obj(
                    &["size", "mtimeMs", "file", "directory"],
                    vec![Value::Num(m.len() as f64), Value::Num(mtime), Value::Bool(m.is_file()), Value::Bool(m.is_dir())],
                )
            }
            Err(e) => return Err(sys_error(vm, e, "stat", &path())),
        },
        FsUnlink => {
            if let Err(e) = std::fs::remove_file(path()) {
                return Err(sys_error(vm, e, "unlink", &path()));
            }
            Value::Undef
        }
        FsRm => {
            let p = path();
            let meta = std::fs::metadata(&p);
            let r = match meta {
                Ok(m) if m.is_dir() && arg(&args, 1).truthy() => std::fs::remove_dir_all(&p),
                Ok(m) if m.is_dir() => std::fs::remove_dir(&p),
                Ok(_) => std::fs::remove_file(&p),
                Err(_) if arg(&args, 2).truthy() => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                return Err(sys_error(vm, e, "rm", &p));
            }
            Value::Undef
        }
        FsRename | FsCopy => {
            let (a, b) = (path(), s(&arg(&args, 1)));
            let r = if n == FsRename { std::fs::rename(&a, &b) } else { std::fs::copy(&a, &b).map(|_| ()) };
            if let Err(e) = r {
                return Err(sys_error(vm, e, if n == FsRename { "rename" } else { "copyfile" }, &a));
            }
            Value::Undef
        }
        FsRealpath => match std::fs::canonicalize(path()) {
            Ok(p) => Value::str(p.to_string_lossy().into_owned()),
            Err(e) => return Err(sys_error(vm, e, "realpath", &path())),
        },
        OsInfo => {
            let home = std::env::var("HOME").unwrap_or_default();
            let tmp = std::env::var("TMPDIR").map(|t| t.trim_end_matches('/').to_string()).unwrap_or_else(|_| "/tmp".into());
            let host = std::process::Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
            obj(
                &["platform", "homedir", "tmpdir", "hostname", "cpus", "arch"],
                vec![
                    Value::str(platform()),
                    Value::str(home),
                    Value::str(tmp),
                    Value::str(host),
                    Value::Num(cpus as f64),
                    Value::str(arch()),
                ],
            )
        }
        UtilFormat => Value::str(format_args(vm, &args)),
        UtilInspect => {
            let depth = match arg(&args, 1) {
                Value::Num(d) => d as usize,
                Value::Null => usize::MAX,
                _ => 2,
            };
            Value::str(vm.inspect_depth(&arg(&args, 0), depth))
        }
        EnvObject => {
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            let mut vars: Vec<(String, String)> = std::env::vars().collect();
            vars.sort();
            for (k, v) in vars {
                keys.push(Rc::from(k.as_str()));
                vals.push(Value::str(v));
            }
            Value::Obj(Object::new(Shape::new(keys), vals, None))
        }
        _ => unreachable!("not a node native: {n:?}"),
    })
}

pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    }
}

pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        a => a,
    }
}

/// `util.format` (and `console.log` with a format string).
pub fn format_args(vm: &mut Vm, args: &[Value]) -> String {
    let Some(Value::Str(f)) = args.first() else {
        return args.iter().map(|a| vm.inspect(a)).collect::<Vec<_>>().join(" ");
    };
    if !f.s.contains('%') {
        return args.iter().map(|a| vm.inspect(a)).collect::<Vec<_>>().join(" ");
    }
    let mut out = String::new();
    let mut rest = args[1..].iter();
    let cs: Vec<char> = f.s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '%' && i + 1 < cs.len() {
            let c = cs[i + 1];
            let take = matches!(c, 's' | 'd' | 'i' | 'f' | 'j' | 'o' | 'O' | 'c');
            if c == '%' {
                out.push('%');
                i += 2;
                continue;
            }
            if take && let Some(v) = rest.next() {
                match c {
                    's' => out.push_str(&match v {
                        Value::Str(s) => s.s.to_string(),
                        Value::Obj(_) | Value::Arr(_) => vm.inspect_depth(v, 1),
                        Value::BigInt(n) => format!("{n}n"),
                        v => v.to_display(),
                    }),
                    'd' | 'i' => {
                        let n = v.to_number();
                        out.push_str(&num_to_string(if c == 'i' { n.trunc() } else { n }));
                    }
                    'f' => out.push_str(&num_to_string(v.to_number())),
                    'j' => {
                        let mut j = String::new();
                        crate::runtime::natives::json_stringify(v, "", "", &mut j);
                        out.push_str(&j);
                    }
                    'c' => {}
                    _ => out.push_str(&vm.inspect(v)),
                }
                i += 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    for v in rest {
        out.push(' ');
        out.push_str(&match v {
            Value::Str(s) => s.s.to_string(),
            v => vm.inspect(v),
        });
    }
    out
}

// ───────────── dates ─────────────

#[repr(C)]
struct Tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const std::ffi::c_char,
}

unsafe extern "C" {
    fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
}

fn local_tm(ms: f64) -> Option<Tm> {
    let secs = (ms / 1000.0).floor() as i64;
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    let r = unsafe { localtime_r(&secs, &mut tm) };
    (!r.is_null()).then_some(tm)
}

/// `getTimezoneOffset()` at time `ms`: UTC minus local time, in minutes.
pub fn tz_offset(ms: f64) -> f64 {
    if !ms.is_finite() {
        return f64::NAN;
    }
    local_tm(ms).map_or(0.0, |tm| -(tm.tm_gmtoff as f64) / 60.0)
}

pub fn tz_name(ms: f64) -> String {
    let abbr = local_tm(ms)
        .filter(|tm| !tm.tm_zone.is_null())
        .map(|tm| unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned())
        .unwrap_or_else(|| "UTC".into());
    match abbr.as_str() {
        "UTC" | "GMT" => "Coordinated Universal Time".into(),
        "PST" => "Pacific Standard Time".into(),
        "PDT" => "Pacific Daylight Time".into(),
        "MST" => "Mountain Standard Time".into(),
        "MDT" => "Mountain Daylight Time".into(),
        "CST" => "Central Standard Time".into(),
        "CDT" => "Central Daylight Time".into(),
        "EST" => "Eastern Standard Time".into(),
        "EDT" => "Eastern Daylight Time".into(),
        "CET" => "Central European Standard Time".into(),
        "CEST" => "Central European Summer Time".into(),
        "BST" => "British Summer Time".into(),
        "IST" => "India Standard Time".into(),
        "JST" => "Japan Standard Time".into(),
        _ => abbr,
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `Date.parse`: ISO 8601 forms (date-only is UTC, date-time without a zone is local).
pub fn parse_date(s: &str) -> f64 {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse().ok()) };
    let (year, mut i) = match b.first() {
        Some(b'+') | Some(b'-') => {
            let y = num(1, 6).map(|y| if b[0] == b'-' { -y } else { y });
            (y, 7)
        }
        _ => (num(0, 4), 4),
    };
    let Some(year) = year else { return f64::NAN };
    let (mut mon, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        let Some(m) = num(i + 1, 2) else { return f64::NAN };
        mon = m;
        i += 3;
        if b.get(i) == Some(&b'-') {
            let Some(d) = num(i + 1, 2) else { return f64::NAN };
            day = d;
            i += 3;
        }
    }
    let (mut h, mut mi, mut sec, mut ms) = (0, 0, 0.0, 0.0);
    let mut has_time = false;
    if matches!(b.get(i), Some(b'T') | Some(b't') | Some(b' ')) {
        has_time = true;
        let (Some(hh), Some(mm)) = (num(i + 1, 2), num(i + 4, 2)) else { return f64::NAN };
        if b.get(i + 3) != Some(&b':') {
            return f64::NAN;
        }
        h = hh;
        mi = mm;
        i += 6;
        if b.get(i) == Some(&b':') {
            let Some(ss) = num(i + 1, 2) else { return f64::NAN };
            sec = ss as f64;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut end = start;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                let frac = &s[start..end];
                ms = format!("0.{frac}").parse::<f64>().unwrap_or(0.0) * 1000.0;
                ms = ms.floor();
                i = end;
            }
        }
    }
    let mut offset_min: Option<i64> = None;
    match b.get(i) {
        None => {}
        Some(b'Z') | Some(b'z') if i + 1 == b.len() => offset_min = Some(0),
        Some(c @ (b'+' | b'-')) => {
            let (Some(oh), Some(om)) = (num(i + 1, 2), num(i + 4, 2).or_else(|| num(i + 3, 2))) else { return f64::NAN };
            let o = oh * 60 + om;
            offset_min = Some(if *c == b'+' { o } else { -o });
        }
        _ => return f64::NAN,
    }
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || h > 24 || mi > 59 || sec >= 60.0 {
        return f64::NAN;
    }
    let days = days_from_civil(year, mon, day);
    let t = days as f64 * 86400000.0 + (h * 3600000 + mi * 60000) as f64 + sec * 1000.0 + ms;
    match offset_min {
        Some(o) => t - o as f64 * 60000.0,
        None if !has_time => t,
        None => {
            let guess = t + tz_offset(t) * 60000.0;
            t + tz_offset(guess) * 60000.0
        }
    }
}

/// `util.isDeepStrictEqual`
pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y && x.is_sign_negative() == y.is_sign_negative() || (x.is_nan() && y.is_nan()),
        (Value::Arr(x), Value::Arr(y)) => {
            let (x, y) = (x.to_vec(), y.to_vec());
            x.len() == y.len() && x.iter().zip(&y).all(|(p, q)| deep_equal(p, q))
        }
        (Value::Obj(x), Value::Obj(y)) => {
            if Gc::ptr_eq(x, y) {
                return true;
            }
            let same_class = match (&x.class, &y.class) {
                (Some(c), Some(d)) => Rc::ptr_eq(c, d),
                (None, None) => true,
                _ => false,
            };
            let (kx, ky) = (x.keys(), y.keys());
            same_class
                && kx.len() == ky.len()
                && kx.iter().all(|k| match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => deep_equal(&p, &q),
                    _ => false,
                })
        }
        (Value::Map(x), Value::Map(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| deep_equal(v, w)))
        }
        (Value::Set(x), Value::Set(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().all(|k| y.contains(k))
        }
        (a, b) => a.strict_eq(b),
    }
}

/// `encodeURIComponent` (keep = extra characters left unescaped).
pub fn uri_encode(s: &str, keep: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) || keep.contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

pub fn uri_decode(s: &str, reserved: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s.get(i + 1..i + 3)?;
            let byte = u8::from_str_radix(h, 16).ok()?;
            if byte < 0x80 && reserved.contains(byte as char) {
                out.extend_from_slice(&b[i..i + 3]);
            } else {
                out.push(byte);
            }
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn call_more(vm: &mut Vm, n: Native, args: Vec<Value>) -> Result<Value, Value> {
    use Native::*;
    Ok(match n {
        DateNowMs => Value::Num(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64),
        ),
        DateOffset => Value::Num(tz_offset(arg(&args, 0).to_number())),
        DateZoneName => Value::str(tz_name(arg(&args, 0).to_number())),
        DateParse => Value::Num(parse_date(&s(&arg(&args, 0)))),
        DeepEqual => Value::Bool(deep_equal(&arg(&args, 0), &arg(&args, 1))),
        ObjectIs => {
            let (a, b) = (arg(&args, 0), arg(&args, 1));
            Value::Bool(match (&a, &b) {
                (Value::Num(x), Value::Num(y)) => (x.is_nan() && y.is_nan()) || (x == y && x.is_sign_negative() == y.is_sign_negative()),
                _ => a.strict_eq(&b),
            })
        }
        EncodeUriComponent => Value::str(uri_encode(&s(&arg(&args, 0)), "")),
        EncodeUri => Value::str(uri_encode(&s(&arg(&args, 0)), ";,/?:@&=+$#")),
        DecodeUriComponent | DecodeUri => {
            let reserved = if n == DecodeUri { ";,/?:@&=+$#" } else { "" };
            match uri_decode(&s(&arg(&args, 0)), reserved) {
                Some(t) => Value::str(t),
                None => {
                    let e = vm.make_error_pub("URI malformed".into());
                    if let Value::Obj(o) = &e {
                        o.set(&"name".into(), Value::str("URIError"));
                    }
                    return Err(e);
                }
            }
        }
        NoOp => Value::Undef,
        n if (n as u16) >= (ConsoleDir as u16) => return call_console_object(vm, n, args),
        _ => return call(vm, n, Value::Undef, args),
    })
}

/// console.group indentation, counters and timers; Object helpers.
pub fn call_console_object(vm: &mut Vm, n: Native, args: Vec<Value>) -> Result<Value, Value> {
    use Native::*;
    let label = |args: &[Value]| match args.first() {
        None | Some(Value::Undef) => "default".to_string(),
        Some(v) => s(v),
    };
    Ok(match n {
        ConsoleDir => {
            let line = vm.inspect(&arg(&args, 0));
            vm.print(&line);
            Value::Undef
        }
        ConsoleGroup => {
            if !args.is_empty() {
                let line = format_args(vm, &args);
                vm.print(&line);
            }
            vm.console_indent += 2;
            Value::Undef
        }
        ConsoleGroupEnd => {
            vm.console_indent = vm.console_indent.saturating_sub(2);
            Value::Undef
        }
        ConsoleAssert => {
            if !arg(&args, 0).truthy() {
                let rest = &args[args.len().min(1)..];
                let msg = if rest.is_empty() { "Assertion failed".to_string() } else { format!("Assertion failed: {}", format_args(vm, rest)) };
                vm.eprint(&msg);
            }
            Value::Undef
        }
        ConsoleCount => {
            let l = label(&args);
            let c = vm.console_counts.entry(l.clone()).or_insert(0);
            *c += 1;
            let line = format!("{l}: {c}");
            vm.print(&line);
            Value::Undef
        }
        ConsoleCountReset => {
            vm.console_counts.remove(&label(&args));
            Value::Undef
        }
        ConsoleTime => {
            let now = vm.now_ms();
            vm.console_timers.insert(label(&args), now);
            Value::Undef
        }
        ConsoleTimeEnd => {
            let l = label(&args);
            if let Some(t) = vm.console_timers.get(&l).copied() {
                let ms = vm.now_ms() - t;
                let line = format!("{l}: {:.3}ms", ms);
                vm.print(&line);
            }
            Value::Undef
        }
        ConsoleTable => {
            let t = console_table(vm, &arg(&args, 0));
            vm.print(&t);
            Value::Undef
        }
        ObjDefineProperty => {
            let (o, k, d) = (arg(&args, 0), arg(&args, 1), arg(&args, 2));
            let Value::Obj(target) = &o else {
                crate::runtime::natives::set_dyn(&o, &k, vm.get_property(&d, "value")?);
                return Ok(o);
            };
            let key: Rc<str> = (&*k.to_js_string().s).into();
            let get = vm.get_property(&d, "get")?;
            let set = vm.get_property(&d, "set")?;
            if !matches!(get, Value::Undef) || !matches!(set, Value::Undef) {
                target.set(&key, Value::Accessor(Gc::new(crate::runtime::task::Accessor { get, set })));
            } else {
                let v = vm.get_property(&d, "value")?;
                target.set(&key, v);
            }
            o
        }
        ObjCreate => match arg(&args, 0) {
            Value::Null => Value::Obj(Object::new(Shape::new_null_proto(vec![]), vec![], None)),
            Value::Obj(ref p) => Value::Obj(Object::new(Shape::with_proto(p.clone()), vec![], None)),
            _ => Value::Obj(Object::new(Shape::new(vec![]), vec![], None)),
        },
        ObjGetPrototypeOf => match arg(&args, 0) {
            Value::Obj(ref o) if o.inner.borrow().shape.null_proto => Value::Null,
            Value::Obj(ref o) if o.proto().is_some() => Value::Obj(o.proto().unwrap()),
            Value::Obj(ref o) if o.class.is_some() => Value::Class(o.class.clone().unwrap()),
            _ => Value::Obj(Object::new(Shape::new(vec![]), vec![], None)),
        },
        ObjSetPrototypeOf => {
            let o = arg(&args, 0);
            if let Value::Obj(ob) = &o {
                let mut inner = ob.inner.borrow_mut();
                inner.shape = match arg(&args, 1) {
                    Value::Obj(ref p) => inner.shape.reproto(Some(p.clone()), false),
                    Value::Null => inner.shape.reproto(None, true),
                    _ => inner.shape.reproto(None, false),
                };
            }
            o
        }
        ObjIsFrozen => Value::Bool(false),
        ObjHasOwn => {
            let k = arg(&args, 1).to_js_string();
            Value::Bool(match arg(&args, 0) {
                Value::Obj(ref o) => o.get(&k.s).is_some(),
                Value::Arr(ref a) => k.s.parse::<usize>().is_ok_and(|i| i < a.len()),
                _ => false,
            })
        }
        ObjGroupBy => {
            let items = crate::runtime::natives::iterate(vm, &arg(&args, 0))?;
            let f = arg(&args, 1);
            let out = Object::new(Shape::new_null_proto(vec![]), vec![], None);
            for (i, x) in items.into_iter().enumerate() {
                let k = vm.call(&f, Value::Undef, vec![x.clone(), Value::Num(i as f64)])?;
                let key: Rc<str> = (&*k.to_js_string().s).into();
                match out.get(&key) {
                    Some(Value::Arr(ref a)) => a.push(x),
                    _ => out.set(&key, Value::Arr(make_array(vec![x]))),
                }
            }
            Value::Obj(out)
        }
        StrRaw => {
            // String.raw(strings, ...subs)
            let strings = arg(&args, 0);
            let raw = vm.get_property(&strings, "raw")?;
            let parts = crate::runtime::natives::iterate(vm, &raw)?;
            let mut out = String::new();
            for (i, p) in parts.iter().enumerate() {
                out.push_str(&s(p));
                if i + 1 < parts.len()
                    && let Some(v) = args.get(i + 1)
                {
                    out.push_str(&s(v));
                }
            }
            Value::str(out)
        }
        _ => unreachable!(),
    })
}

/// `console.table` for arrays of objects/values.
fn console_table(vm: &mut Vm, data: &Value) -> String {
    let rows: Vec<(String, Value)> = match data {
        Value::Arr(a) => a.to_vec().into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect(),
        Value::Obj(o) => o.keys().into_iter().map(|k| (k.to_string(), o.get(&k).unwrap_or(Value::Undef))).collect(),
        v => return vm.inspect(v),
    };
    let mut cols: Vec<String> = Vec::new();
    let mut has_values = false;
    for (_, v) in &rows {
        match v {
            Value::Obj(o) => {
                for k in o.keys() {
                    if !cols.iter().any(|c| **c == *k) {
                        cols.push(k.to_string());
                    }
                }
            }
            _ => has_values = true,
        }
    }
    let mut header = vec!["(index)".to_string()];
    header.extend(cols.iter().cloned());
    if has_values {
        header.push("Values".into());
    }
    let mut table: Vec<Vec<String>> = Vec::new();
    for (i, v) in &rows {
        let mut line = vec![i.clone()];
        for c in &cols {
            line.push(match v {
                Value::Obj(o) => o.get(c).map(|x| vm.inspect_depth(&x, 0)).unwrap_or_default(),
                _ => String::new(),
            });
        }
        if has_values {
            line.push(if matches!(v, Value::Obj(_)) { String::new() } else { vm.inspect(v) });
        }
        table.push(line);
    }
    let width = |s: &str| s.chars().count();
    let widths: Vec<usize> = (0..header.len())
        .map(|c| std::iter::once(&header[c]).chain(table.iter().map(|r| &r[c])).map(|s| width(s) + 2).max().unwrap_or(2))
        .collect();
    let line = |l: &str, m: &str, r: &str| format!("{l}{}{r}", widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join(m));
    let row = |cells: &[String]| {
        format!(
            "│{}│",
            cells
                .iter()
                .zip(&widths)
                .map(|(c, w)| format!(" {c}{} ", " ".repeat(w - 2 - width(c))))
                .collect::<Vec<_>>()
                .join("│")
        )
    };
    let mut out = vec![line("┌", "┬", "┐"), row(&header), line("├", "┼", "┤")];
    for r in &table {
        out.push(row(r));
    }
    out.push(line("└", "┴", "┘"));
    out.join("\n")
}
