use super::*;
use std::collections::BTreeMap;

const KEY: [u8; 32] = [7; 32];

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn open(m: &MemMedium) -> (Store<MemMedium>, Recovery) {
    Store::open(m.clone(), &KEY, StoreConfig::default()).expect("abrir")
}

fn model_of(s: &Store<MemMedium>) -> Model {
    s.scan(b"").map(|(k, v)| (k.to_vec(), v.to_vec())).collect()
}

/// Un lote aleatorio (1 a 4 operaciones sobre pocas claves, para que se pisen y se borren).
fn random_batch(rng: &mut Rng) -> Batch {
    let mut b = Batch::new();
    for _ in 0..1 + rng.below(4) {
        let key = format!("k{}", rng.below(12)).into_bytes();
        if rng.below(4) == 0 {
            b.delete(key);
        } else {
            let len = rng.below(30) as usize;
            let value: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            b.put(key, value);
        }
    }
    b
}

fn apply_model(model: &mut Model, b: &Batch) {
    for op in &b.ops {
        match op {
            Op::Put(k, v) => {
                model.insert(k.clone(), v.clone());
            }
            Op::Del(k) => {
                model.remove(k);
            }
        }
    }
}

/// Escribe `n` lotes aleatorios y devuelve el medio, los finales de cada registro y el estado tras cada
/// uno (el índice 0 es el estado vacío).
fn build(n: usize, seed: u64) -> (MemMedium, Vec<u64>, Vec<Model>) {
    let m = MemMedium::new();
    let (mut s, rec) = open(&m);
    assert!(rec.created);
    let mut rng = Rng(seed);
    let mut model = Model::new();
    let mut ends = Vec::new();
    let mut states = vec![model.clone()];
    for _ in 0..n {
        let b = random_batch(&mut rng);
        s.apply(&b).unwrap();
        apply_model(&mut model, &b);
        ends.push(s.log_len());
        states.push(model.clone());
    }
    s.commit().unwrap();
    (m, ends, states)
}

/// Un medio nuevo con el mismo checkpoint y generación que `m` pero con este diario.
fn medium_with(m: &MemMedium, log: Vec<u8>) -> MemMedium {
    let copy = MemMedium::new();
    copy.set_snapshot(m.snapshot_bytes());
    copy.set_generation(m.generation());
    copy.set_log(log);
    copy
}

fn reopen_with_log(
    m: &MemMedium,
    log: Vec<u8>,
    cfg: StoreConfig,
) -> Result<(Store<MemMedium>, Recovery), StoreError> {
    Store::open(medium_with(m, log), &KEY, cfg)
}

#[test]
fn poner_leer_borrar_y_recorrer_prefijos() {
    let m = MemMedium::new();
    let (mut s, rec) = open(&m);
    assert!(rec.created);
    s.put(b"a/1", b"uno").unwrap();
    s.put(b"a/2", b"dos").unwrap();
    s.put(b"b/1", b"tres").unwrap();
    assert_eq!(s.get(b"a/2"), Some(&b"dos"[..]));
    let a: Vec<_> = s.scan(b"a/").map(|(k, _)| k.to_vec()).collect();
    assert_eq!(a, vec![b"a/1".to_vec(), b"a/2".to_vec()]);
    s.delete(b"a/1").unwrap();
    assert!(!s.contains(b"a/1"));
    assert_eq!(s.len(), 2);
    assert_eq!(
        s.scan(b"zzz").count(),
        0,
        "un prefijo sin entradas no recorre nada"
    );
    assert_eq!(s.scan(b"").count(), 2);
}

