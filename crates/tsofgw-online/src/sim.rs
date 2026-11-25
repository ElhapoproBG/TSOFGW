//! Movimiento de jugadores: determinista y en enteros, idéntico en cliente (predicción) y servidor.
//!
//! El cliente nunca envía posiciones, solo **intenciones** (una dirección por tick). La velocidad máxima
//! la impone la propia función `step`, así que no hay forma de pedir moverse más rápido: un cliente
//! tramposo solo puede intentar enviar más inputs por tick, y eso lo limita el servidor con créditos.

use tsofgw_net::Sim;

/// Desplazamiento máximo por tick, en 1/64 de unidad (24/64 = 0,375 u; a 20 Hz son 7,5 u/s).
pub const STEP_Q: i32 = 24;
/// Mitad del lado del mundo en X y Z (512 unidades), en 1/64 de unidad.
pub const HALF_EXTENT_Q: i32 = 512 * 64;
/// Altura máxima (64 unidades), en 1/64 de unidad.
pub const MAX_Y_Q: i32 = 64 * 64;
const DIR_MAX: i32 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Input {
    pub dir: [i8; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayerState {
    /// Posición en 1/64 de unidad (la misma cuantización que usan los snapshots).
    pub pos: [i32; 3],
}

pub struct MoveSim;

/// Desplazamiento de un tick para una dirección: la dirección se normaliza si su longitud supera 127
/// y se escala a `STEP_Q`. El denominador se redondea hacia **arriba**, de modo que la longitud real
/// nunca supera `STEP_Q` (dividir primero y escalar después dejaba pasar un 0,2 % más en diagonal).
fn displacement(dir: [i8; 3]) -> [i32; 3] {
    let v = [dir[0] as i32, dir[1] as i32, dir[2] as i32];
    let mag2 = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) as u32;
    let floor = mag2.isqrt();
    let ceil = if floor * floor < mag2 {
        floor + 1
    } else {
        floor
    };
    let denom = (ceil as i32).max(DIR_MAX);
    [
        v[0] * STEP_Q / denom,
        v[1] * STEP_Q / denom,
        v[2] * STEP_Q / denom,
    ]
}

impl Sim for MoveSim {
    type State = PlayerState;
    type Input = Input;

    fn step(state: &PlayerState, input: &Input) -> PlayerState {
        let d = displacement(input.dir);
        let mut p = state.pos;
        for i in 0..3 {
            p[i] += d[i];
        }
        p[0] = p[0].clamp(-HALF_EXTENT_Q, HALF_EXTENT_Q);
        p[2] = p[2].clamp(-HALF_EXTENT_Q, HALF_EXTENT_Q);
        p[1] = p[1].clamp(0, MAX_Y_Q);
        PlayerState { pos: p }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist2(a: [i32; 3], b: [i32; 3]) -> i64 {
        (0..3).map(|i| (a[i] as i64 - b[i] as i64).pow(2)).sum()
    }

    #[test]
    fn nunca_se_mueve_mas_de_un_paso_por_input() {
        let start = PlayerState { pos: [0, 1000, 0] };
        // Incluye -128, que no tiene opuesto en i8, y todas las combinaciones de esquinas.
        let vals: [i8; 13] = [-128, -127, -100, -90, -64, -1, 0, 1, 33, 64, 90, 126, 127];
        for &x in &vals {
            for &y in &vals {
                for &z in &vals {
                    let s = MoveSim::step(&start, &Input { dir: [x, y, z] });
                    assert!(
                        dist2(s.pos, start.pos) <= (STEP_Q as i64).pow(2),
                        "dir {x},{y},{z} avanzó demasiado"
                    );
                }
            }
        }
    }

    #[test]
    fn velocidad_maxima_exacta_en_los_ejes() {
        let s = MoveSim::step(&PlayerState::default(), &Input { dir: [127, 0, 0] });
        assert_eq!(s.pos, [STEP_Q, 0, 0]);
        let s = MoveSim::step(
            &PlayerState { pos: [0, 100, 0] },
            &Input { dir: [0, -127, 0] },
        );
        assert_eq!(s.pos, [0, 100 - STEP_Q, 0]);
    }

    #[test]
    fn la_diagonal_no_es_mas_rapida() {
        let eje = MoveSim::step(&PlayerState::default(), &Input { dir: [127, 0, 0] });
        let diag = MoveSim::step(&PlayerState::default(), &Input { dir: [127, 0, 127] });
        assert!(dist2(diag.pos, [0, 0, 0]) <= dist2(eje.pos, [0, 0, 0]));
    }

    #[test]
    fn respeta_los_limites_del_mundo() {
        let edge = PlayerState {
            pos: [HALF_EXTENT_Q, 0, -HALF_EXTENT_Q],
        };
        let s = MoveSim::step(
            &edge,
            &Input {
                dir: [127, -127, -127],
            },
        );
        assert_eq!(s.pos, [HALF_EXTENT_Q, 0, -HALF_EXTENT_Q]);
        let top = PlayerState {
            pos: [0, MAX_Y_Q, 0],
        };
        assert_eq!(
            MoveSim::step(&top, &Input { dir: [0, 127, 0] }).pos[1],
            MAX_Y_Q
        );
    }

    #[test]
    fn es_determinista() {
        let mut a = PlayerState::default();
        let mut b = PlayerState::default();
        for i in 0..1000i32 {
            let inp = Input {
                dir: [(i * 7) as i8, (i * 3) as i8, (i * 11) as i8],
            };
            a = MoveSim::step(&a, &inp);
            b = MoveSim::step(&b, &inp);
        }
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod exhaustivo {
    use super::*;

    /// Recorre las 16 777 216 direcciones posibles: ninguna avanza más de un paso.
    #[test]
    fn ninguna_direccion_supera_el_paso_maximo() {
        let mut worst = 0i64;
        for x in -128i32..=127 {
            for y in -128i32..=127 {
                for z in -128i32..=127 {
                    let d = displacement([x as i8, y as i8, z as i8]);
                    let l2 = (d[0] as i64).pow(2) + (d[1] as i64).pow(2) + (d[2] as i64).pow(2);
                    worst = worst.max(l2);
                }
            }
        }
        assert!(
            worst <= (STEP_Q as i64).pow(2),
            "peor caso {worst} > {}",
            STEP_Q * STEP_Q
        );
        // Y el máximo se alcanza (no es un límite flojo).
        assert_eq!(worst, (STEP_Q as i64).pow(2));
    }
}
