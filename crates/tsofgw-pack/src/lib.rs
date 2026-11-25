//! Módulos verificables para TSOFGW.
//!
//! Un módulo es un *payload* (binario WASM, nodo nativo, datos) más un **manifiesto** que declara
//! nombre, versión, permisos y el hash SHA-256 del payload, firmado por quien lo publica.
//! Antes de cargar, el motor comprueba el hash, la firma, el nivel de confianza y los permisos.
//!
//! **Estado de las firmas:** aquí se incluye `HmacScheme` (HMAC-SHA256, clave compartida), útil para
//! pruebas y redes privadas. La distribución pública necesita firmas asimétricas (Ed25519); el
//! `trait SignatureScheme` está preparado para enchufarlas cuando se añada esa dependencia.
//! No se implementa criptografía asimétrica propia a propósito.

use std::collections::{HashMap, HashSet};
use tsofgw_core::Contract;

// ---------------------------------------------------------------- SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.state, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            Self::compress(&mut self.state, &block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bits = self.total * 8;
        let mut pad = vec![0x80u8];
        let rem = (self.buf_len + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat_n(0, zeros));
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        debug_assert_eq!(self.buf_len, 0);
        let mut out = [0u8; 32];
        for (i, v) in self.state.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&k.map(|b| b ^ 0x5c));
    outer.update(&inner);
    outer.finalize()
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ------------------------------------------------------------------ firmas

pub trait SignatureScheme: Send {
    fn sign(&self, msg: &[u8]) -> Vec<u8>;
    fn verify(&self, msg: &[u8], sig: &[u8]) -> bool;
}

/// HMAC-SHA256 con clave compartida. Solo para pruebas y redes privadas (ver documentación del módulo).
pub struct HmacScheme {
    key: Vec<u8>,
}

impl HmacScheme {
    pub fn new(key: &[u8]) -> Self {
        HmacScheme { key: key.to_vec() }
    }
}

impl SignatureScheme for HmacScheme {
    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        hmac_sha256(&self.key, msg).to_vec()
    }
    fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        ct_eq(&hmac_sha256(&self.key, msg), sig)
    }
}

// -------------------------------------------------------------- manifiesto

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    BadHeader,
    BadLine(usize),
    UnknownKey(String),
    DuplicateKey(String),
    Missing(&'static str),
    BadName,
    BadVersion,
    BadHash,
    BadCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: (u16, u16),
    pub capabilities: Vec<String>,
    pub payload_sha256: [u8; 32],
    /// Quién firma (vacío = sin firma).
    pub signer: String,
    pub signature: Vec<u8>,
}

const HEADER: &str = "tsofgw-module 1";

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Manifest {
    pub fn new(name: &str, version: (u16, u16), capabilities: &[&str], payload: &[u8]) -> Self {
        let mut caps: Vec<String> = capabilities.iter().map(|s| s.to_string()).collect();
        caps.sort();
        caps.dedup();
        Manifest {
            name: name.into(),
            version,
            capabilities: caps,
            payload_sha256: sha256(payload),
            signer: String::new(),
            signature: vec![],
        }
    }

    /// Texto exacto que se firma (todo menos la firma). Orden y formato fijos.
    pub fn signed_body(&self) -> String {
        format!(
            "{HEADER}\nname = {}\nversion = {}.{}\ncapabilities = {}\npayload_sha256 = {}\nsigner = {}\n",
            self.name,
            self.version.0,
            self.version.1,
            self.capabilities.join(","),
            to_hex(&self.payload_sha256),
            self.signer
        )
    }

    pub fn sign(&mut self, signer: &str, scheme: &dyn SignatureScheme) {
        self.signer = signer.into();
        self.signature = scheme.sign(self.signed_body().as_bytes());
    }

    pub fn to_text(&self) -> String {
        format!(
            "{}signature = {}\n",
            self.signed_body(),
            to_hex(&self.signature)
        )
    }

    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(ManifestError::BadHeader);
        }
        let mut kv: HashMap<&str, &str> = HashMap::new();
        for (i, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once(" = ")
                .ok_or(ManifestError::BadLine(i + 2))?;
            if ![
                "name",
                "version",
                "capabilities",
                "payload_sha256",
                "signer",
                "signature",
            ]
            .contains(&k)
            {
                return Err(ManifestError::UnknownKey(k.into()));
            }
            if kv.insert(k, v).is_some() {
                return Err(ManifestError::DuplicateKey(k.into()));
            }
        }
        let get = |k: &'static str| kv.get(k).copied().ok_or(ManifestError::Missing(k));
        let name = get("name")?;
        if !valid_ident(name) {
            return Err(ManifestError::BadName);
        }
        let (maj, min) = get("version")?
            .split_once('.')
            .ok_or(ManifestError::BadVersion)?;
        let version = (
            maj.parse().map_err(|_| ManifestError::BadVersion)?,
            min.parse().map_err(|_| ManifestError::BadVersion)?,
        );
        let mut capabilities: Vec<String> = vec![];
        for c in get("capabilities")?.split(',').filter(|c| !c.is_empty()) {
            if !valid_ident(c) {
                return Err(ManifestError::BadCapability);
            }
            capabilities.push(c.into());
        }
        let hash = from_hex(get("payload_sha256")?).ok_or(ManifestError::BadHash)?;
        let payload_sha256: [u8; 32] = hash.try_into().map_err(|_| ManifestError::BadHash)?;
        let signer = get("signer")?;
        if !signer.is_empty() && !valid_ident(signer) {
            return Err(ManifestError::BadName);
        }
        let signature = from_hex(get("signature")?).ok_or(ManifestError::BadHash)?;
        Ok(Manifest {
            name: name.into(),
            version,
            capabilities,
            payload_sha256,
            signer: signer.into(),
            signature,
        })
    }
}

