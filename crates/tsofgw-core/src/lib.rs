//! Núcleo del grafo de TSOFGW.
//!
//! * **Contratos tipados:** puertos de entrada/salida con tipo, permisos y costo.
//! * **Flujo de datos:** los nodos leen entradas y escriben salidas mediante [`NodeIo`].
//! * **Apagado selectivo:** un nodo solo corre si algo demandado lo necesita.
//! * **Presupuesto:** si lo demandado no cabe, se descartan las raíces de menor prioridad.
//! * **Ejecución paralela por capas:** nodos de la misma profundidad son independientes.
//! * **Hot-swap:** un nodo se reemplaza en caliente si el nuevo contrato es compatible.
//! * **Inspección:** exportación a Graphviz (DOT) con estado, costos y conexiones.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub type NodeId = u32;

/// Tipo de dato que viaja por un puerto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Unit,
    Int,
    Float,
    Bytes,
    Transform,
    Frame,
    Events,
}

/// Posición y rotación (cuaternión) en el espacio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub pos: [f32; 3],
    pub rot: [f32; 4],
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            pos: [0.0; 3],
            rot: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

/// Valor que viaja por un puerto. Los datos grandes se comparten con `Arc` (copia barata).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Unit,
    Int(i64),
    Float(f64),
    Bytes(Arc<[u8]>),
    Transform(Transform),
    Frame(u64),
    Events(Arc<[u64]>),
}