#[test]
fn un_lote_es_atomico_incluso_con_la_cola_rota_en_cada_byte() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    s.put(b"base", b"1").unwrap();
    s.commit().unwrap();
    let before = m.log_bytes().len();
    let mut b = Batch::new();
    b.put(&b"x"[..], &b"1"[..])
        .delete(&b"base"[..])
        .put(&b"y"[..], &b"2"[..]);
    s.apply(&b).unwrap();
    let full = m.log_bytes();
    // El lote no se sincronizó: la caída conserva un prefijo cualquiera de sus bytes.
    for keep in 0..=(full.len() - before) {
        let (s2, rec) =
            reopen_with_log(&m, full[..before + keep].to_vec(), StoreConfig::default()).unwrap();
        let complete = keep == full.len() - before;
        if complete {
            assert_eq!(s2.get(b"x"), Some(&b"1"[..]));
            assert!(!s2.contains(b"base"));
            assert_eq!(s2.get(b"y"), Some(&b"2"[..]));
            assert_eq!(rec.torn_bytes, 0);
        } else {
            // Nada del lote, ni siquiera una de sus operaciones.
            assert!(!s2.contains(b"x") && !s2.contains(b"y"));
            assert_eq!(s2.get(b"base"), Some(&b"1"[..]));
            assert_eq!(
                rec.torn_bytes, keep as u64,
                "se descarta exactamente la cola rota"
            );
        }
    }
}

#[test]
fn truncar_el_diario_en_cada_byte_recupera_el_ultimo_registro_completo() {
    let (m, ends, states) = build(40, 11);
    let log = m.log_bytes();
    assert_eq!(*ends.last().unwrap() as usize, log.len());
    for cut in 0..=log.len() {
        let complete = ends.iter().take_while(|&&e| e as usize <= cut).count();
        let good = if complete == 0 {
            0
        } else {
            ends[complete - 1] as usize
        };
        let (s, rec) = reopen_with_log(&m, log[..cut].to_vec(), StoreConfig::default()).unwrap();
        assert_eq!(model_of(&s), states[complete], "corte en {cut}");
        assert_eq!(rec.records, complete as u64);
        assert_eq!(rec.torn_bytes, (cut - good) as u64, "corte en {cut}");
        assert_eq!(s.log_len(), good as u64);
    }
}

#[test]
fn el_almacen_reabierto_tras_cortar_sigue_aceptando_escrituras() {
    let (m, ends, states) = build(10, 3);
    let log = m.log_bytes();
    // Se corta a mitad del registro 7.
    let cut = (ends[5] as usize + ends[6] as usize) / 2;
    let copy = medium_with(&m, log[..cut].to_vec());
    let (mut s, rec) = Store::open(copy.clone(), &KEY, StoreConfig::default()).unwrap();
    assert!(rec.torn_bytes > 0);
    assert_eq!(model_of(&s), states[6]);
    s.put(b"nuevo", b"valor").unwrap();
    s.commit().unwrap();
    drop(s);
    // La cola rota se cortó de verdad: lo nuevo se lee tras reabrir y no queda basura delante.
    let (s2, rec2) = Store::open(copy, &KEY, StoreConfig::default()).unwrap();
    assert_eq!(rec2.torn_bytes, 0);
    assert_eq!(s2.get(b"nuevo"), Some(&b"valor"[..]));
    assert_eq!(
        s2.len(),
        states[6].len() + usize::from(!states[6].contains_key(&b"nuevo"[..]))
    );
}

#[test]
fn un_bit_cambiado_dentro_de_la_ventana_de_cola_descarta_ese_registro_y_los_siguientes_pero_nunca_acepta_datos_alterados(
) {
    let (m, ends, states) = build(12, 5);
    let log = m.log_bytes();
    for pos in 0..log.len() {
        let rec_index = ends.iter().take_while(|&&e| e as usize <= pos).count();
        for mask in [0x01u8, 0x80, 0xFF] {
            let mut damaged = log.clone();
            damaged[pos] ^= mask;
            let (s, rec) = reopen_with_log(&m, damaged, StoreConfig::default()).unwrap();
            // Todo lo anterior al registro dañado se conserva; nada de lo dañado entra.
            assert_eq!(
                model_of(&s),
                states[rec_index],
                "bit en {pos} (máscara {mask:#x})"
            );
            assert!(rec.torn_bytes > 0);
        }
    }
}

fn small_window() -> StoreConfig {
    StoreConfig {
        tail_window: 100,
        ..StoreConfig::default()
    }
}