// ---------------------------------------------------------------- confianza

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Official,
    Certified,
    Unsigned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// El payload no coincide con el hash del manifiesto (alterado o corrupto).
    HashMismatch,
    UnknownSigner(String),
    BadSignature,
    /// Hash de payload revocado (versión retirada por comprometida).
    Revoked,
    /// Sin firma y la política no admite módulos sin firma.
    UnsignedNotAllowed,
    /// Permisos que este nivel de confianza no puede recibir.
    ForbiddenCapabilities(Vec<String>),
    /// El nodo usa permisos que su manifiesto no declaró.
    UndeclaredCapabilities(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub trust: Trust,
    pub warnings: Vec<String>,
}

/// Qué permisos se pueden conceder a cada nivel y si se admiten módulos sin firma.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Los sin firma solo cargan con esta bandera (decisión explícita y visible del operador).
    pub allow_unsigned: bool,
    pub forbidden_for_unsigned: Vec<String>,
    pub forbidden_for_certified: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            allow_unsigned: false,
            forbidden_for_unsigned: vec!["accounts".into(), "system".into(), "disk_write".into()],
            forbidden_for_certified: vec!["system".into()],
        }
    }
}

#[derive(Default)]
pub struct TrustStore {
    signers: HashMap<String, (Trust, Box<dyn SignatureScheme>)>,
    revoked: HashSet<[u8; 32]>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }
    /// Registra un firmante conocido con su nivel (`Official` o `Certified`).
    pub fn add_signer(&mut self, name: &str, trust: Trust, scheme: Box<dyn SignatureScheme>) {
        assert!(
            trust != Trust::Unsigned,
            "un firmante no puede ser 'sin firma'"
        );
        self.signers.insert(name.into(), (trust, scheme));
    }
    pub fn revoke(&mut self, payload_sha256: [u8; 32]) {
        self.revoked.insert(payload_sha256);
    }

    /// Verifica hash, revocación y firma. Un módulo firmado con firma mala o firmante desconocido se
    /// rechaza: nunca se degrada en silencio a "sin firma".
    pub fn evaluate(&self, m: &Manifest, payload: &[u8]) -> Result<Trust, LoadError> {
        if !ct_eq(&sha256(payload), &m.payload_sha256) {
            return Err(LoadError::HashMismatch);
        }
        if self.revoked.contains(&m.payload_sha256) {
            return Err(LoadError::Revoked);
        }
        if m.signer.is_empty() {
            return if m.signature.is_empty() {
                Ok(Trust::Unsigned)
            } else {
                Err(LoadError::BadSignature)
            };
        }
        let (trust, scheme) = self
            .signers
            .get(&m.signer)
            .ok_or_else(|| LoadError::UnknownSigner(m.signer.clone()))?;
        if scheme.verify(m.signed_body().as_bytes(), &m.signature) {
            Ok(*trust)
        } else {
            Err(LoadError::BadSignature)
        }
    }
}

