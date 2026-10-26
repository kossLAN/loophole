use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn number(s: &str) -> Option<u32> {
    let s = s.trim();
    s.strip_prefix("0x")
        .map_or_else(|| s.parse().ok(), |hex| u32::from_str_radix(hex, 16).ok())
}

fn main() {
    println!("cargo:rerun-if-env-changed=LOOPHOLE_KEYMAPS_CSV");
    let csv = env::var("LOOPHOLE_KEYMAPS_CSV")
        .expect("LOOPHOLE_KEYMAPS_CSV must point at keycodemapdb's keymaps.csv");
    println!("cargo:rerun-if-changed={csv}");
    let text = fs::read_to_string(&csv).expect("reading keymaps.csv");
    let mut lines = text.lines();
    let header = split(lines.next().expect("empty keymaps.csv"));
    let col = |name: &str| header.iter().position(|h| h == name).expect(name);
    let (linux, set1) = (col("Linux Keycode"), col("AT set1 keycode"));

    let mut table = vec![0_u16; 768];
    for line in lines {
        let fields = split(line);
        let (Some(code), Some(scancode)) = (
            fields.get(linux).and_then(|s| number(s)),
            fields.get(set1).and_then(|s| number(s)),
        ) else {
            continue;
        };
        // A qnum is the AT set 1 scancode, with 0x80 set for the 0xe0-prefixed keys.
        let qnum = match scancode {
            0..=0x7f => scancode,
            _ if scancode >> 8 == 0xe0 && scancode & 0xff < 0x80 => 0x80 | (scancode & 0x7f),
            // Pause and other multi-byte sequences
            _ => continue,
        };
        if let (Some(slot), Ok(qnum)) = (table.get_mut(code as usize), u16::try_from(qnum)) {
            *slot = qnum;
        }
    }

    let mut out = String::from("/// Linux evdev keycode -> QEMU qnum (0: unmapped).\n");
    writeln!(out, "pub static LINUX_TO_QNUM: [u16; {}] = [", table.len()).unwrap();
    for chunk in table.chunks(16) {
        let row: Vec<String> = chunk.iter().map(|v| format!("0x{v:02x},")).collect();
        writeln!(out, "    {}", row.join(" ")).unwrap();
    }
    out += "];\n";
    let dest = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("keymap.rs");
    fs::write(dest, out).expect("writing keymap.rs");
}
