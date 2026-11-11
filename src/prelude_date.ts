// Date, compiled only into programs that mention `Date`.

// ───────────── Date ─────────────
// Time is milliseconds since the epoch (NaN for an invalid date). Calendar
// arithmetic is done here; the time zone offset comes from the OS.

const __rts_days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const __rts_months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function __rts_days_from_civil(y: number, m: number, d: number): number {
  y -= m <= 2 ? 1 : 0;
  const era = Math.floor(y / 400);
  const yoe = y - era * 400;
  const doy = Math.floor((153 * (m + (m > 2 ? -3 : 9)) + 2) / 5) + d - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}

function __rts_make_time(y: number, mon: number, d: number, h: number, mi: number, s: number, ms: number): number {
  if (!isFinite(y) || !isFinite(mon) || !isFinite(d) || !isFinite(h) || !isFinite(mi) || !isFinite(s) || !isFinite(ms)) return NaN;
  y = Math.trunc(y);
  mon = Math.trunc(mon);
  y += Math.floor(mon / 12);
  mon = ((mon % 12) + 12) % 12;
  const days = __rts_days_from_civil(y, mon + 1, 1) + Math.trunc(d) - 1;
  const t = days * 86400000 + Math.trunc(h) * 3600000 + Math.trunc(mi) * 60000 + Math.trunc(s) * 1000 + Math.trunc(ms);
  return Math.abs(t) > 8.64e15 ? NaN : t;
}

/// [year, month (0-11), day, hours, minutes, seconds, ms, weekday]
function __rts_fields(t: number): number[] {
  const days = Math.floor(t / 86400000);
  let rem = t - days * 86400000;
  const z = days + 719468;
  const era = Math.floor(z / 146097);
  const doe = z - era * 146097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const d = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const m = mp < 10 ? mp + 3 : mp - 9;
  const y = yoe + era * 400 + (m <= 2 ? 1 : 0);
  const h = Math.floor(rem / 3600000);
  rem -= h * 3600000;
  const mi = Math.floor(rem / 60000);
  rem -= mi * 60000;
  const s = Math.floor(rem / 1000);
  const wd = (((days + 4) % 7) + 7) % 7;
  return [y, m - 1, d, h, mi, s, rem - s * 1000, wd];
}

function __rts_pad(n: number, w: number): string {
  const s = String(Math.abs(n));
  return (n < 0 ? "-" : "") + "0".repeat(Math.max(0, w - s.length)) + s;
}

