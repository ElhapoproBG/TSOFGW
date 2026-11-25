//! Capa de juego en línea de TSOFGW.
//!
//! * [`GameServer`]: servidor autoritativo con regiones activas (apagado selectivo de NPC), interés por
//!   jugador, snapshots delta, límites contra clientes tramposos y transporte autenticado.
//! * [`GameClient`]: predicción de movimiento, reconciliación con el servidor e interpolación del resto.
//! * [`Bot`] y [`scenario`]: jugadores automáticos y un banco de pruebas determinista (red simulada con
//!   pérdida, latencia y desorden) que produce un informe medido.
//!
//! Es un ejemplo funcional de cómo se compone un juego sobre las capas de TSOFGW, no un juego: el
//! movimiento y el mundo son deliberadamente mínimos. Los desarrolladores sustituirían `MoveSim`, los
//! NPC y los mensajes por los suyos.

mod bot;
pub mod bus;
mod client;
pub mod cluster_scenario;
pub mod cluster_udp;
pub mod par;
pub mod partition;
pub mod protocol;
mod replicate;
pub mod scenario;
mod server;
pub mod sim;
mod states;
pub mod udp_bus;

pub use bot::Bot;
pub use client::{ClientStats, Connector, GameClient, NewConnection, Phase};
pub use replicate::{default_lod, LodBand};
pub use server::{
    ClusterConfig, ClusterStats, GameConfig, GameServer, GameStats, KIND_NPC, KIND_PLAYER, NPC_BASE,
};
pub use udp_bus::{UdpBus, UdpBusStats};
