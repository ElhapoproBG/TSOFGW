//! Fragmentación y reensamblado de mensajes más grandes que un datagrama.

use std::collections::HashMap;

pub const MAX_FRAGMENTS: usize = 16;
const MAX_PARTIALS: usize = 8;

struct Partial {
    count: u8,
    got: u16,
    parts: Vec<Vec<u8>>,
    first_seen: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Complete(Vec<u8>),
    Pending,
    Invalid,
}

pub struct Reassembler {
    partial: HashMap<u16, Partial>,
    timeout_ms: u64,
}

impl Reassembler {
    pub fn new(timeout_ms: u64) -> Self {
        Reassembler {
            partial: HashMap::new(),
            timeout_ms,
        }
    }

    pub fn push(&mut self, now: u64, msg_id: u16, idx: u8, count: u8, data: &[u8]) -> Push {
        if count == 0 || count as usize > MAX_FRAGMENTS || idx >= count {
            return Push::Invalid;
        }
        if count == 1 {
            return Push::Complete(data.to_vec());
        }
        self.expire(now);
        if !self.partial.contains_key(&msg_id) && self.partial.len() >= MAX_PARTIALS {
            // Hay demasiados mensajes a medias: se descarta el más antiguo.
            if let Some(oldest) = self
                .partial
                .iter()
                .min_by_key(|(_, p)| p.first_seen)
                .map(|(k, _)| *k)
            {
                self.partial.remove(&oldest);
            }
        }
        let p = self.partial.entry(msg_id).or_insert_with(|| Partial {
            count,
            got: 0,
            parts: vec![Vec::new(); count as usize],
            first_seen: now,
        });
        if p.count != count {
            self.partial.remove(&msg_id);
            return Push::Invalid;
        }
        if p.got & (1 << idx) != 0 {
            return Push::Pending; // duplicado
        }
        p.got |= 1 << idx;
        p.parts[idx as usize] = data.to_vec();
        if p.got.count_ones() as u8 == p.count {
            let p = self.partial.remove(&msg_id).expect("acaba de insertarse");
            return Push::Complete(p.parts.concat());
        }
        Push::Pending
    }

    pub fn expire(&mut self, now: u64) {
        let timeout = self.timeout_ms;
        self.partial
            .retain(|_, p| now.saturating_sub(p.first_seen) <= timeout);
    }

    #[cfg(test)]
    pub fn in_progress(&self) -> usize {
        self.partial.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_fragmento_pasa_directo() {
        let mut r = Reassembler::new(1000);
        assert_eq!(
            r.push(0, 1, 0, 1, b"hola"),
            Push::Complete(b"hola".to_vec())
        );
        assert_eq!(r.in_progress(), 0);
    }

    #[test]
    fn reensambla_en_cualquier_orden_y_con_duplicados() {
        let mut r = Reassembler::new(1000);
        assert_eq!(r.push(0, 9, 2, 3, b"cc"), Push::Pending);
        assert_eq!(r.push(0, 9, 2, 3, b"cc"), Push::Pending);
        assert_eq!(r.push(1, 9, 0, 3, b"aa"), Push::Pending);
        assert_eq!(
            r.push(2, 9, 1, 3, b"bb"),
            Push::Complete(b"aabbcc".to_vec())
        );
        assert_eq!(r.in_progress(), 0);
    }

    #[test]
    fn descarta_invalidos_y_caducados() {
        let mut r = Reassembler::new(100);
        assert_eq!(r.push(0, 1, 0, 0, b""), Push::Invalid);
        assert_eq!(r.push(0, 1, 3, 3, b""), Push::Invalid);
        assert_eq!(r.push(0, 1, 0, 17, b""), Push::Invalid);
        assert_eq!(r.push(0, 5, 0, 2, b"a"), Push::Pending);
        assert_eq!(r.in_progress(), 1);
        // Pasado el plazo, el fragmento viejo no completa nada.
        assert_eq!(r.push(500, 5, 1, 2, b"b"), Push::Pending);
        assert_eq!(r.in_progress(), 1);
        // Cambiar el total del mismo mensaje invalida.
        assert_eq!(r.push(501, 5, 0, 3, b"x"), Push::Invalid);
    }

    #[test]
    fn limita_mensajes_a_medias() {
        let mut r = Reassembler::new(10_000);
        for id in 0..100u16 {
            r.push(id as u64, id, 0, 2, b"x");
        }
        assert!(r.in_progress() <= MAX_PARTIALS);
    }
}