#[test]
fn un_dano_lejos_de_la_cola_es_corrupcion_y_el_modo_reparar_lo_corta() {
    let (m, ends, states) = build(40, 21);
    let log = m.log_bytes();
    // Se daña un byte del registro 5 (el diario mide miles de bytes: está muy fuera de la ventana).
    let pos = (ends[4] as usize + ends[5] as usize) / 2;
    let mut damaged = log.clone();
    damaged[pos] ^= 0x40;
    match reopen_with_log(&m, damaged.clone(), small_window()) {
        Err(StoreError::Corrupt { offset, .. }) => assert_eq!(offset, ends[4]),
        other => panic!("debía ser corrupción, salió {:?}", other.map(|(_, r)| r)),
    }
    let cfg = StoreConfig {
        mode: OpenMode::Repair,
        ..small_window()
    };
    let (s, rec) = reopen_with_log(&m, damaged, cfg).unwrap();
    assert_eq!(model_of(&s), states[5]);
    assert_eq!(rec.repaired_bytes, log.len() as u64 - ends[4]);
    assert_eq!(rec.torn_bytes, 0);
}

#[test]
fn quitar_repetir_o_reordenar_registros_se_detecta() {
    let (m, ends, _) = build(30, 8);
    let log = m.log_bytes();
    let rec = |i: usize| -> Vec<u8> {
        let start = if i == 0 { 0 } else { ends[i - 1] as usize };
        log[start..ends[i] as usize].to_vec()
    };
    let join = |parts: Vec<Vec<u8>>| parts.concat();
    // Quitar el registro 3 de en medio.
    let mut without = Vec::new();
    for i in (0..30).filter(|&i| i != 3) {
        without.extend(rec(i));
    }
    assert!(matches!(
        reopen_with_log(&m, without, small_window()),
        Err(StoreError::Corrupt {
            what: "secuencia fuera de orden",
            ..
        })
    ));
    // Repetir el registro 3.
    let mut dup = Vec::new();
    for i in 0..30 {
        dup.extend(rec(i));
        if i == 3 {
            dup.extend(rec(3));
        }
    }
    assert!(matches!(
        reopen_with_log(&m, dup, small_window()),
        Err(StoreError::Corrupt {
            what: "secuencia fuera de orden",
            ..
        })
    ));
    // Intercambiar dos registros.
    let mut swapped: Vec<Vec<u8>> = (0..30).map(rec).collect();
    swapped.swap(4, 5);
    assert!(matches!(
        reopen_with_log(&m, join(swapped), small_window()),
        Err(StoreError::Corrupt {
            what: "secuencia fuera de orden",
            ..
        })
    ));
}

#[test]
fn clave_equivocada_falla_siempre_y_no_toca_el_diario() {
    let (m, _, _) = build(10, 2);
    let before = m.log_bytes();
    let other = [9u8; 32];
    for cfg in [StoreConfig::default(), small_window()] {
        match Store::open(m.clone(), &other, cfg) {
            Err(StoreError::WrongKey) => {}
            other => panic!("debía ser WrongKey, salió {:?}", other.map(|(_, r)| r)),
        }
    }
    assert_eq!(
        m.log_bytes(),
        before,
        "abrir con la clave equivocada no borra nada"
    );
    // Y con la buena sigue abriendo.
    assert!(Store::open(m, &KEY, StoreConfig::default()).is_ok());
}

#[test]
fn un_checkpoint_alterado_no_se_acepta() {
    let (m, _, _) = build(5, 4);
    let mut snap = m.snapshot_bytes().unwrap();
    let last = snap.len() - 1;
    snap[last] ^= 1;
    m.set_snapshot(Some(snap));
    assert!(matches!(
        Store::open(m.clone(), &KEY, StoreConfig::default()),
        Err(StoreError::WrongKey)
    ));
    // Un checkpoint truncado o que no es de TSOFGW tampoco.
    m.set_snapshot(Some(b"basura".to_vec()));
    assert!(matches!(
        Store::open(m.clone(), &KEY, StoreConfig::default()),
        Err(StoreError::Corrupt { .. })
    ));
    m.set_snapshot(None);
    assert!(
        matches!(
            Store::open(m, &KEY, StoreConfig::default()),
            Err(StoreError::Corrupt {
                what: "hay diario pero falta el checkpoint",
                ..
            })
        ),
        "un diario sin checkpoint no se toma por un almacén nuevo"
    );
}

