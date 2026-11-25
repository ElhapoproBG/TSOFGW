//! Matemática de cámara compartida por todos los backends (software, wgpu, OpenXR…).
//!
//! Convenciones: sistema de la mano derecha, +Y arriba, la cámara mira hacia -Z,
//! profundidad en `[0, 1]` (como wgpu, Vulkan, D3D y Metal), cuaterniones `[x, y, z, w]`.
//! `Mat4` es *row-major* (`m[fila][columna]`) y multiplica vectores columna.

use crate::Eye;
use tsofgw_core::Transform;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat4(pub [[f32; 4]; 4]);

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]);

    /// `self * o` (aplica primero `o`, luego `self`).
    pub fn mul(&self, o: &Mat4) -> Mat4 {
        let mut r = [[0.0f32; 4]; 4];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..4).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        Mat4(r)
    }

    pub fn mul_vec4(&self, v: [f32; 4]) -> [f32; 4] {
        let mut r = [0.0f32; 4];
        for (i, cell) in r.iter_mut().enumerate() {
            *cell = (0..4).map(|k| self.0[i][k] * v[k]).sum();
        }
        r
    }

    /// Orden por columnas, como espera WGSL/GLSL/HLSL para subir la matriz a la GPU.
    pub fn to_cols_array(&self) -> [f32; 16] {
        let mut a = [0.0f32; 16];
        for c in 0..4 {
            for r in 0..4 {
                a[c * 4 + r] = self.0[r][c];
            }
        }
        a
    }
}

pub fn quat_normalize(q: [f32; 4]) -> [f32; 4] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n < 1e-12 {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
    }
}

pub fn quat_from_axis_angle(axis: [f32; 3], angle: f32) -> [f32; 4] {
    let n = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
    if n < 1e-12 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let (s, c) = (angle * 0.5).sin_cos();
    [axis[0] / n * s, axis[1] / n * s, axis[2] / n * s, c]
}

/// Producto de cuaterniones: aplica primero `b`, luego `a`.
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

