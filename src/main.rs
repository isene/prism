//! prism — TUI RGB color picker.
//!
//! Two color slots (FG / BG), Tab to swap which one is being edited.
//! R/G/B sliders, H/S/V sliders, hex input. Live sample-text area
//! shows the current FG-on-BG pair against realistic content
//! (heading, code, comments, body, link). WCAG contrast ratio is
//! shown so theme work doesn't accidentally produce unreadable pairs.
//!
//! CLI:
//!   prism                 standalone, prints both colors on quit
//!   prism --pick          pick one color (focused on FG), prints hex
//!   prism --pair          pick both, prints "fg=#... bg=#..." (default)
//!   prism #f74c00         start with FG preloaded; second arg = BG hex
//!   prism --rgb           output as rgb(R, G, B)
//!   prism --hsv           output as hsv(H, S, V)
//!   prism --all           output hex / rgb / hsv lines for both slots
//!
//! Pure ANSI 24-bit truecolor — works in glass, wezterm, kitty, foot,
//! tmux (with `set -g default-terminal "tmux-256color"` and Tc).

use std::io::Write;

use crust::{Crust, Input};

// ─────────────────────────── color math ──────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Rgb { r: u8, g: u8, b: u8 }

#[derive(Clone, Copy, Debug)]
struct Hsv { h: f32, s: f32, v: f32 }  // h: 0..360, s: 0..1, v: 0..1

impl Rgb {
    fn hex(&self) -> String { format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b) }
    fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 { return None; }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Rgb { r, g, b })
    }
    fn to_hsv(&self) -> Hsv {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let d = mx - mn;
        let h = if d == 0.0 { 0.0 }
                else if mx == r { 60.0 * (((g - b) / d) % 6.0) }
                else if mx == g { 60.0 * (((b - r) / d) + 2.0) }
                else { 60.0 * (((r - g) / d) + 4.0) };
        let h = if h < 0.0 { h + 360.0 } else { h };
        let s = if mx == 0.0 { 0.0 } else { d / mx };
        Hsv { h, s, v: mx }
    }
    /// Relative luminance (WCAG 2.x).
    fn luminance(&self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }
}