#[test]
fn checkpoint_compacta_y_conserva_el_estado() {
    let (m, _, states) = build(50, 13);
    let (mut s, _) = open(&m);
    assert_eq!(model_of(&s), states[50]);
    assert!(s.log_len() > 0);
    let g = s.generation();
    s.checkpoint().unwrap();
    assert_eq!(s.generation(), g + 1);
    assert_eq!(s.log_len(), 0);
    s.put(b"despues", b"1").unwrap();
    s.commit().unwrap();
    drop(s);
    m.crash(CrashKind::Clean);
    let (s2, rec) = open(&m);
    assert_eq!(rec.generation, g + 1);
    assert_eq!(rec.records, 1, "solo se repite lo posterior al checkpoint");
    assert_eq!(rec.snapshot_entries as usize, states[50].len());
    assert_eq!(s2.get(b"despues"), Some(&b"1"[..]));
    for (k, v) in &states[50] {
        assert_eq!(s2.get(k), Some(v.as_slice()));
    }
}

#[test]
fn checkpoint_if_larger_solo_actua_cuando_toca() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    let before = s.stats().checkpoints;
    s.put(b"a", &[0u8; 100]).unwrap();
    assert!(!s.checkpoint_if_larger(10_000).unwrap());
    assert!(s.checkpoint_if_larger(50).unwrap());
    assert_eq!(s.log_len(), 0);
    assert_eq!(s.stats().checkpoints, before + 1);
}

#[test]
fn una_caida_a_mitad_de_un_checkpoint_deja_una_generacion_entera() {
    for fault in [InstallFault::BeforeRename, InstallFault::AfterRename] {
        let m = MemMedium::new();
        let (mut s, _) = open(&m);
        s.put(b"a", b"1").unwrap();
        s.put(b"b", b"2").unwrap();
        s.commit().unwrap();
        let g = s.generation();
        m.fail_install(Some(fault));
        assert!(s.checkpoint().is_err());
        assert!(s.is_poisoned());
        assert!(matches!(s.put(b"c", b"3"), Err(StoreError::Poisoned)));
        drop(s);
        m.crash(CrashKind::Clean);
        let (s2, _) = open(&m);
        assert_eq!(s2.get(b"a"), Some(&b"1"[..]), "{fault:?}");
        assert_eq!(s2.get(b"b"), Some(&b"2"[..]), "{fault:?}");
        let expected = if fault == InstallFault::BeforeRename {
            g
        } else {
            g + 1
        };
        assert_eq!(s2.generation(), expected, "{fault:?}");
        assert_eq!(
            m.stale_generations(),
            0,
            "los restos de la generación vieja se limpian al abrir"
        );
    }
}

#[test]
fn un_checkpoint_que_no_llega_a_instalarse_no_pierde_lo_escrito_sin_sincronizar() {
    // `checkpoint` sincroniza el diario viejo antes de instalar el nuevo: si la instalación no se completa
    // y el proceso cae, lo que se había aplicado (aunque nadie hubiera hecho `commit`) sigue en el diario
    // viejo. Un `checkpoint` que lo olvidara perdería escrituras que el programa creía a salvo.
    for fault in [InstallFault::BeforeRename, InstallFault::AfterRename] {
        let m = MemMedium::new();
        let (mut s, _) = open(&m);
        s.put(b"a", b"1").unwrap();
        s.commit().unwrap();
        s.put(b"b", b"2").unwrap(); // sin `commit`: solo está en lo no sincronizado
        m.fail_install(Some(fault));
        assert!(s.checkpoint().is_err());
        drop(s);
        m.crash(CrashKind::Clean); // se pierde todo lo que no se sincronizó
        let (s2, _) = open(&m);
        assert_eq!(s2.get(b"a"), Some(&b"1"[..]), "{fault:?}");
        assert_eq!(
            s2.get(b"b"),
            Some(&b"2"[..]),
            "{fault:?}: se perdió una escritura anterior al checkpoint"
        );
    }
}