pub fn rotation_matrix(q: [f32; 4]) -> [[f32; 3]; 3] {
    let [x, y, z, w] = quat_normalize(q);
    let (xx, yy, zz, xy, xz, yz, wx, wy, wz) = (
        x * x,
        y * y,
        z * z,
        x * y,
        x * z,
        y * z,
        w * x,
        w * y,
        w * z,
    );
    [
        [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
        [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
        [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
    ]
}

/// Del espacio local del objeto al mundo.
pub fn model_matrix(t: &Transform) -> Mat4 {
    let r = rotation_matrix(t.rot);
    Mat4([
        [r[0][0], r[0][1], r[0][2], t.pos[0]],
        [r[1][0], r[1][1], r[1][2], t.pos[1]],
        [r[2][0], r[2][1], r[2][2], t.pos[2]],
        [0.0, 0.0, 0.0, 1.0],
    ])
}

/// Del mundo al espacio de la cámara: la inversa de `model_matrix` (movimiento rígido).
pub fn view_matrix(t: &Transform) -> Mat4 {
    let r = rotation_matrix(t.rot);
    // R transpuesta y traslación -Rᵀ·p
    let rt = [
        [r[0][0], r[1][0], r[2][0]],
        [r[0][1], r[1][1], r[2][1]],
        [r[0][2], r[1][2], r[2][2]],
    ];
    let p = t.pos;
    let tr = [
        -(rt[0][0] * p[0] + rt[0][1] * p[1] + rt[0][2] * p[2]),
        -(rt[1][0] * p[0] + rt[1][1] * p[1] + rt[1][2] * p[2]),
        -(rt[2][0] * p[0] + rt[2][1] * p[1] + rt[2][2] * p[2]),
    ];
    Mat4([
        [rt[0][0], rt[0][1], rt[0][2], tr[0]],
        [rt[1][0], rt[1][1], rt[1][2], tr[1]],
        [rt[2][0], rt[2][1], rt[2][2], tr[2]],
        [0.0, 0.0, 0.0, 1.0],
    ])
}

/// Proyección en perspectiva con campo de visión asimétrico (el formato de OpenXR).
/// `fov` son ángulos en radianes: `[izquierda, derecha, arriba, abajo]`, con izquierda y abajo negativos.
/// Profundidad de clip en `[0, 1]`: el plano cercano da 0 y el lejano 1.
pub fn perspective(fov: [f32; 4], near: f32, far: f32) -> Mat4 {
    let (tl, tr, tu, td) = (fov[0].tan(), fov[1].tan(), fov[2].tan(), fov[3].tan());
    let (w, h) = (tr - tl, tu - td);
    Mat4([
        [2.0 / w, 0.0, (tr + tl) / w, 0.0],
        [0.0, 2.0 / h, (tu + td) / h, 0.0],
        [0.0, 0.0, -far / (far - near), -far * near / (far - near)],
        [0.0, 0.0, -1.0, 0.0],
    ])
}

/// Campo de visión simétrico a partir del ángulo vertical total y la relación de aspecto (ancho/alto).
pub fn symmetric_fov(vertical: f32, aspect: f32) -> [f32; 4] {
    let ty = (vertical * 0.5).tan();
    let ax = (ty * aspect).atan();
    let ay = (vertical * 0.5).abs();
    [-ax, ax, ay, -ay]
}

/// Matrices de vista y proyección de un ojo.
pub fn eye_matrices(eye: &Eye, near: f32, far: f32) -> (Mat4, Mat4) {
    (view_matrix(&eye.transform), perspective(eye.fov, near, far))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};

    fn cerca(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }
    fn cerca4(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| cerca(*x, y))
    }

    #[test]
    fn identidad_y_producto() {
        let m = model_matrix(&Transform {
            pos: [1.0, 2.0, 3.0],
            rot: quat_from_axis_angle([0.0, 1.0, 0.0], 0.7),
        });
        assert_eq!(m.mul(&Mat4::IDENTITY), m);
        assert_eq!(Mat4::IDENTITY.mul(&m), m);
        let v = m.mul_vec4([0.0, 0.0, 0.0, 1.0]);
        assert!(cerca4(v, [1.0, 2.0, 3.0, 1.0]));
    }

    #[test]
    fn rotacion_90_grados_en_y_lleva_x_a_menos_z() {
        let r = rotation_matrix(quat_from_axis_angle([0.0, 1.0, 0.0], FRAC_PI_2));
        let m = Mat4([
            [r[0][0], r[0][1], r[0][2], 0.0],
            [r[1][0], r[1][1], r[1][2], 0.0],
            [r[2][0], r[2][1], r[2][2], 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        assert!(cerca4(
            m.mul_vec4([1.0, 0.0, 0.0, 1.0]),
            [0.0, 0.0, -1.0, 1.0]
        ));
        assert!(cerca4(
            m.mul_vec4([0.0, 1.0, 0.0, 1.0]),
            [0.0, 1.0, 0.0, 1.0]
        ));
        // mismo resultado con cuaternión no normalizado
        let r2 = rotation_matrix([0.0, 2.0 * (FRAC_PI_4).sin(), 0.0, 2.0 * (FRAC_PI_4).cos()]);
        assert!(r
            .iter()
            .flatten()
            .zip(r2.iter().flatten())
            .all(|(a, b)| cerca(*a, *b)));
    }

    #[test]
    fn vista_es_la_inversa_del_modelo() {
        let t = Transform {
            pos: [3.0, -1.0, 2.0],
            rot: quat_mul(
                quat_from_axis_angle([1.0, 0.0, 0.0], 0.4),
                quat_from_axis_angle([0.0, 1.0, 0.0], -1.1),
            ),
        };
        let id = view_matrix(&t).mul(&model_matrix(&t));
        for (i, (fila, esperada)) in id.0.iter().zip(Mat4::IDENTITY.0).enumerate() {
            for (j, (obtenido, esperado)) in fila.iter().zip(esperada).enumerate() {
                assert!(cerca(*obtenido, esperado), "[{i}][{j}] = {obtenido}");
            }
        }
    }

    #[test]
    fn la_camara_mira_hacia_menos_z() {
        // cámara en el origen sin rotar: un punto en z = -5 queda a 5 unidades delante (z de vista = -5)
        let v = view_matrix(&Transform::default()).mul_vec4([0.0, 0.0, -5.0, 1.0]);
        assert!(cerca4(v, [0.0, 0.0, -5.0, 1.0]));
        // cámara movida a z = 2: el mismo punto queda a 7 unidades
        let cam = Transform {
            pos: [0.0, 0.0, 2.0],
            ..Transform::default()
        };
        assert!(cerca(
            view_matrix(&cam).mul_vec4([0.0, 0.0, -5.0, 1.0])[2],
            -7.0
        ));
    }

    #[test]
    fn perspectiva_profundidad_de_0_a_1() {
        let p = perspective(symmetric_fov(1.0, 1.0), 0.1, 100.0);
        let cerca_ndc = p.mul_vec4([0.0, 0.0, -0.1, 1.0]);
        assert!(cerca(cerca_ndc[2] / cerca_ndc[3], 0.0));
        let lejos = p.mul_vec4([0.0, 0.0, -100.0, 1.0]);
        assert!(cerca(lejos[2] / lejos[3], 1.0));
    }

    #[test]
    fn los_bordes_del_campo_de_vision_llegan_a_menos_uno_y_uno() {
        let fov = [-0.9f32, 0.7, 0.5, -0.3];
        let p = perspective(fov, 0.1, 50.0);
        let z = -4.0f32;
        let en = |ang: f32, eje: usize| {
            let mut v = [0.0, 0.0, z, 1.0];
            v[eje] = ang.tan() * -z;
            let c = p.mul_vec4(v);
            c[eje] / c[3]
        };
        assert!(cerca(en(fov[0], 0), -1.0));
        assert!(cerca(en(fov[1], 0), 1.0));
        assert!(cerca(en(fov[2], 1), 1.0));
        assert!(cerca(en(fov[3], 1), -1.0));
    }

    #[test]
    fn orden_por_columnas_para_la_gpu() {
        let m = Mat4([
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
        ]);
        let a = m.to_cols_array();
        assert_eq!(&a[0..4], &[1.0, 5.0, 9.0, 13.0]); // primera columna
        assert_eq!(&a[12..16], &[4.0, 8.0, 12.0, 16.0]); // última columna
    }

    #[test]
    fn cuaterniones_componen_en_el_orden_esperado() {
        let ry = quat_from_axis_angle([0.0, 1.0, 0.0], FRAC_PI_2);
        let rx = quat_from_axis_angle([1.0, 0.0, 0.0], FRAC_PI_2);
        // primero rx, luego ry
        let q = quat_mul(ry, rx);
        let r = rotation_matrix(q);
        // el eje Y (0,1,0): rx lo lleva a +Z, luego ry lleva +Z a +X
        let v = [r[0][1], r[1][1], r[2][1]];
        assert!(
            cerca(v[0], 1.0) && cerca(v[1], 0.0) && cerca(v[2], 0.0),
            "{v:?}"
        );
        assert_eq!(quat_from_axis_angle([0.0; 3], 1.0), [0.0, 0.0, 0.0, 1.0]);
    }
}