impl Value {
    pub fn ty(&self) -> DataType {
        match self {
            Value::Unit => DataType::Unit,
            Value::Int(_) => DataType::Int,
            Value::Float(_) => DataType::Float,
            Value::Bytes(_) => DataType::Bytes,
            Value::Transform(_) => DataType::Transform,
            Value::Frame(_) => DataType::Frame,
            Value::Events(_) => DataType::Events,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub name: &'static str,
    pub ty: DataType,
}

/// Costo declarado por tick. Unidades: microsegundos (CPU/GPU) y KiB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    pub cpu_us: u32,
    pub gpu_us: u32,
    pub mem_kb: u32,
}

impl Cost {
    pub fn plus(self, o: Cost) -> Cost {
        Cost {
            cpu_us: self.cpu_us.saturating_add(o.cpu_us),
            gpu_us: self.gpu_us.saturating_add(o.gpu_us),
            mem_kb: self.mem_kb.saturating_add(o.mem_kb),
        }
    }
    pub fn fits(&self, budget: &Cost) -> bool {
        self.cpu_us <= budget.cpu_us && self.gpu_us <= budget.gpu_us && self.mem_kb <= budget.mem_kb
    }
}

/// Contrato público de un nodo. Versión `(mayor, menor)`.
#[derive(Debug, Clone)]
pub struct Contract {
    pub name: &'static str,
    pub version: (u16, u16),
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
    pub capabilities: Vec<&'static str>,
    pub cost: Cost,
}

impl Contract {
    pub fn new(name: &'static str, version: (u16, u16)) -> Self {
        Contract {
            name,
            version,
            inputs: vec![],
            outputs: vec![],
            capabilities: vec![],
            cost: Cost::default(),
        }
    }
    pub fn input(mut self, name: &'static str, ty: DataType) -> Self {
        self.inputs.push(Port { name, ty });
        self
    }
    pub fn output(mut self, name: &'static str, ty: DataType) -> Self {
        self.outputs.push(Port { name, ty });
        self
    }
    pub fn cap(mut self, c: &'static str) -> Self {
        self.capabilities.push(c);
        self
    }
    pub fn cost(mut self, cost: Cost) -> Self {
        self.cost = cost;
        self
    }
}

/// Entradas y salidas de un nodo durante un tick.
#[derive(Debug, Default)]
pub struct NodeIo {
    inputs: Vec<(&'static str, Value)>,
    outputs: Vec<(&'static str, Value)>,
}

impl NodeIo {
    pub fn input(&self, name: &str) -> Option<&Value> {
        self.inputs.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.input(name) {
            Some(Value::Float(f)) => Some(*f),
            _ => None,
        }
    }
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.input(name) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }
    /// Escribe una salida (reemplaza la anterior del mismo nombre).
    pub fn set_output(&mut self, name: &'static str, value: Value) {
        if let Some(slot) = self.outputs.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else {
            self.outputs.push((name, value));
        }
    }
}

/// Un nodo del motor. Debe ser `Send` para poder ejecutarse en paralelo.
pub trait Node: Send {
    fn contract(&self) -> Contract;
    fn tick(&mut self, io: &mut NodeIo);
    /// Estado serializado para transferirlo al reemplazar el nodo en caliente.
    fn export_state(&self) -> Vec<u8> {
        Vec::new()
    }
    fn import_state(&mut self, _state: &[u8]) {}
}

/// Nodo definido por una función: la forma más corta de crear uno.
pub struct FnNode<F: FnMut(&mut NodeIo) + Send> {
    contract: Contract,
    f: F,
}

impl<F: FnMut(&mut NodeIo) + Send> Node for FnNode<F> {
    fn contract(&self) -> Contract {
        self.contract.clone()
    }
    fn tick(&mut self, io: &mut NodeIo) {
        (self.f)(io);
    }
}

/// Crea un nodo a partir de un contrato y una función.
pub fn fn_node<F>(contract: Contract, f: F) -> Box<dyn Node>
where
    F: FnMut(&mut NodeIo) + Send + 'static,
{
    Box::new(FnNode { contract, f })
}

/// Conexión: salida `output` del nodo `from` hacia la entrada `input` del nodo nuevo.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    pub from: NodeId,
    pub output: &'static str,
    pub input: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Hibernated,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GraphError {
    UnknownNode(NodeId),
    UnknownPort {
        node: NodeId,
        port: &'static str,
    },
    TypeMismatch {
        output: DataType,
        input: DataType,
    },
    DuplicateInput(&'static str),
    /// El contrato nuevo no es compatible con el anterior (motivo en texto).
    IncompatibleContract(&'static str),
    /// El nodo nuevo pide permisos que el anterior no tenía.
    CapabilityEscalation(Vec<&'static str>),
}

struct Slot {
    node: Box<dyn Node>,
    contract: Contract,
    links: Vec<Link>,
    depth: u32,
    demanded: bool,
    priority: u8,
    state: State,
    ticks: u64,
    out: HashMap<&'static str, Value>,
}

#[derive(Default)]
pub struct Graph {
    slots: Vec<Slot>,
    budget: Option<Cost>,
    active_cost: Cost,
    shed: Vec<NodeId>,
    violations: u64,
}

/// Ejecuta un nodo y valida sus salidas contra el contrato. Devuelve cuántas salidas eran inválidas.
fn run_slot(s: &mut Slot, io: &mut NodeIo) -> u64 {
    s.node.tick(io);
    s.ticks += 1;
    s.out.clear();
    let mut bad = 0;
    for (name, v) in std::mem::take(&mut io.outputs) {
        match s.contract.outputs.iter().find(|p| p.name == name) {
            Some(p) if p.ty == v.ty() => {
                s.out.insert(name, v);
            }
            _ => bad += 1,
        }
    }
    bad
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Añade un nodo. Las conexiones solo pueden apuntar a nodos existentes, así que no hay ciclos.
    pub fn add(&mut self, node: Box<dyn Node>, links: &[Link]) -> Result<NodeId, GraphError> {
        let contract = node.contract();
        let new_id = self.slots.len() as NodeId;
        let mut seen = HashSet::new();
        let mut depth = 0;
        for l in links {
            let src = self
                .slots
                .get(l.from as usize)
                .ok_or(GraphError::UnknownNode(l.from))?;
            let out = src
                .contract
                .outputs
                .iter()
                .find(|p| p.name == l.output)
                .ok_or(GraphError::UnknownPort {
                    node: l.from,
                    port: l.output,
                })?;
            let inp = contract.inputs.iter().find(|p| p.name == l.input).ok_or(
                GraphError::UnknownPort {
                    node: new_id,
                    port: l.input,
                },
            )?;
            if out.ty != inp.ty {
                return Err(GraphError::TypeMismatch {
                    output: out.ty,
                    input: inp.ty,
                });
            }
            if !seen.insert(l.input) {
                return Err(GraphError::DuplicateInput(l.input));
            }
            depth = depth.max(src.depth + 1);
        }
        self.slots.push(Slot {
            node,
            contract,
            links: links.to_vec(),
            depth,
            demanded: false,
            priority: 0,
            state: State::Hibernated,
            ticks: 0,
            out: HashMap::new(),
        });
        Ok(new_id)
    }

    /// Marca un nodo como demandado (p. ej. el render de un jugador presente).
    pub fn set_demanded(&mut self, id: NodeId, demanded: bool) {
        self.slots[id as usize].demanded = demanded;
    }

    /// Prioridad de una raíz demandada: mayor número = se conserva primero bajo presupuesto.
    pub fn set_priority(&mut self, id: NodeId, priority: u8) {
        self.slots[id as usize].priority = priority;
    }

    /// Presupuesto por tick (`None` = sin límite).
    pub fn set_budget(&mut self, budget: Option<Cost>) {
        self.budget = budget;
    }

    fn closure(&self, root: NodeId) -> HashSet<NodeId> {
        let mut set = HashSet::new();
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            if set.insert(i) {
                stack.extend(self.slots[i as usize].links.iter().map(|l| l.from));
            }
        }
        set
    }

    /// Decide qué nodos están activos: cierre de dependencias de las raíces demandadas,
    /// admitidas por prioridad mientras quepan en el presupuesto.
    pub fn resolve(&mut self) {
        let mut roots: Vec<NodeId> = (0..self.slots.len() as NodeId)
            .filter(|&i| self.slots[i as usize].demanded)
            .collect();
        roots.sort_by(|&a, &b| {
            self.slots[b as usize]
                .priority
                .cmp(&self.slots[a as usize].priority)
                .then(a.cmp(&b))
        });

        let mut needed: HashSet<NodeId> = HashSet::new();
        let mut cost = Cost::default();
        self.shed.clear();
        for r in roots {
            let extra: Vec<NodeId> = self
                .closure(r)
                .into_iter()
                .filter(|i| !needed.contains(i))
                .collect();
            let extra_cost = extra.iter().fold(Cost::default(), |c, &i| {
                c.plus(self.slots[i as usize].contract.cost)
            });
            let total = cost.plus(extra_cost);
            if self.budget.is_none_or(|b| total.fits(&b)) {
                needed.extend(extra);
                cost = total;
            } else {
                self.shed.push(r);
            }
        }
        self.active_cost = cost;
        for (i, s) in self.slots.iter_mut().enumerate() {
            s.state = if needed.contains(&(i as NodeId)) {
                State::Active
            } else {
                State::Hibernated
            };
        }
    }

    /// Un tick en un solo hilo.
    pub fn tick(&mut self) {
        self.run(1);
    }

    /// Un tick usando hasta `threads` hilos: los nodos de cada capa de profundidad corren en paralelo.
    /// El resultado es idéntico al de `tick`, porque los nodos de una capa son independientes.
    pub fn tick_parallel(&mut self, threads: usize) {
        self.run(threads.max(1));
    }

    fn gather(&self, i: usize) -> NodeIo {
        let mut io = NodeIo::default();
        for l in &self.slots[i].links {
            if let Some(v) = self.slots[l.from as usize].out.get(l.output) {
                io.inputs.push((l.input, v.clone()));
            }
        }
        io
    }

    fn run(&mut self, threads: usize) {
        self.resolve();
        let max_depth = self.slots.iter().map(|s| s.depth).max().unwrap_or(0);
        let bad = AtomicU64::new(0);
        for d in 0..=max_depth {
            let idx: Vec<usize> = (0..self.slots.len())
                .filter(|&i| self.slots[i].state == State::Active && self.slots[i].depth == d)
                .collect();
            if idx.is_empty() {
                continue;
            }
            let ios: Vec<NodeIo> = idx.iter().map(|&i| self.gather(i)).collect();
            let mut work: Vec<(&mut Slot, NodeIo)> = self
                .slots
                .iter_mut()
                .enumerate()
                .filter(|(i, _)| idx.binary_search(i).is_ok())
                .map(|(_, s)| s)
                .zip(ios)
                .collect();
            if threads <= 1 || work.len() <= 1 {
                for (s, io) in work.iter_mut() {
                    bad.fetch_add(run_slot(s, io), Ordering::Relaxed);
                }
            } else {
                let chunk = work.len().div_ceil(threads);
                std::thread::scope(|sc| {
                    for part in work.chunks_mut(chunk) {
                        let bad = &bad;
                        sc.spawn(move || {
                            for (s, io) in part.iter_mut() {
                                bad.fetch_add(run_slot(s, io), Ordering::Relaxed);
                            }
                        });
                    }
                });
            }
        }
        self.violations += bad.load(Ordering::Relaxed);
    }

    /// Reemplaza un nodo en caliente conservando su id, sus conexiones y su estado.
    ///
    /// Compatible si: mismo nombre y versión mayor, versión menor igual o mayor, conserva todas
    /// las salidas, no exige entradas nuevas, mantiene las entradas ya conectadas y no pide
    /// permisos nuevos (salvo `allow_new_caps`).
    pub fn replace(
        &mut self,
        id: NodeId,
        mut new: Box<dyn Node>,
        allow_new_caps: bool,
    ) -> Result<(), GraphError> {
        let slot = self
            .slots
            .get(id as usize)
            .ok_or(GraphError::UnknownNode(id))?;
        let old = &slot.contract;
        let nc = new.contract();

        if nc.name != old.name {
            return Err(GraphError::IncompatibleContract("nombre distinto"));
        }
        if nc.version.0 != old.version.0 {
            return Err(GraphError::IncompatibleContract("versión mayor distinta"));
        }
        if nc.version.1 < old.version.1 {
            return Err(GraphError::IncompatibleContract("versión menor retrocede"));
        }
        if old.outputs.iter().any(|o| !nc.outputs.contains(o)) {
            return Err(GraphError::IncompatibleContract(
                "falta o cambió una salida",
            ));
        }
        if nc.inputs.iter().any(|i| !old.inputs.contains(i)) {
            return Err(GraphError::IncompatibleContract("exige una entrada nueva"));
        }
        if slot
            .links
            .iter()
            .any(|l| !nc.inputs.iter().any(|i| i.name == l.input))
        {
            return Err(GraphError::IncompatibleContract(
                "falta una entrada conectada",
            ));
        }
        if !allow_new_caps {
            let extra: Vec<&'static str> = nc
                .capabilities
                .iter()
                .copied()
                .filter(|c| !old.capabilities.contains(c))
                .collect();
            if !extra.is_empty() {
                return Err(GraphError::CapabilityEscalation(extra));
            }
        }

        let state = slot.node.export_state();
        new.import_state(&state);
        let slot = &mut self.slots[id as usize];
        slot.node = new;
        slot.contract = nc;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    pub fn state(&self, id: NodeId) -> State {
        self.slots[id as usize].state
    }
    pub fn ticks(&self, id: NodeId) -> u64 {
        self.slots[id as usize].ticks
    }
    pub fn depth(&self, id: NodeId) -> u32 {
        self.slots[id as usize].depth
    }
    pub fn contract(&self, id: NodeId) -> &Contract {
        &self.slots[id as usize].contract
    }
    /// Última salida publicada por un nodo.
    pub fn output(&self, id: NodeId, port: &str) -> Option<&Value> {
        self.slots[id as usize].out.get(port)
    }
    /// Costo total de los nodos activos tras el último `resolve`.
    pub fn active_cost(&self) -> Cost {
        self.active_cost
    }
    /// Raíces demandadas que se descartaron por no caber en el presupuesto.
    pub fn shed(&self) -> &[NodeId] {
        &self.shed
    }
    /// Salidas que un nodo publicó sin declararlas en su contrato o con tipo equivocado (se descartan).
    pub fn contract_violations(&self) -> u64 {
        self.violations
    }

    /// Permisos pedidos por cada nodo, para auditar o aplicar sandbox al cargar.
    pub fn capability_report(&self) -> HashMap<&'static str, Vec<&'static str>> {
        self.slots
            .iter()
            .map(|s| (s.contract.name, s.contract.capabilities.clone()))
            .collect()
    }

    /// Identificador de contenido de un módulo (hash calculado por la librería C).
    pub fn module_id(bytes: &[u8]) -> u64 {
        tsofgw_sys::hash64(bytes)
    }

    /// Exporta el grafo en formato Graphviz: verde = activo, gris = hibernado, rojo = descartado por presupuesto.
    pub fn to_dot(&self) -> String {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut out = String::from("digraph tsofgw {\n  rankdir=LR;\n  node [shape=box, style=filled, fontname=\"Helvetica\"];\n");
        for (i, s) in self.slots.iter().enumerate() {
            let color = if self.shed.contains(&(i as NodeId)) {
                "#f4b6b6"
            } else if s.state == State::Active {
                "#b7e4c7"
            } else {
                "#dddddd"
            };
            let c = &s.contract;
            out.push_str(&format!(
                "  n{i} [label=\"{} v{}.{}\\ncpu {}us gpu {}us mem {}KiB\", fillcolor=\"{color}\"];\n",
                esc(c.name),
                c.version.0,
                c.version.1,
                c.cost.cpu_us,
                c.cost.gpu_us,
                c.cost.mem_kb
            ));
        }
        for (i, s) in self.slots.iter().enumerate() {
            for l in &s.links {
                out.push_str(&format!(
                    "  n{} -> n{i} [label=\"{}→{}\"];\n",
                    l.from,
                    esc(l.output),
                    esc(l.input)
                ));
            }
        }
        out.push_str("}\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nodo de prueba con un contador como estado.
    struct T {
        c: Contract,
        count: u32,
    }
    impl Node for T {
        fn contract(&self) -> Contract {
            self.c.clone()
        }
        fn tick(&mut self, _io: &mut NodeIo) {
            self.count += 1;
        }
        fn export_state(&self) -> Vec<u8> {
            self.count.to_le_bytes().to_vec()
        }
        fn import_state(&mut self, s: &[u8]) {
            if s.len() == 4 {
                self.count = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            }
        }
    }
    fn node(c: Contract) -> Box<dyn Node> {
        Box::new(T { c, count: 0 })
    }
    fn cost(cpu: u32) -> Cost {
        Cost {
            cpu_us: cpu,
            gpu_us: 0,
            mem_kb: 0,
        }
    }
    fn link(from: NodeId, output: &'static str, input: &'static str) -> Link {
        Link {
            from,
            output,
            input,
        }
    }

    #[test]
    fn apagado_selectivo() {
        let mut g = Graph::new();
        let fisica = g
            .add(
                node(Contract::new("fisica", (1, 0)).output("pos", DataType::Transform)),
                &[],
            )
            .unwrap();
        let red = g
            .add(node(Contract::new("red", (1, 0)).cap("net")), &[])
            .unwrap();
        let render = g
            .add(
                node(Contract::new("render", (1, 0)).input("pos", DataType::Transform)),
                &[link(fisica, "pos", "pos")],
            )
            .unwrap();
        let audio = g.add(node(Contract::new("audio", (1, 0))), &[]).unwrap();

        g.set_demanded(render, true);
        g.tick();
        assert_eq!(g.state(render), State::Active);
        assert_eq!(g.state(fisica), State::Active);
        assert_eq!(g.state(red), State::Hibernated);
        assert_eq!(g.state(audio), State::Hibernated);
        assert_eq!(g.ticks(red), 0);

        g.set_demanded(render, false);
        g.tick();
        assert_eq!(g.state(fisica), State::Hibernated);
        assert_eq!(g.ticks(fisica), 1);
    }

    #[test]
    fn validacion_de_conexiones() {
        let mut g = Graph::new();
        let a = g
            .add(
                node(Contract::new("a", (1, 0)).output("x", DataType::Float)),
                &[],
            )
            .unwrap();
        let e = g.add(
            node(Contract::new("b", (1, 0)).input("x", DataType::Int)),
            &[link(a, "x", "x")],
        );
        assert_eq!(
            e.err(),
            Some(GraphError::TypeMismatch {
                output: DataType::Float,
                input: DataType::Int
            })
        );
        let e = g.add(
            node(Contract::new("b", (1, 0)).input("x", DataType::Float)),
            &[link(a, "nada", "x")],
        );
        assert_eq!(
            e.err(),
            Some(GraphError::UnknownPort {
                node: a,
                port: "nada"
            })
        );
        let e = g.add(
            node(Contract::new("b", (1, 0)).input("x", DataType::Float)),
            &[link(9, "x", "x")],
        );
        assert_eq!(e.err(), Some(GraphError::UnknownNode(9)));
        let e = g.add(
            node(Contract::new("b", (1, 0)).input("x", DataType::Float)),
            &[link(a, "x", "x"), link(a, "x", "x")],
        );
        assert_eq!(e.err(), Some(GraphError::DuplicateInput("x")));
    }

    #[test]
    fn presupuesto_descarta_menor_prioridad() {
        let mut g = Graph::new();
        let base = g
            .add(
                node(
                    Contract::new("base", (1, 0))
                        .output("o", DataType::Unit)
                        .cost(cost(100)),
                ),
                &[],
            )
            .unwrap();
        let vr = g
            .add(
                node(
                    Contract::new("vr", (1, 0))
                        .input("i", DataType::Unit)
                        .cost(cost(300)),
                ),
                &[link(base, "o", "i")],
            )
            .unwrap();
        let hud = g
            .add(node(Contract::new("hud", (1, 0)).cost(cost(500))), &[])
            .unwrap();
        g.set_demanded(vr, true);
        g.set_demanded(hud, true);
        g.set_priority(vr, 10);
        g.set_priority(hud, 1);

        g.set_budget(Some(cost(1000)));
        g.resolve();
        assert!(g.shed().is_empty());
        assert_eq!(g.active_cost().cpu_us, 900);

        g.set_budget(Some(cost(600)));
        g.resolve();
        assert_eq!(g.shed(), &[hud]);
        assert_eq!(g.state(vr), State::Active);
        assert_eq!(g.state(base), State::Active);
        assert_eq!(g.state(hud), State::Hibernated);
        assert_eq!(g.active_cost().cpu_us, 400);
    }

    #[test]
    fn hot_swap_conserva_conexiones() {
        let mut g = Graph::new();
        let fisica = g
            .add(
                node(Contract::new("fisica", (1, 0)).output("pos", DataType::Transform)),
                &[],
            )
            .unwrap();
        let render = g
            .add(
                node(Contract::new("render", (1, 0)).input("pos", DataType::Transform)),
                &[link(fisica, "pos", "pos")],
            )
            .unwrap();
        g.set_demanded(render, true);
        g.tick();
        g.tick();
        let nueva = node(Contract::new("fisica", (1, 1)).output("pos", DataType::Transform));
        g.replace(fisica, nueva, false).unwrap();
        assert_eq!(g.contract(fisica).version, (1, 1));
        g.tick();
        assert_eq!(g.state(render), State::Active);
        assert_eq!(g.ticks(fisica), 3);
    }

    #[test]
    fn hot_swap_conserva_estado_interno() {
        let mut g = Graph::new();
        let a = g.add(node(Contract::new("a", (1, 0))), &[]).unwrap();
        g.set_demanded(a, true);
        g.tick();
        g.tick();
        g.tick();
        g.replace(a, node(Contract::new("a", (1, 1))), false)
            .unwrap();
        assert_eq!(
            g.slots[a as usize].node.export_state(),
            3u32.to_le_bytes().to_vec()
        );
        g.tick();
        assert_eq!(
            g.slots[a as usize].node.export_state(),
            4u32.to_le_bytes().to_vec()
        );
    }

    #[test]
    fn hot_swap_rechaza_incompatibles() {
        let mut g = Graph::new();
        let a = g
            .add(
                node(
                    Contract::new("a", (1, 2))
                        .output("o", DataType::Float)
                        .cap("disk"),
                ),
                &[],
            )
            .unwrap();
        let b = g
            .add(
                node(Contract::new("b", (1, 0)).input("i", DataType::Float)),
                &[link(a, "o", "i")],
            )
            .unwrap();
        let base = || {
            Contract::new("a", (1, 2))
                .output("o", DataType::Float)
                .cap("disk")
        };

        let mut c = base();
        c.name = "otro";
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::IncompatibleContract("nombre distinto"))
        );
        let mut c = base();
        c.version = (2, 0);
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::IncompatibleContract("versión mayor distinta"))
        );
        let mut c = base();
        c.version = (1, 1);
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::IncompatibleContract("versión menor retrocede"))
        );
        let c = Contract::new("a", (1, 2)).cap("disk");
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::IncompatibleContract(
                "falta o cambió una salida"
            ))
        );
        let c = base().input("nueva", DataType::Int);
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::IncompatibleContract("exige una entrada nueva"))
        );
        let c = Contract::new("b", (1, 0));
        assert_eq!(
            g.replace(b, node(c), false),
            Err(GraphError::IncompatibleContract(
                "falta una entrada conectada"
            ))
        );
        let c = base().cap("net");
        assert_eq!(
            g.replace(a, node(c), false),
            Err(GraphError::CapabilityEscalation(vec!["net"]))
        );
        let c = base().cap("net");
        assert_eq!(g.replace(a, node(c), true), Ok(()));
        assert_eq!(g.capability_report()["a"], vec!["disk", "net"]);
    }

    #[test]
    fn flujo_de_datos_entre_nodos() {
        let mut g = Graph::new();
        let mut n = 0.0;
        let fuente = g
            .add(
                fn_node(
                    Contract::new("fuente", (1, 0)).output("x", DataType::Float),
                    move |io| {
                        n += 1.0;
                        io.set_output("x", Value::Float(n));
                    },
                ),
                &[],
            )
            .unwrap();
        let doble = g
            .add(
                fn_node(
                    Contract::new("doble", (1, 0))
                        .input("x", DataType::Float)
                        .output("y", DataType::Float),
                    |io| {
                        let x = io.float("x").unwrap_or(0.0);
                        io.set_output("y", Value::Float(x * 2.0));
                    },
                ),
                &[link(fuente, "x", "x")],
            )
            .unwrap();
        g.set_demanded(doble, true);
        g.tick();
        g.tick();
        g.tick();
        assert_eq!(g.output(doble, "y"), Some(&Value::Float(6.0)));
        assert_eq!(g.depth(fuente), 0);
        assert_eq!(g.depth(doble), 1);
        assert_eq!(g.contract_violations(), 0);
    }

    #[test]
    fn salida_invalida_se_descarta_y_se_cuenta() {
        let mut g = Graph::new();
        let a = g
            .add(
                fn_node(
                    Contract::new("a", (1, 0)).output("ok", DataType::Int),
                    |io| {
                        io.set_output("ok", Value::Float(1.0)); // tipo equivocado
                        io.set_output("fantasma", Value::Int(1)); // no declarada
                    },
                ),
                &[],
            )
            .unwrap();
        g.set_demanded(a, true);
        g.tick();
        assert_eq!(g.contract_violations(), 2);
        assert_eq!(g.output(a, "ok"), None);
    }

    #[test]
    fn paralelo_igual_a_secuencial() {
        fn construir() -> (Graph, NodeId) {
            let mut g = Graph::new();
            let mut ids = vec![];
            for k in 0..16i64 {
                let id = g
                    .add(
                        fn_node(
                            Contract::new("gen", (1, 0)).output("v", DataType::Int),
                            move |io| {
                                let mut acc = 0i64;
                                for i in 0..2000 {
                                    acc = acc.wrapping_add(i * (k + 1));
                                }
                                io.set_output("v", Value::Int(acc));
                            },
                        ),
                        &[],
                    )
                    .unwrap();
                ids.push(id);
            }
            // Un sumidero con 16 entradas requiere nombres estáticos distintos.
            const NOMBRES: [&str; 16] = [
                "i0", "i1", "i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "i10", "i11", "i12",
                "i13", "i14", "i15",
            ];
            let mut c = Contract::new("suma", (1, 0)).output("total", DataType::Int);
            let mut links = vec![];
            for (k, id) in ids.iter().enumerate() {
                c = c.input(NOMBRES[k], DataType::Int);
                links.push(link(*id, "v", NOMBRES[k]));
            }
            let suma = g
                .add(
                    fn_node(c, |io| {
                        let t: i64 = NOMBRES.iter().filter_map(|n| io.int(n)).sum();
                        io.set_output("total", Value::Int(t));
                    }),
                    &links,
                )
                .unwrap();
            g.set_demanded(suma, true);
            (g, suma)
        }
        let (mut a, sa) = construir();
        let (mut b, sb) = construir();
        a.tick();
        b.tick_parallel(4);
        assert_eq!(a.output(sa, "total"), b.output(sb, "total"));
        assert!(matches!(a.output(sa, "total"), Some(Value::Int(t)) if *t != 0));
        assert_eq!(a.depth(sa), 1);
    }

    #[test]
    fn exporta_dot() {
        let mut g = Graph::new();
        let a = g
            .add(
                node(
                    Contract::new("a", (1, 0))
                        .output("o", DataType::Unit)
                        .cost(cost(5)),
                ),
                &[],
            )
            .unwrap();
        let b = g
            .add(
                node(Contract::new("b", (1, 0)).input("i", DataType::Unit)),
                &[link(a, "o", "i")],
            )
            .unwrap();
        g.set_demanded(b, true);
        g.resolve();
        let dot = g.to_dot();
        assert!(dot.starts_with("digraph tsofgw {"));
        assert!(dot.contains("n0 -> n1"));
        assert!(dot.contains("#b7e4c7")); // activos
        assert!(dot.contains("cpu 5us"));
    }

    #[test]
    fn reporte_de_capacidades() {
        let mut g = Graph::new();
        g.add(node(Contract::new("red", (1, 0)).cap("net")), &[])
            .unwrap();
        assert_eq!(g.capability_report()["red"], vec!["net"]);
    }

    #[test]
    fn id_de_modulo_usa_c() {
        assert_ne!(Graph::module_id(b"a"), Graph::module_id(b"b"));
    }
}
