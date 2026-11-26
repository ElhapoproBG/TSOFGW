use super::*;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    hex(s).try_into().unwrap()
}

// --- Vectores del RFC 8439 ----------------------------------------------------------------------

#[test]
fn rfc8439_2_3_2_bloque_de_chacha20() {
    let key: [u8; 32] = core::array::from_fn(|i| i as u8);
    let nonce = arr::<12>("000000090000004a00000000");
    let block = chacha20_block(&key, 1, &nonce);
    let want = hex(
        "10f1e7e4d13b5915500fdd1fa32071c4 c7d1f4c733c068030422aa9ac3d46c4e
                    d2826446079faa0914c2d705d98b02a2 b5129cd1de164eb9cbd083e8a2503c4e",
    );
    assert_eq!(&block[..], &want[..]);
}

#[test]
fn rfc8439_2_4_2_cifrado_de_chacha20() {
    let key: [u8; 32] = core::array::from_fn(|i| i as u8);
    let nonce = arr::<12>("000000000000004a00000000");
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let mut buf = pt.to_vec();
    chacha20_xor(&key, 1, &nonce, &mut buf);
    // Primeros 32 bytes del texto cifrado del RFC.
    assert_eq!(
        &buf[..32],
        &hex("6e2e359a2568f98041ba0728dd0d6981 e97e7aec1d4360c20a27afccfd9fae0b")[..]
    );
    chacha20_xor(&key, 1, &nonce, &mut buf);
    assert_eq!(&buf[..], &pt[..]);
}

#[test]
fn rfc8439_2_5_2_poly1305() {
    let key = arr::<32>("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
    let tag = poly1305(&key, b"Cryptographic Forum Research Group");
    assert_eq!(&tag[..], &hex("a8061dc1305136c6c22b8baf0c0127a9")[..]);
}

#[test]
fn rfc8439_2_6_2_clave_de_poly1305() {
    let key = arr::<32>("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
    let nonce = arr::<12>("000000000001020304050607");
    assert_eq!(
        &poly_key(&key, &nonce)[..],
        &hex("8ad5a08b905f81cc815040274ab29471a833b637e3fd0da508dbb8e2fdd1a646")[..]
    );
}

#[test]
fn rfc8439_2_8_2_aead() {
    let key = arr::<32>("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
    let nonce = arr::<12>("070000004041424344454647");
    let aad = hex("50515253c0c1c2c3c4c5c6c7");
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let sealed = seal(&key, &nonce, &aad, pt);
    assert_eq!(sealed.len(), pt.len() + TAG_LEN);
    assert_eq!(&sealed[..16], &hex("d31a8d34648e60db7b86afbc53ef7ec2")[..]);
    assert_eq!(
        &sealed[pt.len()..],
        &hex("1ae10b594f09e26a7e902ecbd0600691")[..]
    );
    assert_eq!(open(&key, &nonce, &aad, &sealed).as_deref(), Some(&pt[..]));
}

// --- Propiedades --------------------------------------------------------------------------------

/// Generador determinista sencillo (no es criptográfico; solo para armar entradas de prueba).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn ida_y_vuelta_para_todas_las_longitudes_y_cualquier_alteracion_se_rechaza() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    for len in 0..=140usize {
        let key: [u8; 32] = r.bytes(32).try_into().unwrap();
        let nonce: [u8; 12] = r.bytes(12).try_into().unwrap();
        let aad = r.bytes(len % 40);
        let pt = r.bytes(len);
        let sealed = seal(&key, &nonce, &aad, &pt);
        assert_eq!(open(&key, &nonce, &aad, &sealed), Some(pt.clone()), "{len}");
        // Cada bit del cifrado y de la etiqueta.
        for i in 0..sealed.len() {
            for bit in 0..8 {
                let mut m = sealed.clone();
                m[i] ^= 1 << bit;
                assert!(
                    open(&key, &nonce, &aad, &m).is_none(),
                    "len {len} byte {i} bit {bit}"
                );
            }
        }
        // Datos asociados, nonce y clave distintos.
        let mut a2 = aad.clone();
        a2.push(0);
        assert!(open(&key, &nonce, &a2, &sealed).is_none());
        if !aad.is_empty() {
            let mut a3 = aad.clone();
            a3[0] ^= 1;
            assert!(open(&key, &nonce, &a3, &sealed).is_none());
            assert!(open(&key, &nonce, &aad[1..], &sealed).is_none());
        }
        let mut n2 = nonce;
        n2[11] ^= 1;
        assert!(open(&key, &n2, &aad, &sealed).is_none());
        let mut k2 = key;
        k2[0] ^= 0x80;
        assert!(open(&k2, &nonce, &aad, &sealed).is_none());
        // Truncado y alargado.
        assert!(open(&key, &nonce, &aad, &sealed[..sealed.len() - 1]).is_none());
        let mut longer = sealed.clone();
        longer.push(0);
        assert!(open(&key, &nonce, &aad, &longer).is_none());
    }
}

#[test]
fn un_fallo_de_autenticacion_no_toca_el_buffer() {
    let key = [7u8; 32];
    let nonce = [9u8; 12];
    let mut buf = b"mensaje secreto".to_vec();
    let tag = seal_in_place(&key, &nonce, b"aad", &mut buf);
    let cifrado = buf.clone();
    let mut mala = tag;
    mala[3] ^= 1;
    assert!(!open_in_place(&key, &nonce, b"aad", &mut buf, &mala));
    assert_eq!(buf, cifrado, "no debe descifrar nada si la etiqueta falla");
    assert!(!open_in_place(&key, &nonce, b"aad", &mut buf, &tag[..15]));
    assert!(open_in_place(&key, &nonce, b"aad", &mut buf, &tag));
    assert_eq!(buf, b"mensaje secreto");
}

#[test]
fn el_texto_cifrado_no_se_parece_al_claro_y_el_nonce_cambia_el_flujo() {
    let key = [1u8; 32];
    let pt = vec![0u8; 256];
    let a = seal(&key, &[0u8; 12], b"", &pt);
    let b = seal(
        &key,
        &{
            let mut n = [0u8; 12];
            n[0] = 1;
            n
        },
        b"",
        &pt,
    );
    assert_ne!(&a[..256], &pt[..]);
    assert_ne!(&a[..256], &b[..256]);
    // Con ceros en claro, el cifrado es el flujo de claves: no hay bloques repetidos.
    let blocks: Vec<&[u8]> = a[..256].chunks(64).collect();
    for i in 0..blocks.len() {
        for j in i + 1..blocks.len() {
            assert_ne!(blocks[i], blocks[j]);
        }
    }
}

#[test]
fn comparacion_de_etiquetas() {
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"ab"));
    assert!(ct_eq(b"", b""));
}

#[test]
#[should_panic(expected = "contador de bloque agotado")]
fn el_contador_de_bloque_no_da_la_vuelta() {
    let mut buf = vec![0u8; 128]; // dos bloques con el contador en u32::MAX: el segundo se repetiría
    chacha20_xor(&[0u8; 32], u32::MAX, &[0u8; 12], &mut buf);
}
