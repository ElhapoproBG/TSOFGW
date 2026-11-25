//! Estado cuantizado de las entidades, tal y como se replica a los clientes.
//!
//! Se calcula **una vez** cuando la entidad cambia (no una vez por observador) y es de solo lectura
//! durante la fase de replicación, de modo que muchos hilos pueden consultarlo a la vez.
//!
//! Cada NPC tiene además una nota de **propiedad**: en un servidor suelto todos son `Owned`; en un cluster,
//! cada shard es dueño de unos (`Owned`), ve copias de otros que están junto a su borde (`Ghost`), tiene
//! algunos en pleno traspaso (`Transferring`) y no sabe nada del resto (`Absent`).

use tsofgw_net::EntityState;
use tsofgw_world::{EntityId, FastMap};

use crate::server::NPC_BASE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Own {
    /// Este shard no lo tiene.
    Absent = 0,
    /// Este shard manda sobre él: lo simula.
    Owned = 1,
    /// Copia de solo lectura de un NPC que simula otro shard.
    Ghost = 2,
    /// Congelado aquí mientras otro shard confirma que lo ha recibido.
    Transferring = 3,
}

pub(crate) struct Entities {
    /// Indexado por `id - NPC_BASE`.
    npc: Vec<EntityState>,
    own: Vec<Own>,
    owned: usize,
    players: FastMap<EntityId, EntityState>,
}

impl Entities {
    pub fn new(npc_count: usize) -> Self {
        Entities {
            npc: Vec::with_capacity(npc_count),
            own: Vec::with_capacity(npc_count),
            owned: 0,
            players: FastMap::default(),
        }
    }

    pub fn push_npc(&mut self, st: EntityState, own: Own) {
        self.npc.push(st);
        self.own.push(own);
        self.owned += (own == Own::Owned) as usize;
    }

    pub fn npc_mut(&mut self, index: usize) -> &mut EntityState {
        &mut self.npc[index]
    }

    pub fn npc_ref(&self, index: usize) -> &EntityState {
        &self.npc[index]
    }

    pub fn npc_own(&self, index: usize) -> Own {
        self.own[index]
    }

    pub fn set_npc_own(&mut self, index: usize, own: Own) {
        let old = std::mem::replace(&mut self.own[index], own);
        self.owned = self.owned + (own == Own::Owned) as usize - (old == Own::Owned) as usize;
    }

    /// NPC que este servidor simula.
    pub fn owned_npcs(&self) -> usize {
        self.owned
    }

    pub fn set_player(&mut self, id: EntityId, st: EntityState) {
        self.players.insert(id, st);
    }

    pub fn remove_player(&mut self, id: EntityId) {
        self.players.remove(&id);
    }

    /// Estado de una entidad que este servidor conoce (propia o fantasma).
    pub fn get(&self, id: EntityId) -> Option<&EntityState> {
        if id >= NPC_BASE {
            let i = (id - NPC_BASE) as usize;
            match self.own.get(i) {
                Some(Own::Absent) | None => None,
                Some(_) => self.npc.get(i),
            }
        } else {
            self.players.get(&id)
        }
    }
}
