//! Núcleo del grafo de TSOFGW.
//!
//! Cada nodo declara de quién depende. El motor evalúa bajo demanda:
//! un nodo solo corre si algún nodo "demandado" (raíz) lo necesita,
//! directa o indirectamente. El resto se hiberna y no consume CPU.

use std::collections::{HashMap, HashSet};

pub type NodeId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Hibernated,
}

/// Contrato mínimo de un nodo: capacidades declaradas + trabajo por tick.
pub trait Node {
    fn name(&self) -> &str;
    /// Permisos que el nodo pide (p. ej. "net", "disk"). Base del modelo de amenazas.
    fn capabilities(&self) -> &[&'static str] {
        &[]
    }
    fn tick(&mut self);
}

struct Slot {
    node: Box<dyn Node>,
    deps: Vec<NodeId>,
    demanded: bool,
    state: State,
    ticks: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GraphError {
    UnknownDependency(NodeId),
    Cycle,
}

#[derive(Default)]
pub struct Graph {
    slots: Vec<Slot>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Añade un nodo con sus dependencias (que ya deben existir: así no hay ciclos).
    pub fn add(&mut self, node: Box<dyn Node>, deps: &[NodeId]) -> Result<NodeId, GraphError> {
        for &d in deps {
            if d as usize >= self.slots.len() {
                return Err(GraphError::UnknownDependency(d));
            }
        }
        let id = self.slots.len() as NodeId;
        self.slots.push(Slot { node, deps: deps.to_vec(), demanded: false, state: State::Hibernated, ticks: 0 });
        Ok(id)
    }

    /// Marca o desmarca un nodo como demandado (p. ej. render de un jugador presente).
    pub fn set_demanded(&mut self, id: NodeId, demanded: bool) {
        self.slots[id as usize].demanded = demanded;
    }

    /// Recalcula qué nodos deben estar activos: cierre transitivo de las dependencias.
    pub fn resolve(&mut self) {
        let mut needed: HashSet<NodeId> = HashSet::new();
        let mut stack: Vec<NodeId> =
            (0..self.slots.len() as NodeId).filter(|&i| self.slots[i as usize].demanded).collect();
        while let Some(i) = stack.pop() {
            if needed.insert(i) {
                stack.extend(self.slots[i as usize].deps.iter().copied());
            }
        }
        for (i, s) in self.slots.iter_mut().enumerate() {
            s.state = if needed.contains(&(i as NodeId)) { State::Active } else { State::Hibernated };
        }
    }

    /// Ejecuta un tick: solo nodos activos, en orden de inserción (ya topológico).
    pub fn tick(&mut self) {
        self.resolve();
        for s in self.slots.iter_mut().filter(|s| s.state == State::Active) {
            s.node.tick();
            s.ticks += 1;
        }
    }

    pub fn state(&self, id: NodeId) -> State {
        self.slots[id as usize].state
    }
    pub fn ticks(&self, id: NodeId) -> u64 {
        self.slots[id as usize].ticks
    }

    /// Permisos pedidos por cada nodo, para auditar o aplicar sandbox al cargar.
    pub fn capability_report(&self) -> HashMap<String, Vec<&'static str>> {
        self.slots.iter().map(|s| (s.node.name().to_string(), s.node.capabilities().to_vec())).collect()
    }

    /// Hash de contenido de un identificador de módulo (usa la librería C).
    pub fn module_id(bytes: &[u8]) -> u64 {
        tsofgw_sys::hash64(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy(&'static str, &'static [&'static str]);
    impl Node for Dummy {
        fn name(&self) -> &str { self.0 }
        fn capabilities(&self) -> &[&'static str] { self.1 }
        fn tick(&mut self) {}
    }
    fn d(n: &'static str) -> Box<dyn Node> { Box::new(Dummy(n, &[])) }

    #[test]
    fn apagado_selectivo() {
        let mut g = Graph::new();
        let fisica = g.add(d("fisica"), &[]).unwrap();
        let red = g.add(Box::new(Dummy("red", &["net"])), &[]).unwrap();
        let render = g.add(d("render"), &[fisica]).unwrap();
        let audio = g.add(d("audio"), &[]).unwrap();

        g.set_demanded(render, true);
        g.tick();
        assert_eq!(g.state(render), State::Active);
        assert_eq!(g.state(fisica), State::Active); // dependencia transitiva
        assert_eq!(g.state(red), State::Hibernated);
        assert_eq!(g.state(audio), State::Hibernated);
        assert_eq!(g.ticks(red), 0);

        g.set_demanded(render, false); // nadie lo pide: todo se apaga
        g.tick();
        assert_eq!(g.state(fisica), State::Hibernated);
        assert_eq!(g.ticks(fisica), 1);
    }

    #[test]
    fn dependencia_inexistente() {
        let mut g = Graph::new();
        assert_eq!(g.add(d("x"), &[7]).err(), Some(GraphError::UnknownDependency(7)));
    }

    #[test]
    fn reporte_de_capacidades() {
        let mut g = Graph::new();
        g.add(Box::new(Dummy("red", &["net"])), &[]).unwrap();
        assert_eq!(g.capability_report()["red"], vec!["net"]);
    }
}
