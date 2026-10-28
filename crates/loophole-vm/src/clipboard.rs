use std::io::{self, ErrorKind, Read, Write};

const MAGIC: u32 = 0x6263_686c;
const TEXT: u32 = 1;
const HEADER: usize = 12;
pub const MAX: usize = 1 << 20;

pub fn write_text(w: &mut impl Write, text: &str) -> io::Result<()> {
    if text.len() > MAX {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("{} bytes is over the {} MiB limit", text.len(), MAX >> 20),
        ));
    }

    let mut frame: Vec<u8> = [MAGIC, TEXT, text.len() as u32]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    frame.extend_from_slice(text.as_bytes());
    w.write_all(&frame)
}

fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn read_text(r: &mut impl Read) -> io::Result<Option<String>> {
    let mut header = [0; HEADER];

    if !fill(r, &mut header)? {
        return Ok(None);
    }

    loop {
        let (words, _) = header.as_chunks::<4>();
        let [magic, kind, size] = std::array::from_fn(|i| u32::from_le_bytes(words[i]));
        let size = size as usize;

        if magic != MAGIC || size > MAX {
            header.copy_within(1.., 0);

            if !fill(r, &mut header[HEADER - 1..])? {
                return Ok(None);
            }

            continue;
        }

        let mut payload = vec![0; size];

        if !fill(r, &mut payload)? {
            return Ok(None);
        }

        if kind == TEXT
            && let Ok(text) = String::from_utf8(payload)
        {
            return Ok(Some(text));
        }

        if !fill(r, &mut header)? {
            return Ok(None);
        }
    }
}
