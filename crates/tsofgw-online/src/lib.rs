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
mod client;
pub mod protocol;
pub mod scenario;
mod server;
pub mod sim;

pub use bot::Bot;
pub use client::{ClientStats, GameClient, Phase};
pub use server::{GameConfig, GameServer, GameStats, KIND_NPC, KIND_PLAYER, NPC_BASE};
