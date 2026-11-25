//! Dónde viven los bytes del almacén: un trait mínimo ([`Medium`]) y dos implementaciones.
//!
//! * [`FsMedium`]: archivos en una carpeta (diario + checkpoint por generación, con bloqueo exclusivo).
//! * [`MemMedium`]: todo en memoria, con **inyección de fallos** (caídas con cola rota, huecos, errores de
//!   escritura, caídas a mitad de un checkpoint). Es lo que permite probar la recuperación en cada byte.
//!
//! El almacén ([`crate::Store`]) es el mismo para las dos: toda la lógica de formato y recuperación se
//! prueba una vez, contra el medio que sabe fallar.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Lo que un medio devuelve al abrirse: la última generación completa.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Número de la generación (0 si el medio es nuevo y no tiene checkpoint).
    pub generation: u64,
    /// Bytes del checkpoint de esa generación (`None` solo en un medio nuevo).
    pub snapshot: Option<Vec<u8>>,
    /// Bytes del diario que sigue a ese checkpoint (puede acabar en un registro roto por una caída).
    pub log: Vec<u8>,
}

/// Almacenamiento de bytes con las garantías que necesita un diario con checkpoints.
pub trait Medium {
    /// Lee la última generación completa (y borra restos de intentos de checkpoint que no llegaron a
    /// completarse).
    fn load(&mut self) -> io::Result<Loaded>;
    /// Añade bytes al final del diario actual. No son durables hasta [`Medium::sync`].
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Hace durable todo lo añadido hasta ahora.
    fn sync(&mut self) -> io::Result<()>;
    /// Corta el diario a `len` bytes (descarta una cola rota) y lo hace durable.
    fn truncate_log(&mut self, len: u64) -> io::Result<()>;
    /// Instala el checkpoint de la generación `generation` y empieza su diario vacío, **de forma
    /// atómica**: tras una caída está entera la generación anterior o entera la nueva, nunca mezcladas.
    fn install_checkpoint(&mut self, generation: u64, snapshot: &[u8]) -> io::Result<()>;
}

impl<M: Medium + ?Sized> Medium for Box<M> {
    fn load(&mut self) -> io::Result<Loaded> {
        (**self).load()
    }
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        (**self).append(bytes)
    }
    fn sync(&mut self) -> io::Result<()> {
        (**self).sync()
    }
    fn truncate_log(&mut self, len: u64) -> io::Result<()> {
        (**self).truncate_log(len)
    }
    fn install_checkpoint(&mut self, generation: u64, snapshot: &[u8]) -> io::Result<()> {
        (**self).install_checkpoint(generation, snapshot)
    }
}

// ====================================================================== memoria con fallos

/// Cómo se pierde lo que aún no se sincronizó cuando el proceso cae.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    /// Se pierde todo lo no sincronizado.
    Clean,
    /// Sobrevive un prefijo de lo no sincronizado (una escritura a medias).
    Torn(usize),
    /// El archivo llegó a crecer lo no sincronizado, pero con ceros: los datos no llegaron a disco.
    Hole,
}

/// En qué punto de un checkpoint se simula la caída.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallFault {
    /// El checkpoint nuevo se escribió pero no llegó a renombrarse: sigue valiendo la generación anterior.
    BeforeRename,
    /// Se renombró (ya vale la generación nueva) pero la anterior no se borró.
    AfterRename,
}

#[derive(Default)]
struct MemInner {
    generation: u64,
    snapshot: Option<Vec<u8>>,
    log: Vec<u8>,
    /// Bytes del diario que ya son durables.
    synced: usize,
    /// Generación anterior que sobrevive a un checkpoint a medias (para comprobar que se limpia).
    stale: Vec<(u64, Vec<u8>)>,
    fail_appends: u32,
    fail_syncs: u32,
    fail_install: Option<InstallFault>,
    appends: u64,
    syncs: u64,
}

/// Medio en memoria. Es un **identificador compartido** (`Clone` apunta a lo mismo): el almacén usa una
/// copia y la prueba conserva otra para provocar caídas, dañar bytes o leer lo que hay «en disco».
#[derive(Clone, Default)]
pub struct MemMedium {
    inner: Arc<Mutex<MemInner>>,
}

impl MemMedium {
    pub fn new() -> MemMedium {
        MemMedium::default()
    }

    fn lock(&self) -> MutexGuard<'_, MemInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Simula que el proceso cae: lo no sincronizado se pierde según `kind`. Después se puede reabrir.
    pub fn crash(&self, kind: CrashKind) {
        let mut m = self.lock();
        let synced = m.synced;
        match kind {
            CrashKind::Clean => m.log.truncate(synced),
            CrashKind::Torn(extra) => {
                let keep = (synced + extra).min(m.log.len());
                m.log.truncate(keep);
            }
            CrashKind::Hole => {
                for b in m.log[synced..].iter_mut() {
                    *b = 0;
                }
            }
        }
        // Lo que queda en el medio tras reiniciar es, por definición, lo que había en disco.
        m.synced = m.log.len();
    }