class Date {
  private t: number;
  constructor(a?: any, b?: number, c?: number, d?: number, e?: number, f?: number, g?: number) {
    if (a === undefined) {
      this.t = __rts_now();
    } else if (b === undefined) {
      if (typeof a === "number") this.t = Math.abs(a) > 8.64e15 ? NaN : Math.trunc(a);
      else if (a instanceof Date) this.t = a.getTime();
      else this.t = __rts_date_parse(String(a));
    } else {
      this.t = Date.fromLocal(__rts_make_time(a, b, c ?? 1, d ?? 0, e ?? 0, f ?? 0, g ?? 0));
    }
  }
  private static fromLocal(local: number): number {
    if (isNaN(local)) return NaN;
    const guess = local + __rts_date_offset(local) * 60000;
    return local + __rts_date_offset(guess) * 60000;
  }
  static now(): number {
    return __rts_now();
  }
  static parse(s: string): number {
    return __rts_date_parse(String(s));
  }
  static UTC(y: number, m?: number, d?: number, h?: number, mi?: number, s?: number, ms?: number): number {
    return __rts_make_time(y, m ?? 0, d ?? 1, h ?? 0, mi ?? 0, s ?? 0, ms ?? 0);
  }
  getTime(): number {
    return this.t;
  }
  valueOf(): number {
    return this.t;
  }
  getTimezoneOffset(): number {
    return isNaN(this.t) ? NaN : __rts_date_offset(this.t);
  }
  private local(): number[] {
    return __rts_fields(this.t - this.getTimezoneOffset() * 60000);
  }
  private utc(): number[] {
    return __rts_fields(this.t);
  }
  private field(i: number, utc: boolean): number {
    if (isNaN(this.t)) return NaN;
    return (utc ? this.utc() : this.local())[i];
  }
  getFullYear(): number { return this.field(0, false); }
  getMonth(): number { return this.field(1, false); }
  getDate(): number { return this.field(2, false); }
  getHours(): number { return this.field(3, false); }
  getMinutes(): number { return this.field(4, false); }
  getSeconds(): number { return this.field(5, false); }
  getMilliseconds(): number { return this.field(6, false); }
  getDay(): number { return this.field(7, false); }
  getUTCFullYear(): number { return this.field(0, true); }
  getUTCMonth(): number { return this.field(1, true); }
  getUTCDate(): number { return this.field(2, true); }
  getUTCHours(): number { return this.field(3, true); }
  getUTCMinutes(): number { return this.field(4, true); }
  getUTCSeconds(): number { return this.field(5, true); }
  getUTCMilliseconds(): number { return this.field(6, true); }
  getUTCDay(): number { return this.field(7, true); }
  setTime(t: number): number {
    this.t = Math.trunc(t);
    return this.t;
  }
  private set(i: number, v: number, utc: boolean): number {
    const f = utc ? this.utc() : this.local();
    f[i] = v;
    const t = __rts_make_time(f[0], f[1], f[2], f[3], f[4], f[5], f[6]);
    this.t = utc ? t : Date.fromLocal(t);
    return this.t;
  }
  setFullYear(v: number): number { return this.set(0, v, false); }
  setMonth(v: number): number { return this.set(1, v, false); }
  setDate(v: number): number { return this.set(2, v, false); }
  setHours(v: number): number { return this.set(3, v, false); }
  setMinutes(v: number): number { return this.set(4, v, false); }
  setSeconds(v: number): number { return this.set(5, v, false); }
  setMilliseconds(v: number): number { return this.set(6, v, false); }
  setUTCFullYear(v: number): number { return this.set(0, v, true); }
  setUTCMonth(v: number): number { return this.set(1, v, true); }
  setUTCDate(v: number): number { return this.set(2, v, true); }
  setUTCHours(v: number): number { return this.set(3, v, true); }
  setUTCMinutes(v: number): number { return this.set(4, v, true); }
  setUTCSeconds(v: number): number { return this.set(5, v, true); }
  setUTCMilliseconds(v: number): number { return this.set(6, v, true); }
  toISOString(): string {
    if (isNaN(this.t)) throw new RangeError("Invalid time value");
    const f = this.utc();
    const y = f[0] >= 0 && f[0] <= 9999 ? __rts_pad(f[0], 4) : (f[0] < 0 ? "-" : "+") + __rts_pad(Math.abs(f[0]), 6);
    return y + "-" + __rts_pad(f[1] + 1, 2) + "-" + __rts_pad(f[2], 2) + "T" + __rts_pad(f[3], 2) + ":" + __rts_pad(f[4], 2) + ":" + __rts_pad(f[5], 2) + "." + __rts_pad(f[6], 3) + "Z";
  }
  toJSON(): string | null {
    return isNaN(this.t) ? null : this.toISOString();
  }
  private zone(): string {
    const off = -this.getTimezoneOffset();
    const sign = off >= 0 ? "+" : "-";
    const a = Math.abs(off);
    return "GMT" + sign + __rts_pad(Math.floor(a / 60), 2) + __rts_pad(a % 60, 2) + " (" + __rts_date_zone_name(this.t) + ")";
  }
  toDateString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    const f = this.local();
    return __rts_days[f[7]] + " " + __rts_months[f[1]] + " " + __rts_pad(f[2], 2) + " " + __rts_pad(f[0], 4);
  }
  toTimeString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    const f = this.local();
    return __rts_pad(f[3], 2) + ":" + __rts_pad(f[4], 2) + ":" + __rts_pad(f[5], 2) + " " + this.zone();
  }
  toString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    return this.toDateString() + " " + this.toTimeString();
  }
  toUTCString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    const f = this.utc();
    return __rts_days[f[7]] + ", " + __rts_pad(f[2], 2) + " " + __rts_months[f[1]] + " " + __rts_pad(f[0], 4) + " " + __rts_pad(f[3], 2) + ":" + __rts_pad(f[4], 2) + ":" + __rts_pad(f[5], 2) + " GMT";
  }
  toLocaleDateString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    const f = this.local();
    return f[1] + 1 + "/" + f[2] + "/" + f[0];
  }
  toLocaleTimeString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    const f = this.local();
    const h = f[3] % 12 === 0 ? 12 : f[3] % 12;
    return h + ":" + __rts_pad(f[4], 2) + ":" + __rts_pad(f[5], 2) + (f[3] < 12 ? " AM" : " PM");
  }
  toLocaleString(): string {
    if (isNaN(this.t)) return "Invalid Date";
    return this.toLocaleDateString() + ", " + this.toLocaleTimeString();
  }
}
