//! El almacén sobre archivos de verdad: el mismo comportamiento que sobre el medio en memoria, más lo
//! que solo existe en un sistema de archivos (bloqueo exclusivo, limpieza de restos, nombres).

use std::fs;
use std::path::PathBuf;

use tsofgw_store::{Batch, FsMedium, Store, StoreConfig, StoreError};

const KEY: [u8; 32] = [3; 32];

/// Carpeta temporal única que se borra al soltarla.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tsofgw-store-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        Temp(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open(t: &Temp) -> (Store<FsMedium>, tsofgw_store::Recovery) {
    let m = FsMedium::open(t.path()).expect("abrir carpeta");
    Store::open(m, &KEY, StoreConfig::default()).expect("abrir almacén")
}

fn files(t: &Temp) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(t.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn lo_sincronizado_sobrevive_a_cerrar_y_reabrir() {
    let t = Temp::new("roundtrip");
    {
        let (mut s, rec) = open(&t);
        assert!(rec.created);
        s.put(b"a", b"1").unwrap();
        let mut b = Batch::new();
        b.put(&b"b"[..], &b"2"[..])
            .put(&b"c"[..], &b"3"[..])
            .delete(&b"a"[..]);
        s.apply(&b).unwrap();
        s.commit().unwrap();
    }
    let (s, rec) = open(&t);
    assert!(!rec.created);
    assert_eq!(rec.records, 2);
    assert_eq!(rec.torn_bytes, 0);
    assert!(!s.contains(b"a"));
    assert_eq!(s.get(b"b"), Some(&b"2"[..]));
    assert_eq!(s.get(b"c"), Some(&b"3"[..]));
}

#[test]
fn una_carpeta_no_se_abre_dos_veces_a_la_vez() {
    let t = Temp::new("lock");
    let first = FsMedium::open(t.path()).unwrap();
    match FsMedium::open(t.path()) {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{e}"),
        Ok(_) => panic!("dos procesos (o dos almacenes) escribiendo en la misma carpeta"),
    }
    drop(first);
    assert!(
        FsMedium::open(t.path()).is_ok(),
        "al soltarla se puede volver a abrir"
    );
}

#[test]
fn el_checkpoint_deja_una_sola_generacion_y_sin_temporales() {
    let t = Temp::new("generaciones");
    let (mut s, _) = open(&t);
    s.put(b"a", b"1").unwrap();
    s.checkpoint().unwrap();
    s.put(b"b", b"2").unwrap();
    s.checkpoint().unwrap();
    s.put(b"c", b"3").unwrap();
    s.commit().unwrap();
    let g = s.generation();
    drop(s);
    let names = files(&t);
    assert_eq!(
        names,
        vec![
            "LOCK".to_string(),
            format!("snap-{g:020}.bin"),
            format!("wal-{g:020}.log"),
        ],
        "una sola generación y nada más"
    );
    let (s, rec) = open(&t);
    assert_eq!(rec.generation, g);
    assert_eq!(rec.records, 1);
    for (k, v) in [(&b"a"[..], &b"1"[..]), (b"b", b"2"), (b"c", b"3")] {
        assert_eq!(s.get(k), Some(v));
    }
}

#[test]
fn restos_de_un_checkpoint_interrumpido_se_limpian_y_no_se_usan() {
    let t = Temp::new("restos");
    {
        let (mut s, _) = open(&t);
        s.put(b"a", b"1").unwrap();
        s.commit().unwrap();
    }
    let g = {
        let (s, _) = open(&t);
        s.generation()
    };
    // Una caída antes del renombrado deja un .tmp y quizá el diario de la generación siguiente.
    fs::write(
        t.path().join(format!("snap-{:020}.tmp", g + 1)),
        b"a medias",
    )
    .unwrap();
    fs::write(t.path().join(format!("wal-{:020}.log", g + 1)), b"").unwrap();
    let (s, rec) = open(&t);
    assert_eq!(
        rec.generation, g,
        "la generación vigente es la última con checkpoint completo"
    );
    assert_eq!(s.get(b"a"), Some(&b"1"[..]));
    drop(s);
    let names = files(&t);
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    assert!(
        !names.contains(&format!("wal-{:020}.log", g + 1)),
        "{names:?}"
    );
}

#[test]
fn una_cola_rota_en_el_archivo_se_descarta_y_se_recorta() {
    let t = Temp::new("cola");
    let (g, len_ok) = {
        let (mut s, _) = open(&t);
        s.put(b"a", b"1").unwrap();
        s.put(b"b", b"2").unwrap();
        s.commit().unwrap();
        (s.generation(), s.log_len())
    };
    let wal = t.path().join(format!("wal-{g:020}.log"));
    // Se simula una escritura a medias: bytes sueltos al final del diario.
    let mut bytes = fs::read(&wal).unwrap();
    assert_eq!(bytes.len() as u64, len_ok);
    bytes.extend_from_slice(&[0xAB; 37]);
    fs::write(&wal, &bytes).unwrap();
    let (mut s, rec) = open(&t);
    assert_eq!(rec.torn_bytes, 37);
    assert_eq!(rec.records, 2);
    assert_eq!(
        fs::metadata(&wal).unwrap().len(),
        len_ok,
        "el archivo se recortó de verdad"
    );
    // Y lo siguiente se escribe justo después del último registro bueno.
    s.put(b"c", b"3").unwrap();
    s.commit().unwrap();
    drop(s);
    let (s, rec) = open(&t);
    assert_eq!(rec.torn_bytes, 0);
    assert_eq!(s.get(b"c"), Some(&b"3"[..]));
}

#[test]
fn clave_equivocada_sobre_archivos() {
    let t = Temp::new("clave");
    {
        let (mut s, _) = open(&t);
        s.put(b"a", b"1").unwrap();
        s.commit().unwrap();
    }
    let m = FsMedium::open(t.path()).unwrap();
    assert!(matches!(
        Store::open(m, &[4u8; 32], StoreConfig::default()),
        Err(StoreError::WrongKey)
    ));
    // No se dañó nada: con la clave buena sigue abriendo.
    let (s, _) = open(&t);
    assert_eq!(s.get(b"a"), Some(&b"1"[..]));
}

#[test]
fn muchas_escrituras_y_compactaciones_sobre_archivos() {
    let t = Temp::new("muchas");
    let (mut s, _) = open(&t);
    let mut expected = std::collections::BTreeMap::new();
    for i in 0u32..3000 {
        let k = (i % 97).to_le_bytes().to_vec();
        let v = i.to_le_bytes().repeat(8);
        s.put(&k, &v).unwrap();
        expected.insert(k, v);
        if i % 500 == 499 {
            s.commit().unwrap();
            s.checkpoint_if_larger(20_000).unwrap();
        }
    }
    s.commit().unwrap();
    assert!(s.stats().checkpoints >= 2, "la compactación actuó");
    drop(s);
    let (s, _) = open(&t);
    assert_eq!(s.len(), expected.len());
    for (k, v) in &expected {
        assert_eq!(s.get(k), Some(v.as_slice()));
    }
}