/// Verifica hash, firma, revocación y política de permisos según el propio manifiesto.
pub fn check_manifest(
    store: &TrustStore,
    policy: &Policy,
    m: &Manifest,
    payload: &[u8],
) -> Result<Loaded, LoadError> {
    let trust = store.evaluate(m, payload)?;
    let mut warnings = vec![];

    let forbidden: &[String] = match trust {
        Trust::Official => &[],
        Trust::Certified => &policy.forbidden_for_certified,
        Trust::Unsigned => {
            if !policy.allow_unsigned {
                return Err(LoadError::UnsignedNotAllowed);
            }
            warnings.push(format!(
                "módulo '{}' sin firma: cargado por decisión explícita del operador",
                m.name
            ));
            &policy.forbidden_for_unsigned
        }
    };
    let bad: Vec<String> = m
        .capabilities
        .iter()
        .filter(|c| forbidden.contains(c))
        .cloned()
        .collect();
    if !bad.is_empty() {
        return Err(LoadError::ForbiddenCapabilities(bad));
    }
    Ok(Loaded { trust, warnings })
}

/// Decisión completa de carga: [`check_manifest`] + coherencia con el contrato del nodo
/// (el nodo no puede usar permisos que su manifiesto no declaró).
pub fn check_load(
    store: &TrustStore,
    policy: &Policy,
    m: &Manifest,
    payload: &[u8],
    contract: &Contract,
) -> Result<Loaded, LoadError> {
    let loaded = check_manifest(store, policy, m, payload)?;
    let undeclared: Vec<String> = contract
        .capabilities
        .iter()
        .filter(|c| !m.capabilities.iter().any(|d| d == *c))
        .map(|c| c.to_string())
        .collect();
    if !undeclared.is_empty() {
        return Err(LoadError::UndeclaredCapabilities(undeclared));
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &[u8]) -> String {
        to_hex(&sha256(d))
    }

    #[test]
    fn sha256_vectores_oficiales() {
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let millon = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&millon),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_por_partes_igual_que_de_una_vez() {
        let datos: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for corte in [0, 1, 55, 56, 63, 64, 65, 127, 500, 999, 1000] {
            let mut h = Sha256::new();
            h.update(&datos[..corte]);
            h.update(&datos[corte..]);
            assert_eq!(h.finalize(), sha256(&datos), "corte {corte}");
        }
        // longitudes alrededor del relleno
        for n in 50..70 {
            let d = vec![b'x'; n];
            let mut h = Sha256::new();
            for b in &d {
                h.update(std::slice::from_ref(b));
            }
            assert_eq!(h.finalize(), sha256(&d));
        }
    }

    #[test]
    fn hmac_vectores_rfc4231() {
        assert_eq!(
            to_hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // clave más larga que un bloque
        let k = [0xaa; 131];
        assert_eq!(
            to_hex(&hmac_sha256(
                &k,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hex_ida_y_vuelta() {
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(to_hex(&[1, 171]), "01ab");
    }

    fn firmado(payload: &[u8], caps: &[&str], signer: &str, key: &[u8]) -> Manifest {
        let mut m = Manifest::new("fisica", (1, 2), caps, payload);
        m.sign(signer, &HmacScheme::new(key));
        m
    }

    #[test]
    fn manifiesto_ida_y_vuelta() {
        let m = firmado(b"payload", &["net", "disk"], "oficial", b"k");
        let texto = m.to_text();
        assert!(texto.starts_with("tsofgw-module 1\n"));
        assert_eq!(Manifest::parse(&texto).unwrap(), m);
        // capacidades ordenadas y sin duplicados
        let m2 = Manifest::new("a", (0, 1), &["net", "disk", "net"], b"");
        assert_eq!(m2.capabilities, vec!["disk", "net"]);
    }

    #[test]
    fn manifiesto_rechaza_entradas_invalidas() {
        let ok = firmado(b"p", &["net"], "oficial", b"k").to_text();
        assert_eq!(Manifest::parse("hola"), Err(ManifestError::BadHeader));
        assert_eq!(
            Manifest::parse(&ok.replace("name = fisica", "name = FISICA")),
            Err(ManifestError::BadName)
        );
        assert_eq!(
            Manifest::parse(&ok.replace("version = 1.2", "version = uno")),
            Err(ManifestError::BadVersion)
        );
        assert_eq!(
            Manifest::parse(&ok.replace("capabilities = net", "capabilities = n et")),
            Err(ManifestError::BadCapability)
        );
        assert_eq!(
            Manifest::parse(&format!("{ok}extra = 1\n")),
            Err(ManifestError::UnknownKey("extra".into()))
        );
        assert_eq!(
            Manifest::parse(&format!("{ok}name = otro\n")),
            Err(ManifestError::DuplicateKey("name".into()))
        );
        assert_eq!(
            Manifest::parse(&ok.replace("signer = oficial\n", "")),
            Err(ManifestError::Missing("signer"))
        );
        assert_eq!(
            Manifest::parse(&ok.replace("payload_sha256 = ", "payload_sha256 = 00")),
            Err(ManifestError::BadHash)
        );
        assert_eq!(
            Manifest::parse(&format!("{HEADER}\nbasura\n")),
            Err(ManifestError::BadLine(2))
        );
    }

    fn almacen() -> TrustStore {
        let mut s = TrustStore::new();
        s.add_signer(
            "oficial",
            Trust::Official,
            Box::new(HmacScheme::new(b"clave-oficial")),
        );
        s.add_signer(
            "estudio-x",
            Trust::Certified,
            Box::new(HmacScheme::new(b"clave-x")),
        );
        s
    }
    fn contrato(caps: &[&'static str]) -> Contract {
        let mut c = Contract::new("fisica", (1, 2));
        for cap in caps {
            c = c.cap(cap);
        }
        c
    }

    #[test]
    fn oficial_certificado_y_sin_firma() {
        let s = almacen();
        let p = Policy::default();
        let payload = b"codigo del modulo";

        let m = firmado(payload, &["net", "accounts"], "oficial", b"clave-oficial");
        let r = check_load(&s, &p, &m, payload, &contrato(&["net"])).unwrap();
        assert_eq!(
            r,
            Loaded {
                trust: Trust::Official,
                warnings: vec![]
            }
        );

        let m = firmado(payload, &["net"], "estudio-x", b"clave-x");
        assert_eq!(
            check_load(&s, &p, &m, payload, &contrato(&["net"]))
                .unwrap()
                .trust,
            Trust::Certified
        );
        let m = firmado(payload, &["system"], "estudio-x", b"clave-x");
        assert_eq!(
            check_load(&s, &p, &m, payload, &contrato(&[])),
            Err(LoadError::ForbiddenCapabilities(vec!["system".into()]))
        );

        let sin = Manifest::new("fisica", (1, 2), &["net"], payload);
        assert_eq!(
            check_load(&s, &p, &sin, payload, &contrato(&["net"])),
            Err(LoadError::UnsignedNotAllowed)
        );
        let permisiva = Policy {
            allow_unsigned: true,
            ..Policy::default()
        };
        let r = check_load(&s, &permisiva, &sin, payload, &contrato(&["net"])).unwrap();
        assert_eq!(r.trust, Trust::Unsigned);
        assert_eq!(r.warnings.len(), 1);
        // incluso permitidos, los sin firma no reciben permisos sensibles
        let sin = Manifest::new("fisica", (1, 2), &["accounts"], payload);
        assert_eq!(
            check_load(&s, &permisiva, &sin, payload, &contrato(&[])),
            Err(LoadError::ForbiddenCapabilities(vec!["accounts".into()]))
        );
    }

    #[test]
    fn detecta_alteraciones() {
        let s = almacen();
        let p = Policy::default();
        let payload = b"original";
        let m = firmado(payload, &["net"], "oficial", b"clave-oficial");

        // payload alterado
        assert_eq!(s.evaluate(&m, b"alterado"), Err(LoadError::HashMismatch));
        // manifiesto alterado (permisos añadidos) con la firma original
        let mut t = m.clone();
        t.capabilities.push("accounts".into());
        assert_eq!(s.evaluate(&t, payload), Err(LoadError::BadSignature));
        // firma de otro firmante
        let falso = firmado(payload, &["net"], "oficial", b"clave-robada");
        assert_eq!(s.evaluate(&falso, payload), Err(LoadError::BadSignature));
        // firmante desconocido: se rechaza, no se degrada a "sin firma"
        let raro = firmado(payload, &["net"], "intruso", b"x");
        assert_eq!(
            check_load(
                &s,
                &Policy {
                    allow_unsigned: true,
                    ..p.clone()
                },
                &raro,
                payload,
                &contrato(&[])
            ),
            Err(LoadError::UnknownSigner("intruso".into()))
        );
        // firma sin firmante
        let mut huérfano = Manifest::new("fisica", (1, 2), &[], payload);
        huérfano.signature = vec![1, 2, 3];
        assert_eq!(s.evaluate(&huérfano, payload), Err(LoadError::BadSignature));
    }

    #[test]
    fn revocacion_y_permisos_no_declarados() {
        let mut s = almacen();
        let p = Policy::default();
        let payload = b"v1";
        let m = firmado(payload, &["net"], "oficial", b"clave-oficial");
        // el nodo usa un permiso que el manifiesto no declaró
        assert_eq!(
            check_load(&s, &p, &m, payload, &contrato(&["net", "disk"])),
            Err(LoadError::UndeclaredCapabilities(vec!["disk".into()]))
        );
        s.revoke(m.payload_sha256);
        assert_eq!(
            check_load(&s, &p, &m, payload, &contrato(&["net"])),
            Err(LoadError::Revoked)
        );
    }
}