    /// Bytes del diario actual.
    pub fn log_bytes(&self) -> Vec<u8> {
        self.lock().log.clone()
    }

    /// Sustituye el diario entero (para fabricar daños o recortes exactos).
    pub fn set_log(&self, bytes: Vec<u8>) {
        let mut m = self.lock();
        m.synced = bytes.len();
        m.log = bytes;
    }

    /// Bytes del checkpoint actual.
    pub fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        self.lock().snapshot.clone()
    }

    pub fn set_snapshot(&self, bytes: Option<Vec<u8>>) {
        self.lock().snapshot = bytes;
    }

    /// Invierte bits del diario (`mask`) en `offset`.
    pub fn flip_log(&self, offset: usize, mask: u8) {
        self.lock().log[offset] ^= mask;
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Fija la generación (para montar un medio a partir de piezas de otro en las pruebas).
    pub fn set_generation(&self, generation: u64) {
        self.lock().generation = generation;
    }

    /// Cuántas generaciones viejas sobreviven (restos de un checkpoint interrumpido).
    pub fn stale_generations(&self) -> usize {
        self.lock().stale.len()
    }

    /// Los próximos `n` `append` fallan con un error de E/S.
    pub fn fail_appends(&self, n: u32) {
        self.lock().fail_appends = n;
    }

    /// Los próximos `n` `sync` fallan.
    pub fn fail_syncs(&self, n: u32) {
        self.lock().fail_syncs = n;
    }

    /// El próximo checkpoint se interrumpe en ese punto (devuelve un error y deja el medio como estaría
    /// tras una caída justo ahí).
    pub fn fail_install(&self, at: Option<InstallFault>) {
        self.lock().fail_install = at;
    }

    pub fn append_count(&self) -> u64 {
        self.lock().appends
    }

    pub fn sync_count(&self) -> u64 {
        self.lock().syncs
    }
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("fallo inyectado: {what}"))
}

