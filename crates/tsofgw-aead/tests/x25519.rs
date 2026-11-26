//! Vectores del RFC 7748 (§5.2, §6.1 y la prueba iterada) para X25519.

use tsofgw_aead::x25519::*;

fn h<const N: usize>(s: &str) -> [u8; N] {
    let v: Vec<u8> = (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    v.try_into().unwrap()
}

#[test]
fn rfc7748_seccion_5_2() {
    assert_eq!(
        x25519(
            &h("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"),
            &h("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c")
        ),
        h("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")
    );
    assert_eq!(
        x25519(
            &h("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d"),
            &h("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493")
        ),
        h("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957")
    );
}

#[test]
fn rfc7748_prueba_iterada() {
    let mut k = BASEPOINT;
    let mut u = BASEPOINT;
    for i in 1..=1000 {
        let r = x25519(&k, &u);
        u = k;
        k = r;
        if i == 1 {
            assert_eq!(
                k,
                h("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079")
            );
        }
    }
    assert_eq!(
        k,
        h("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
    );
}

#[test]
fn rfc7748_seccion_6_1_alice_y_bob() {
    let a = h::<32>("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let b = h::<32>("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let (pa, pb) = (public_key(&a), public_key(&b));
    assert_eq!(
        pa,
        h("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
    );
    assert_eq!(
        pb,
        h("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
    );
    let k = h::<32>("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    assert_eq!(shared_secret(&a, &pb), Some(k));
    assert_eq!(shared_secret(&b, &pa), Some(k));
}

#[test]
fn puntos_de_orden_pequeno_se_rechazan() {
    let sk = h::<32>("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    assert_eq!(shared_secret(&sk, &[0u8; 32]), None); // u = 0
    let mut one = [0u8; 32];
    one[0] = 1;
    assert_eq!(shared_secret(&sk, &one), None); // u = 1
                                                // u = p−1 (orden 2) y u = p (≡ 0)
    let mut pm1 = [0xffu8; 32];
    pm1[0] = 0xec;
    pm1[31] = 0x7f;
    assert_eq!(shared_secret(&sk, &pm1), None);
    let mut p = pm1;
    p[0] = 0xed;
    assert_eq!(shared_secret(&sk, &p), None);
}

#[test]
fn el_bit_alto_de_u_se_ignora() {
    let sk = h::<32>("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
    let u = h::<32>("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
    let mut u2 = u;
    u2[31] ^= 0x80;
    assert_eq!(x25519(&sk, &u), x25519(&sk, &u2));
}