#[test]
fn un_error_de_escritura_inutiliza_el_almacen_pero_no_el_estado_en_disco() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    s.put(b"a", b"1").unwrap();
    s.commit().unwrap();
    m.fail_appends(1);
    assert!(matches!(s.put(b"b", b"2"), Err(StoreError::Io(_))));
    assert!(s.is_poisoned());
    assert!(matches!(s.put(b"c", b"3"), Err(StoreError::Poisoned)));
    assert!(matches!(s.commit(), Err(StoreError::Poisoned)));
    // El estado en memoria no avanzó con lo que no se pudo escribir.
    assert!(!s.contains(b"b"));
    drop(s);
    // Y el disco tiene media escritura al final, que se descarta al reabrir.
    let (s2, rec) = open(&m);
    assert!(rec.torn_bytes > 0);
    assert_eq!(s2.get(b"a"), Some(&b"1"[..]));
    assert!(!s2.contains(b"b"));
}

#[test]
fn un_error_de_sincronizacion_tambien_inutiliza_el_almacen() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    s.put(b"a", b"1").unwrap();
    m.fail_syncs(1);
    assert!(s.commit().is_err());
    assert!(s.is_poisoned());
    drop(s);
    m.crash(CrashKind::Clean);
    let (s2, _) = open(&m);
    assert!(
        !s2.contains(b"a"),
        "lo que no se pudo sincronizar no cuenta como guardado"
    );
}

#[test]
fn una_caida_no_conserva_lo_no_sincronizado_pero_si_lo_sincronizado() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    s.put(b"seguro", b"1").unwrap();
    s.commit().unwrap();
    s.put(b"en_el_aire", b"2").unwrap();
    drop(s);
    m.crash(CrashKind::Clean);
    let (s2, rec) = open(&m);
    assert_eq!(s2.get(b"seguro"), Some(&b"1"[..]));
    assert!(!s2.contains(b"en_el_aire"));
    assert_eq!(rec.torn_bytes, 0, "una caída limpia no deja cola rota");
}

#[test]
fn un_hueco_de_ceros_en_la_cola_se_descarta() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    s.put(b"seguro", b"1").unwrap();
    s.commit().unwrap();
    s.put(b"en_el_aire", b"2").unwrap();
    let unsynced = s.unsynced();
    drop(s);
    m.crash(CrashKind::Hole);
    let (s2, rec) = open(&m);
    assert_eq!(rec.torn_bytes, unsynced);
    assert_eq!(s2.get(b"seguro"), Some(&b"1"[..]));
    assert!(!s2.contains(b"en_el_aire"));
}

#[test]
fn sincroniza_sola_al_pasar_el_limite_de_bytes_sin_sincronizar() {
    let m = MemMedium::new();
    let cfg = StoreConfig {
        max_unsynced: 200,
        ..StoreConfig::default()
    };
    let (mut s, _) = Store::open(m.clone(), &KEY, cfg).unwrap();
    let base = m.sync_count();
    for i in 0..20u32 {
        s.put(&i.to_le_bytes(), &[0u8; 40]).unwrap();
        assert!(s.unsynced() < 200 + 100, "lo no sincronizado está acotado");
    }
    assert!(m.sync_count() > base, "sincronizó sin que se lo pidieran");
    // Sin nada pendiente, `commit` no gasta una sincronización.
    s.commit().unwrap();
    let n = m.sync_count();
    s.commit().unwrap();
    assert_eq!(m.sync_count(), n);
}

#[test]
fn limites_de_tamano_y_claves_vacias() {
    let m = MemMedium::new();
    let (mut s, _) = open(&m);
    assert!(matches!(s.put(b"", b"x"), Err(StoreError::TooLarge(_))));
    assert!(matches!(
        s.put(&vec![1u8; MAX_KEY + 1], b"x"),
        Err(StoreError::TooLarge("clave"))
    ));
    assert!(matches!(
        s.put(b"k", &vec![1u8; MAX_VALUE + 1]),
        Err(StoreError::TooLarge("valor"))
    ));
    assert!(s.put(&vec![1u8; MAX_KEY], &vec![2u8; MAX_VALUE]).is_ok());
    let mut b = Batch::new();
    for i in 0..5u8 {
        b.put(vec![i + 1], vec![0u8; MAX_VALUE]);
    }
    assert!(matches!(s.apply(&b), Err(StoreError::TooLarge("lote"))));
    assert!(!s.is_poisoned(), "rechazar un lote no daña el almacén");
    // Un lote vacío no escribe nada.
    let before = s.log_len();
    s.apply(&Batch::new()).unwrap();
    assert_eq!(s.log_len(), before);
}