impl Medium for MemMedium {
    fn load(&mut self) -> io::Result<Loaded> {
        let mut m = self.lock();
        m.stale.clear();
        m.synced = m.log.len();
        Ok(Loaded {
            generation: m.generation,
            snapshot: m.snapshot.clone(),
            log: m.log.clone(),
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut m = self.lock();
        if m.fail_appends > 0 {
            m.fail_appends -= 1;
            // Una escritura que falla puede dejar escrita una parte.
            let part = bytes.len() / 2;
            m.log.extend_from_slice(&bytes[..part]);
            return Err(injected("append"));
        }
        m.appends += 1;
        m.log.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut m = self.lock();
        if m.fail_syncs > 0 {
            m.fail_syncs -= 1;
            return Err(injected("sync"));
        }
        m.syncs += 1;
        m.synced = m.log.len();
        Ok(())
    }

    fn truncate_log(&mut self, len: u64) -> io::Result<()> {
        let mut m = self.lock();
        m.log.truncate(len as usize);
        m.synced = m.log.len();
        Ok(())
    }

    fn install_checkpoint(&mut self, generation: u64, snapshot: &[u8]) -> io::Result<()> {
        let mut m = self.lock();
        match m.fail_install.take() {
            Some(InstallFault::BeforeRename) => {
                return Err(injected("checkpoint antes de renombrar"))
            }
            Some(InstallFault::AfterRename) => {
                let old = (m.generation, std::mem::take(&mut m.log));
                if let Some(s) = m.snapshot.take() {
                    m.stale.push((old.0, s));
                }
                m.snapshot = Some(snapshot.to_vec());
                m.generation = generation;
                m.synced = 0;
                return Err(injected("checkpoint tras renombrar"));
            }
            None => {}
        }
        m.snapshot = Some(snapshot.to_vec());
        m.generation = generation;
        m.log.clear();
        m.synced = 0;
        m.stale.clear();
        Ok(())
    }
}

// ====================================================================== archivos

const LOCK_NAME: &str = "LOCK";

/// Medio sobre archivos de una carpeta:
///
/// ```text
/// LOCK                         bloqueo exclusivo (un solo proceso por carpeta)
/// snap-<generación>.bin        checkpoint (se escribe como .tmp, se sincroniza y se renombra)
/// wal-<generación>.log         diario que sigue a ese checkpoint
/// ```
///
/// La generación vigente es la más alta con un `snap-*.bin` completo; todo lo demás son restos que
/// `load` borra.
pub struct FsMedium {
    dir: PathBuf,
    /// Se mantiene abierto mientras viva el medio: el bloqueo se libera solo al cerrarse (también si
    /// el proceso muere).
    _lock: File,
    generation: u64,
    wal: Option<File>,
}

fn snap_name(generation: u64) -> String {
    format!("snap-{generation:020}.bin")
}
fn wal_name(generation: u64) -> String {
    format!("wal-{generation:020}.log")
}
fn tmp_name(generation: u64) -> String {
    format!("snap-{generation:020}.tmp")
}

fn parse_generation(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let digits = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// En Windows no se puede abrir una carpeta como archivo con la biblioteca estándar; NTFS registra los
/// cambios de nombres en su propio diario, así que el renombrado ya es durable en la práctica.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

impl FsMedium {
    /// Abre (y crea si hace falta) la carpeta de datos y toma su bloqueo. Falla con
    /// [`io::ErrorKind::WouldBlock`] si otro proceso (u otro `FsMedium` de este) ya la tiene abierta.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<FsMedium> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_NAME))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "la carpeta de datos {} ya está abierta por otro proceso",
                        dir.display()
                    ),
                ))
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        Ok(FsMedium {
            dir,
            _lock: lock,
            generation: 0,
            wal: None,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn open_wal(&self, generation: u64, create: bool) -> io::Result<File> {
        let path = self.dir.join(wal_name(generation));
        let mut f = OpenOptions::new()
            .create(create)
            .truncate(false)
            .write(true)
            .read(true)
            .open(path)?;
        // No se usa O_APPEND: en Windows un archivo abierto solo para añadir no se puede recortar.
        f.seek(SeekFrom::End(0))?;
        Ok(f)
    }
}

impl Medium for FsMedium {
    fn load(&mut self) -> io::Result<Loaded> {
        let mut snaps: Vec<u64> = Vec::new();
        let mut stray: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == LOCK_NAME {
                continue;
            }
            if let Some(g) = parse_generation(name, "snap-", ".bin") {
                snaps.push(g);
            } else if name.starts_with("snap-") && name.ends_with(".tmp") {
                stray.push(entry.path());
            }
        }
        snaps.sort_unstable();
        let current = snaps.last().copied();
        for p in stray {
            let _ = fs::remove_file(p);
        }
        let generation = current.unwrap_or(0);
        let snapshot = match current {
            Some(g) => Some(fs::read(self.dir.join(snap_name(g)))?),
            None => None,
        };
        // Restos de otras generaciones (de un checkpoint interrumpido): se limpian.
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let old_snap = parse_generation(name, "snap-", ".bin").is_some_and(|g| g != generation);
            let old_wal = parse_generation(name, "wal-", ".log").is_some_and(|g| g != generation);
            if old_snap || old_wal {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut wal = self.open_wal(generation, true)?;
        wal.seek(SeekFrom::Start(0))?;
        let mut log = Vec::new();
        wal.read_to_end(&mut log)?;
        wal.seek(SeekFrom::End(0))?;
        self.generation = generation;
        self.wal = Some(wal);
        Ok(Loaded {
            generation,
            snapshot,
            log,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.wal.as_mut() {
            Some(f) => f.write_all(bytes),
            None => Err(io::Error::other("el medio no se ha cargado")),
        }
    }

    fn sync(&mut self) -> io::Result<()> {
        match self.wal.as_mut() {
            Some(f) => f.sync_data(),
            None => Err(io::Error::other("el medio no se ha cargado")),
        }
    }

    fn truncate_log(&mut self, len: u64) -> io::Result<()> {
        match self.wal.as_mut() {
            Some(f) => {
                f.set_len(len)?;
                f.seek(SeekFrom::Start(len))?;
                f.sync_all()
            }
            None => Err(io::Error::other("el medio no se ha cargado")),
        }
    }

    fn install_checkpoint(&mut self, generation: u64, snapshot: &[u8]) -> io::Result<()> {
        // 1. El checkpoint se escribe completo en un archivo temporal y se sincroniza.
        let tmp = self.dir.join(tmp_name(generation));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(snapshot)?;
            f.sync_all()?;
        }
        // 2. Su diario (vacío) existe antes de que el checkpoint sea visible.
        let new_wal_path = self.dir.join(wal_name(generation));
        let new_wal = {
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .read(true)
                .open(&new_wal_path)?;
            f.sync_all()?;
            f
        };
        // 3. El renombrado es el instante en que la generación nueva pasa a valer.
        fs::rename(&tmp, self.dir.join(snap_name(generation)))?;
        sync_dir(&self.dir)?;
        // 4. Se pasa al diario nuevo y se borra la generación anterior.
        let old = self.generation;
        self.wal = Some(new_wal);
        self.generation = generation;
        if old != generation {
            let _ = fs::remove_file(self.dir.join(snap_name(old)));
            let _ = fs::remove_file(self.dir.join(wal_name(old)));
        }
        Ok(())
    }
}
