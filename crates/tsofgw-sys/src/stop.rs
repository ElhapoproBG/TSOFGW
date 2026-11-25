//! Parada ordenada del proceso.
//!
//! Un servidor con almacén durable quiere cerrarlo bien (guardar, sincronizar, compactar y avisar a
//! los clientes) cuando el operador lo detiene, en lugar de morir de golpe. Este módulo levanta una
//! bandera cuando llega la señal habitual de parada —`SIGINT` (Ctrl-C) o `SIGTERM` en Unix; Ctrl-C o
//! Ctrl-Pausa en Windows— y el bucle del servidor la consulta con [`requested`].
//!
//! No hace nada más que levantar una bandera atómica dentro del manejador (lo único que es seguro
//! hacer ahí). Una segunda señal no la baja ni mata el proceso: si el cierre se atasca, el operador
//! tiene `SIGKILL` / el administrador de tareas, y el almacén sobrevive a eso por diseño.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// ¿Ha pedido alguien parar (o se llamó a [`request`])?
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Pide la parada como si hubiera llegado la señal (para pruebas y para quien quiera parar desde otro
/// hilo).
pub fn request() {
    STOP.store(true, Ordering::SeqCst);
}

/// Baja la bandera (solo para pruebas).
#[doc(hidden)]
pub fn reset() {
    STOP.store(false, Ordering::SeqCst);
}

/// Instala el manejador de las señales de parada. `true` si quedó instalado; `false` si esta
/// plataforma no lo soporta (la bandera solo se levantará con [`request`]).
pub fn install() -> bool {
    imp::install()
}

#[cfg(unix)]
mod imp {
    use super::STOP;
    use std::sync::atomic::Ordering;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" {
        // `void (*signal(int, void (*)(int)))(int)`: devuelve el manejador anterior, o SIG_ERR (-1).
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    extern "C" fn on_signal(_signum: i32) {
        STOP.store(true, Ordering::SeqCst);
    }

    pub fn install() -> bool {
        // SAFETY: `on_signal` solo hace una escritura atómica, que es segura dentro de un manejador de
        // señales; las constantes son las de POSIX y la firma de `signal` la de la biblioteca de C.
        unsafe {
            signal(SIGINT, on_signal) != usize::MAX && signal(SIGTERM, on_signal) != usize::MAX
        }
    }

    /// Se envía a sí mismo una señal (para probar el manejador de verdad).
    #[cfg(test)]
    pub fn raise_term() {
        extern "C" {
            fn raise(signum: i32) -> i32;
        }
        // SAFETY: `raise` con una señal válida; el manejador ya está instalado.
        assert_eq!(unsafe { raise(SIGTERM) }, 0);
    }
}

#[cfg(windows)]
mod imp {
    use super::STOP;
    use std::sync::atomic::Ordering;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    type Handler = unsafe extern "system" fn(ctrl_type: u32) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }

    unsafe extern "system" fn on_ctrl(ctrl_type: u32) -> i32 {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                STOP.store(true, Ordering::SeqCst);
                1 // TRUE: atendido, no se aplica el manejador por defecto (que mataría el proceso)
            }
            // Cierre de la consola, cierre de sesión o apagado: el sistema acaba con el proceso en
            // cuanto el manejador devuelve, así que se deja el comportamiento por defecto.
            _ => 0,
        }
    }

    pub fn install() -> bool {
        // SAFETY: `on_ctrl` solo hace una escritura atómica; `SetConsoleCtrlHandler` es de kernel32.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) != 0 }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn install() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_bandera_se_levanta_y_se_baja() {
        reset();
        assert!(!requested());
        request();
        assert!(requested());
        reset();
        assert!(!requested());
    }

    #[cfg(unix)]
    #[test]
    fn una_senal_de_parada_levanta_la_bandera() {
        assert!(install());
        reset();
        assert!(!requested());
        imp::raise_term();
        // La señal se entrega al propio hilo antes de que `raise` devuelva, pero se espera un poco por
        // si el sistema la entregara a otro hilo.
        for _ in 0..200 {
            if requested() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(requested());
        reset();
    }
}
