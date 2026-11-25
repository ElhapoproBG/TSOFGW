//! Codificador PNG mínimo (RGBA de 8 bits) sin dependencias.
//!
//! Usa bloques *stored* de zlib (sin compresión): los archivos pesan más, pero el formato es
//! válido y cualquier lector de PNG los abre. [`decode_rgba`] solo lee archivos hechos por este
//! codificador; sirve para pruebas y comparación de imágenes, no es un lector general de PNG.

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (n, slot) in t.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    !data.iter().fold(!0u32, |c, &b| {
        t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8)
    })
}

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

/// Codifica píxeles RGBA8 (filas de arriba abajo) como PNG.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert!(width > 0 && height > 0, "imagen vacía");
    assert_eq!(
        rgba.len(),
        width as usize * height as usize * 4,
        "tamaño de píxeles incorrecto"
    );
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filtro "ninguno"
        raw.extend_from_slice(&rgba[y * row..(y + 1) * row]);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none())); // bit BFINAL, tipo 00 = sin compresión
        z.extend((c.len() as u16).to_le_bytes());
        z.extend((!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend(adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, filtro estándar, sin entrelazado
    let mut out = SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Lee un PNG producido por [`encode_rgba`]. Devuelve `(ancho, alto, píxeles RGBA8)`.
pub fn decode_rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if png.len() < 8 || png[..8] != SIGNATURE {
        return Err("no es un PNG".into());
    }
    let (mut pos, mut size, mut idat) = (8, None, Vec::new());
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let end = pos + 8 + len;
        if end + 4 > png.len() {
            return Err("fragmento truncado".into());
        }
        let kind = &png[pos + 4..pos + 8];
        let crc = u32::from_be_bytes(png[end..end + 4].try_into().unwrap());
        if crc != crc32(&png[pos + 4..end]) {
            return Err("CRC incorrecto".into());
        }
        let data = &png[pos + 8..end];
        match kind {
            b"IHDR" => {
                if data.len() != 13 || data[8..] != [8, 6, 0, 0, 0] {
                    return Err("formato no soportado".into());
                }
                size = Some((
                    u32::from_be_bytes(data[0..4].try_into().unwrap()),
                    u32::from_be_bytes(data[4..8].try_into().unwrap()),
                ));
            }
            b"IDAT" => idat.extend_from_slice(data),
            _ => {}
        }
        pos = end + 4;
    }
    let (w, h) = size.ok_or("falta IHDR")?;
    if idat.len() < 6 || idat[0] != 0x78 {
        return Err("zlib inválido".into());
    }
    let (mut raw, mut p) = (Vec::new(), 2);
    loop {
        let hdr = *idat.get(p).ok_or("zlib truncado")?;
        if hdr & 0b110 != 0 {
            return Err("solo se leen bloques sin compresión".into());
        }
        let len = u16::from_le_bytes(
            idat.get(p + 1..p + 3)
                .ok_or("zlib truncado")?
                .try_into()
                .unwrap(),
        ) as usize;
        let nlen = u16::from_le_bytes(
            idat.get(p + 3..p + 5)
                .ok_or("zlib truncado")?
                .try_into()
                .unwrap(),
        );
        if nlen != !(len as u16) {
            return Err("longitud de bloque incorrecta".into());
        }
        raw.extend_from_slice(idat.get(p + 5..p + 5 + len).ok_or("zlib truncado")?);
        p += 5 + len;
        if hdr & 1 == 1 {
            break;
        }
    }
    let adler = u32::from_be_bytes(
        idat.get(p..p + 4)
            .ok_or("zlib truncado")?
            .try_into()
            .unwrap(),
    );
    if adler != adler32(&raw) {
        return Err("Adler-32 incorrecto".into());
    }
    let row = w as usize * 4;
    if raw.len() != (row + 1) * h as usize {
        return Err("tamaño de datos incorrecto".into());
    }
    let mut px = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        let line = &raw[y * (row + 1)..(y + 1) * (row + 1)];
        if line[0] != 0 {
            return Err("filtro no soportado".into());
        }
        px.extend_from_slice(&line[1..]);
    }
    Ok((w, h, px))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_y_adler_con_valores_conocidos() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn ida_y_vuelta() {
        let (w, h) = (7u32, 5u32);
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i * 13 + 5) as u8).collect();
        let png = encode_rgba(w, h, &px);
        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(decode_rgba(&png).unwrap(), (w, h, px));
    }

    #[test]
    fn imagen_grande_usa_varios_bloques() {
        let (w, h) = (300u32, 200u32); // 240 200 bytes > 65 535
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        let png = encode_rgba(w, h, &px);
        assert_eq!(decode_rgba(&png).unwrap().2, px);
    }

    #[test]
    fn detecta_corrupcion() {
        let png = encode_rgba(4, 4, &[200u8; 64]);
        let mut mal = png.clone();
        let n = mal.len();
        mal[n / 2] ^= 0xFF;
        assert!(decode_rgba(&mal).is_err());
        assert!(decode_rgba(&png[..20]).is_err());
        assert!(decode_rgba(b"hola").is_err());
    }
}