#[test]
fn el_diario_decodifica_o_rechaza_cualquier_carga_sin_reservar_de_mas() {
    // Una cuenta de operaciones enorme con una carga mínima no debe reservar memoria.
    let mut payload = Vec::new();
    put_varint(&mut payload, u64::MAX);
    assert!(Batch::decode(&payload).is_none());
    // Sobrantes, operaciones desconocidas y valores demasiado largos.
    let mut ok = Batch::new();
    ok.put(&b"k"[..], &b"v"[..]);
    let mut bytes = ok.encode();
    assert_eq!(Batch::decode(&bytes), Some(ok.clone()));
    bytes.push(0);
    assert!(Batch::decode(&bytes).is_none());
    let mut bad_op = ok.encode();
    bad_op[1] = 9;
    assert!(Batch::decode(&bad_op).is_none());
    for cut in 0..ok.encode().len() {
        assert!(Batch::decode(&ok.encode()[..cut]).is_none());
    }
}

#[test]
fn derive_key_separa_usos_y_secretos() {
    let a = derive_key(b"secreto", "shard-0");
    let b = derive_key(b"secreto", "shard-1");
    let c = derive_key(b"otro", "shard-0");
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_eq!(a, derive_key(b"secreto", "shard-0"));
}

#[test]
fn funciona_sobre_un_medio_dinamico() {
    let m = MemMedium::new();
    let medium: Box<dyn Medium> = Box::new(m.clone());
    let (mut s, _): (DynStore, _) = Store::open(medium, &KEY, StoreConfig::default()).unwrap();
    s.put(b"a", b"1").unwrap();
    s.commit().unwrap();
    drop(s);
    let (s2, _) = open(&m);
    assert_eq!(s2.get(b"a"), Some(&b"1"[..]));
}

#[test]
fn muchas_caidas_seguidas_recuperan_exactamente_un_prefijo_de_lo_aplicado() {
    // Propiedad: tras una caída con cola rota, el estado es el de «el último registro completo dentro
    // de lo sincronizado más lo que sobrevivió de la cola», y nunca nada distinto.
    let m = MemMedium::new();
    let mut rng = Rng(99);
    let (mut s, _) = open(&m);
    // Estado del último checkpoint y registros posteriores (con el final de cada uno y el estado tras él).
    let mut snap_state = Model::new();
    let mut recs: Vec<(u64, Model)> = Vec::new();
    let mut pending = Model::new();
    let mut synced_off = 0u64;
    let mut crashes = 0;
    for round in 0..300 {
        for _ in 0..1 + rng.below(6) {
            let b = random_batch(&mut rng);
            s.apply(&b).unwrap();
            apply_model(&mut pending, &b);
            recs.push((s.log_len(), pending.clone()));
        }
        match rng.below(6) {
            0 => {
                s.commit().unwrap();
                synced_off = s.log_len();
            }
            1 => {
                s.checkpoint().unwrap();
                snap_state = pending.clone();
                recs.clear();
                synced_off = 0;
            }
            2 | 3 => {
                let unsynced = s.unsynced();
                drop(s);
                let extra = rng.below(unsynced + 1);
                m.crash(CrashKind::Torn(extra as usize));
                let keep = synced_off + extra;
                let count = recs.iter().take_while(|(end, _)| *end <= keep).count();
                let expected = if count == 0 {
                    snap_state.clone()
                } else {
                    recs[count - 1].1.clone()
                };
                let (s2, rec) = open(&m);
                assert_eq!(model_of(&s2), expected, "ronda {round}");
                assert_eq!(rec.records, count as u64, "ronda {round}");
                recs.truncate(count);
                pending = expected;
                synced_off = s2.log_len();
                s = s2;
                crashes += 1;
            }
            _ => {}
        }
    }
    assert!(
        crashes > 50,
        "la prueba debía caerse muchas veces ({crashes})"
    );
}