impl Hsv {
    fn to_rgb(&self) -> Rgb {
        let c = self.v * self.s;
        let h6 = self.h / 60.0;
        let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
        let (r1, g1, b1) = match h6 as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = self.v - c;
        Rgb {
            r: ((r1 + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
            g: ((g1 + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
            b: ((b1 + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
        }
    }
}

fn contrast_ratio(a: &Rgb, b: &Rgb) -> f32 {
    let la = a.luminance() + 0.05;
    let lb = b.luminance() + 0.05;
    if la > lb { la / lb } else { lb / la }
}

fn wcag_label(ratio: f32) -> (&'static str, &'static str) {
    if ratio >= 7.0 { ("AAA", "✓") }
    else if ratio >= 4.5 { ("AA",  "✓") }
    else if ratio >= 3.0 { ("AA·", "·") }
    else { ("✗ low", "✗") }
}

// ─────────────────────────── ANSI helpers ────────────────────────────

fn fg_esc(c: &Rgb) -> String { format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b) }
fn bg_esc(c: &Rgb) -> String { format!("\x1b[48;2;{};{};{}m", c.r, c.g, c.b) }
const RESET: &str = "\x1b[0m";

fn move_to(row: u16, col: u16) -> String { format!("\x1b[{};{}H", row, col) }

// ─────────────────────────── app state ───────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Slot { Fg, Bg }

#[derive(Clone, Copy, PartialEq)]
enum Channel { R, G, B, H, S, V }

#[derive(Clone, Copy, PartialEq)]
enum OutputFmt { Hex, Rgb, Hsv, All }

#[derive(Clone, Copy, PartialEq)]
enum Mode { Pick, Pair }

struct App {
    fg: Rgb,
    bg: Rgb,
    editing: Slot,
    channel: Channel,
    out_fmt: OutputFmt,
    mode: Mode,
    status: String,
}

impl App {
    fn current(&self) -> Rgb {
        match self.editing { Slot::Fg => self.fg, Slot::Bg => self.bg }
    }
    fn set_current(&mut self, c: Rgb) {
        match self.editing { Slot::Fg => self.fg = c, Slot::Bg => self.bg = c }
    }
    fn step(&mut self, delta: i32) {
        let mut c = self.current();
        let mut hsv = c.to_hsv();
        match self.channel {
            Channel::R => c.r = (c.r as i32 + delta).clamp(0, 255) as u8,
            Channel::G => c.g = (c.g as i32 + delta).clamp(0, 255) as u8,
            Channel::B => c.b = (c.b as i32 + delta).clamp(0, 255) as u8,
            Channel::H => {
                let mut h = hsv.h + delta as f32;
                while h < 0.0 { h += 360.0; }
                while h >= 360.0 { h -= 360.0; }
                hsv.h = h;
                c = hsv.to_rgb();
            }
            Channel::S => {
                hsv.s = (hsv.s + delta as f32 / 100.0).clamp(0.0, 1.0);
                c = hsv.to_rgb();
            }
            Channel::V => {
                hsv.v = (hsv.v + delta as f32 / 100.0).clamp(0.0, 1.0);
                c = hsv.to_rgb();
            }
        }
        self.set_current(c);
    }
}

// ─────────────────────────── rendering ───────────────────────────────

fn slider(label: &str, value: i32, max: i32, focused: bool, color_hint: &Rgb) -> String {
    let width = 22;
    let filled = (value as f32 / max as f32 * width as f32).round() as usize;
    let bar: String = (0..width).map(|i| if i < filled { '█' } else { '░' }).collect();
    let bar = format!("{}{}{}", fg_esc(color_hint), bar, RESET);
    let lbl = if focused {
        format!("\x1b[1;33m{}\x1b[0m", label)  // bold yellow
    } else {
        format!("\x1b[2m{}\x1b[0m", label)     // dim
    };
    format!("{} [{}] {:>3}", lbl, bar, value)
}

fn render(app: &App, cols: u16, rows: u16) {
    // Build entire frame in a String, then write at once.
    let mut s = String::new();
    s.push_str("\x1b[H");          // move home
    s.push_str("\x1b[J");          // clear from cursor

    let editing_label = match app.editing { Slot::Fg => "FG", Slot::Bg => "BG" };

    // Title
    s.push_str(&move_to(2, 3));
    s.push_str(&format!("\x1b[1;38;2;247;76;0mprism\x1b[0m  TUI color picker  \x1b[2m(editing: {})\x1b[0m",
        editing_label));

    // Slot blocks at rows 4-9
    let fg_focus = app.editing == Slot::Fg;
    let bg_focus = app.editing == Slot::Bg;
    let slot_w = 16u16;
    let fg_x = 3;
    let bg_x = fg_x + slot_w + 4;

    let frame = |x: u16, y: u16, w: u16, h: u16, fill: &Rgb, label: &str, hex: &str, focused: bool| -> String {
        let mut out = String::new();
        // Outer border (focused = bright rust, else dim)
        let bcolor = if focused { "\x1b[38;2;255;122;58m" } else { "\x1b[2m" };
        out.push_str(&move_to(y, x));
        out.push_str(bcolor);
        out.push('┌');
        let mid = if focused { format!(" {} ", label) } else { format!(" {} ", label) };
        let pad = (w as usize).saturating_sub(2 + mid.len());
        out.push_str(&"─".repeat(pad / 2));
        out.push_str(&mid);
        out.push_str(&"─".repeat(pad - pad / 2));
        out.push('┐');
        for r in 1..h-1 {
            out.push_str(&move_to(y + r, x));
            out.push('│');
            out.push_str(RESET);
            out.push_str(&bg_esc(fill));
            out.push_str(&" ".repeat(w as usize - 2));
            out.push_str(RESET);
            out.push_str(bcolor);
            out.push('│');
        }
        out.push_str(&move_to(y + h - 1, x));
        out.push('└');
        let bottom_pad = (w as usize).saturating_sub(2 + hex.len() + 2);
        out.push_str(&"─".repeat(bottom_pad));
        out.push_str(&format!(" {} ", hex));
        out.push('┘');
        out.push_str(RESET);
        out
    };

    let slot_h = 6u16;
    s.push_str(&frame(fg_x, 4, slot_w, slot_h, &app.fg, "FG", &app.fg.hex(), fg_focus));
    s.push_str(&frame(bg_x, 4, slot_w, slot_h, &app.bg, "BG", &app.bg.hex(), bg_focus));

    // WCAG contrast indicator next to slots
    let ratio = contrast_ratio(&app.fg, &app.bg);
    let (lvl, mark) = wcag_label(ratio);
    s.push_str(&move_to(5, bg_x + slot_w + 3));
    s.push_str(&format!("\x1b[1mContrast:\x1b[0m {:.2}:1  {} {}", ratio, lvl, mark));
    s.push_str(&move_to(7, bg_x + slot_w + 3));
    s.push_str("\x1b[2mWCAG: AAA ≥ 7   AA ≥ 4.5   AA· ≥ 3\x1b[0m");

    // Sample-text area (rows 11-17)
    let sx = 3u16;
    let sy = 11u16;
    let sw = (cols.saturating_sub(6)).min(80);
    let sh = 7u16;
    // Top rule
    s.push_str(&move_to(sy, sx));
    s.push_str("\x1b[2m── Sample ");
    s.push_str(&"─".repeat((sw as usize).saturating_sub(11)));
    s.push_str("\x1b[0m");
    // Sample lines, painted with current fg+bg
    let lines: [&str; 5] = [
        "# The quick brown fox jumps over the lazy dog",
        "fn main() { println!(\"Hello, world!\"); }    // body comment",
        "*bold* /italic/ _underline_   →   link text",
        "Plain prose on the chosen background — sample, sample, sample.",
        "1234567890   !@#$%^&*()   abc DEF ghi JKL",
    ];
    for (i, ln) in lines.iter().enumerate() {
        s.push_str(&move_to(sy + 1 + i as u16, sx));
        s.push_str(&bg_esc(&app.bg));
        s.push_str(&fg_esc(&app.fg));
        // Pad to sw to make the bg fill the row
        let mut content = ln.to_string();
        let pad = (sw as usize).saturating_sub(content.chars().count());
        content.push_str(&" ".repeat(pad));
        s.push_str(&content);
        s.push_str(RESET);
    }
    // Bottom rule
    s.push_str(&move_to(sy + sh - 1, sx));
    s.push_str(&format!("\x1b[2m{}\x1b[0m", "─".repeat(sw as usize)));

    // Sliders for current slot
    let cur = app.current();
    let hsv = cur.to_hsv();
    let hint_r = Rgb { r: 255, g: 80, b: 80 };
    let hint_g = Rgb { r: 80, g: 220, b: 100 };
    let hint_b = Rgb { r: 90, g: 140, b: 255 };
    let hint_hue = cur;
    let hint_sat = cur;
    let hint_val = cur;

    let row_r = sy + sh + 1;
    s.push_str(&move_to(row_r, 3));
    s.push_str(&slider("R", cur.r as i32, 255, app.channel == Channel::R, &hint_r));
    s.push_str(&move_to(row_r, 45));
    s.push_str(&slider("H", hsv.h as i32, 359, app.channel == Channel::H, &hint_hue));

    s.push_str(&move_to(row_r + 1, 3));
    s.push_str(&slider("G", cur.g as i32, 255, app.channel == Channel::G, &hint_g));
    s.push_str(&move_to(row_r + 1, 45));
    s.push_str(&slider("S", (hsv.s * 100.0) as i32, 100, app.channel == Channel::S, &hint_sat));

    s.push_str(&move_to(row_r + 2, 3));
    s.push_str(&slider("B", cur.b as i32, 255, app.channel == Channel::B, &hint_b));
    s.push_str(&move_to(row_r + 2, 45));
    s.push_str(&slider("V", (hsv.v * 100.0) as i32, 100, app.channel == Channel::V, &hint_val));

    // Channel-model explanations under the sliders (full-width, blank
    // row gap above and between the two model groups).
    s.push_str(&move_to(row_r + 4, 3));
    s.push_str("\x1b[2mRGB — additive light. Each channel 0–255; mix the three primaries.\x1b[0m");
    s.push_str(&move_to(row_r + 5, 3));
    s.push_str("\x1b[2m  R = red     G = green     B = blue       (0,0,0)=black · (255,255,255)=white\x1b[0m");

    s.push_str(&move_to(row_r + 7, 3));
    s.push_str("\x1b[2mHSV — perceptual model, what humans intuitively reach for.\x1b[0m");
    s.push_str(&move_to(row_r + 8, 3));
    s.push_str("\x1b[2m  H = hue          0–360°   which color (0=red, 120=green, 240=blue)\x1b[0m");
    s.push_str(&move_to(row_r + 9, 3));
    s.push_str("\x1b[2m  S = saturation   0–100    vividness   (0 = grayscale, 100 = pure)\x1b[0m");
    s.push_str(&move_to(row_r + 10, 3));
    s.push_str("\x1b[2m  V = value        0–100    brightness  (0 = black, 100 = full)\x1b[0m");

    // Output line
    s.push_str(&move_to(row_r + 12, 3));
    let out_str = match app.out_fmt {
        OutputFmt::Hex => cur.hex(),
        OutputFmt::Rgb => format!("rgb({}, {}, {})", cur.r, cur.g, cur.b),
        OutputFmt::Hsv => format!("hsv({:.0}, {:.0}%, {:.0}%)", hsv.h, hsv.s * 100.0, hsv.v * 100.0),
        OutputFmt::All => format!("{}  rgb({}, {}, {})  hsv({:.0}, {:.0}%, {:.0}%)",
            cur.hex(), cur.r, cur.g, cur.b, hsv.h, hsv.s * 100.0, hsv.v * 100.0),
    };
    s.push_str(&format!("\x1b[1mHex:\x1b[0m {}    \x1b[2mout({}):\x1b[0m {}",
        cur.hex(),
        match app.out_fmt {
            OutputFmt::Hex => "hex", OutputFmt::Rgb => "rgb",
            OutputFmt::Hsv => "hsv", OutputFmt::All => "all",
        },
        out_str));

    // Help line
    let help_y = rows.saturating_sub(2);
    s.push_str(&move_to(help_y, 3));
    s.push_str("\x1b[2mTab swap FG/BG · r/g/b/h/s/v focus · j/k ±1 · J/K ±10 · # type hex · c copy · o output · q quit\x1b[0m");

    // Status line (right above help)
    if !app.status.is_empty() {
        s.push_str(&move_to(help_y - 1, 3));
        s.push_str(&format!("\x1b[1;38;2;90;200;255m{}\x1b[0m", app.status));
    }

    // Hide cursor
    s.push_str("\x1b[?25l");

    print!("{}", s);
    let _ = std::io::stdout().flush();
}

// ─────────────────────────── main ────────────────────────────────────

fn main() {
    // Defaults: rust orange on pure black.
    let mut app = App {
        fg: Rgb { r: 247, g: 76, b: 0 },
        bg: Rgb { r: 0, g: 0, b: 0 },
        editing: Slot::Fg,
        channel: Channel::R,
        out_fmt: OutputFmt::Hex,
        mode: Mode::Pair,
        status: String::new(),
    };

    // Parse argv.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pick" => { app.mode = Mode::Pick; }
            "--pair" => { app.mode = Mode::Pair; }
            "--rgb"  => { app.out_fmt = OutputFmt::Rgb; }
            "--hsv"  => { app.out_fmt = OutputFmt::Hsv; }
            "--hex"  => { app.out_fmt = OutputFmt::Hex; }
            "--all"  => { app.out_fmt = OutputFmt::All; }
            "-h" | "--help" => {
                println!("prism — TUI RGB color picker");
                println!();
                println!("Usage: prism [--pick|--pair] [--hex|--rgb|--hsv|--all] [HEX [HEX]]");
                println!();
                println!("  --pick           pick a single color (FG slot)");
                println!("  --pair           pick FG + BG (default)");
                println!("  --hex/--rgb/--hsv/--all  output format on quit");
                println!("  HEX HEX          preload FG and BG with hex codes");
                return;
            }
            s if s.starts_with('#') || s.len() == 6 => {
                if let Some(c) = Rgb::from_hex(s) {
                    if i == 0 { app.fg = c; } else { app.bg = c; }
                }
            }
            _ => {}
        }
        i += 1;
    }

    Crust::init();
    let (mut cols, mut rows) = Crust::terminal_size();
    // Fall back to a reasonable default if running without a TTY (script,
    // ssh-without-pty, etc.). Otherwise zero-sized layout underflows.
    if cols < 80 { cols = 100; }
    if rows < 24 { rows = 28; }

    loop {
        render(&app, cols, rows);
        let key = match Input::getchr(None) { Some(k) => k, None => continue };
        match key.as_str() {
            "q" | "ESC" => break,
            "TAB" => {
                app.editing = if app.editing == Slot::Fg { Slot::Bg } else { Slot::Fg };
                app.status.clear();
            }
            "r" => app.channel = Channel::R,
            "g" => app.channel = Channel::G,
            "b" => app.channel = Channel::B,
            "h" => app.channel = Channel::H,
            "s" => app.channel = Channel::S,
            "v" => app.channel = Channel::V,
            "j" | "DOWN" | "LEFT" => app.step(-1),
            "k" | "UP" | "RIGHT"  => app.step(1),
            "J" | "S-DOWN" | "S-LEFT" => app.step(-10),
            "K" | "S-UP" | "S-RIGHT"  => app.step(10),
            "#" => {
                // Inline hex input. Render a minimal prompt at the bottom.
                let prompt_y = rows.saturating_sub(3);
                print!("{}\x1b[2K\x1b[1mhex:\x1b[0m \x1b[?25h", move_to(prompt_y, 3));
                let _ = std::io::stdout().flush();
                let mut buf = String::new();
                loop {
                    let k = match Input::getchr(None) { Some(k) => k, None => continue };
                    match k.as_str() {
                        "ENTER" => break,
                        "ESC" => { buf.clear(); break; }
                        "BACK" => { buf.pop(); }
                        s if s.len() == 1 => {
                            let ch = s.chars().next().unwrap();
                            if ch.is_ascii_hexdigit() && buf.len() < 6 { buf.push(ch); }
                        }
                        _ => {}
                    }
                    print!("{}\x1b[2K\x1b[1mhex:\x1b[0m {}", move_to(prompt_y, 3), buf);
                    let _ = std::io::stdout().flush();
                }
                if let Some(c) = Rgb::from_hex(&buf) {
                    app.set_current(c);
                    app.status = format!("set {} = {}", match app.editing { Slot::Fg => "FG", Slot::Bg => "BG" }, c.hex());
                } else if !buf.is_empty() {
                    app.status = format!("invalid hex: {}", buf);
                }
                print!("\x1b[?25l");
            }
            "c" => {
                let hex = app.current().hex();
                crust::clipboard_copy(&hex, "clipboard");
                app.status = format!("copied {} to clipboard", hex);
            }
            "o" => {
                app.out_fmt = match app.out_fmt {
                    OutputFmt::Hex => OutputFmt::Rgb,
                    OutputFmt::Rgb => OutputFmt::Hsv,
                    OutputFmt::Hsv => OutputFmt::All,
                    OutputFmt::All => OutputFmt::Hex,
                };
            }
            "RESIZE" => {
                let (_c, _r) = Crust::terminal_size();
                // render() reads the size each call.
            }
            _ => {}
        }
    }

    Crust::cleanup();
    print!("\x1b[?25h");

    // Emit chosen colors on stdout. Hex is always printed for both
    // slots (it's the most copy-pasted format). If --rgb / --hsv /
    // --all was requested, those lines follow.
    println!("fg={}", app.fg.hex());
    println!("bg={}", app.bg.hex());
    let extras = |slot: &str, c: &Rgb, fmt: OutputFmt| {
        let hsv = c.to_hsv();
        match fmt {
            OutputFmt::Hex => {}
            OutputFmt::Rgb => println!("{}=rgb({}, {}, {})", slot, c.r, c.g, c.b),
            OutputFmt::Hsv => println!("{}=hsv({:.0}, {:.0}%, {:.0}%)", slot, hsv.h, hsv.s * 100.0, hsv.v * 100.0),
            OutputFmt::All => {
                println!("{}=rgb({}, {}, {})", slot, c.r, c.g, c.b);
                println!("{}=hsv({:.0}, {:.0}%, {:.0}%)", slot, hsv.h, hsv.s * 100.0, hsv.v * 100.0);
            }
        }
    };
    extras("fg", &app.fg, app.out_fmt);
    extras("bg", &app.bg, app.out_fmt);
}
