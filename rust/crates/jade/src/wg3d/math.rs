//! Column-major 4×4 matrix math for the 3D weight grid, ported verbatim from
//! `renderer/ml/weight-grid-gl.ts` (`mat4Perspective` / `mat4LookAt`) so the
//! Rust projection matches the original WebGL2 pipeline exactly.
//!
//! Storage is column-major (WebGL convention): element at row `r`, column `c`
//! lives at `m[c * 4 + r]`. Everything here is pure and unit-tested against
//! values computed by hand from the TS source.

pub type Vec3 = [f32; 3];

/// A 4×4 matrix in column-major order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat4(pub [f32; 16]);

impl Mat4 {
    pub fn zero() -> Self {
        Mat4([0.0; 16])
    }

    /// Perspective projection (`mat4Perspective`, weight-grid-gl.ts:18-33).
    /// `fov_y_deg` is the vertical field of view in degrees.
    pub fn perspective(fov_y_deg: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / (fov_y_deg * std::f32::consts::PI / 360.0).tan();
        let mut m = [0.0f32; 16];
        m[0] = f / aspect;
        m[5] = f;
        m[10] = (far + near) / (near - far);
        m[11] = -1.0;
        m[14] = (2.0 * far * near) / (near - far);
        Mat4(m)
    }

    /// Right-handed look-at view matrix (`mat4LookAt`, weight-grid-gl.ts:35-70).
    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Self {
        let mut zx = eye[0] - target[0];
        let mut zy = eye[1] - target[1];
        let mut zz = eye[2] - target[2];
        let mut len = (zx * zx + zy * zy + zz * zz).sqrt();
        if len == 0.0 {
            len = 1.0;
        }
        zx /= len;
        zy /= len;
        zz /= len;

        let mut xx = up[1] * zz - up[2] * zy;
        let mut xy = up[2] * zx - up[0] * zz;
        let mut xz = up[0] * zy - up[1] * zx;
        len = (xx * xx + xy * xy + xz * xz).sqrt();
        if len == 0.0 {
            len = 1.0;
        }
        xx /= len;
        xy /= len;
        xz /= len;

        let yx = zy * xz - zz * xy;
        let yy = zz * xx - zx * xz;
        let yz = zx * xy - zy * xx;

        let mut m = [0.0f32; 16];
        m[0] = xx;
        m[1] = yx;
        m[2] = zx;
        m[3] = 0.0;
        m[4] = xy;
        m[5] = yy;
        m[6] = zy;
        m[7] = 0.0;
        m[8] = xz;
        m[9] = yz;
        m[10] = zz;
        m[11] = 0.0;
        m[12] = -(xx * eye[0] + xy * eye[1] + xz * eye[2]);
        m[13] = -(yx * eye[0] + yy * eye[1] + yz * eye[2]);
        m[14] = -(zx * eye[0] + zy * eye[1] + zz * eye[2]);
        m[15] = 1.0;
        Mat4(m)
    }

    /// Matrix product `self * rhs` (column-major).
    pub fn mul(&self, rhs: &Mat4) -> Mat4 {
        let a = &self.0;
        let b = &rhs.0;
        let mut out = [0.0f32; 16];
        for c in 0..4 {
            for r in 0..4 {
                let mut s = 0.0;
                for k in 0..4 {
                    s += a[k * 4 + r] * b[c * 4 + k];
                }
                out[c * 4 + r] = s;
            }
        }
        Mat4(out)
    }

    /// Transform a homogeneous point `(x, y, z, 1)`; returns clip-space
    /// `(x, y, z, w)`.
    pub fn transform4(&self, x: f32, y: f32, z: f32) -> [f32; 4] {
        let m = &self.0;
        [
            m[0] * x + m[4] * y + m[8] * z + m[12],
            m[1] * x + m[5] * y + m[9] * z + m[13],
            m[2] * x + m[6] * y + m[10] * z + m[14],
            m[3] * x + m[7] * y + m[11] * z + m[15],
        ]
    }
}

/// Screen-space projection of a world point.
pub struct Projected {
    pub x: f32,
    pub y: f32,
    /// Clip-space `w` (monotonic with distance from the camera).
    pub depth: f32,
}

/// Project a world point through `proj * view` onto screen space of size
/// `w × h` (top-left origin). Returns `None` when the point is at/behind the
/// camera (`clip.w <= 0`).
pub fn project(mvp: &Mat4, p: Vec3, w: f32, h: f32) -> Option<Projected> {
    let c = mvp.transform4(p[0], p[1], p[2]);
    let cw = c[3];
    if cw <= 1e-6 {
        return None;
    }
    let ndc_x = c[0] / cw;
    let ndc_y = c[1] / cw;
    Some(Projected {
        x: (ndc_x * 0.5 + 0.5) * w,
        y: (1.0 - (ndc_y * 0.5 + 0.5)) * h,
        depth: cw,
    })
}

/// View-space depth of a world point: the z coordinate after the view
/// transform (camera looks down −z, so more-negative = farther). Used to sort
/// bars back-to-front for the painter's algorithm.
pub fn view_depth(view: &Mat4, p: Vec3) -> f32 {
    view.transform4(p[0], p[1], p[2])[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "expected {b}, got {a}");
    }

    #[test]
    fn perspective_matches_ts() {
        // fovY=50, aspect=1, near=0.1, far=4000 (the WeightGrid3D defaults).
        let m = Mat4::perspective(50.0, 1.0, 0.1, 4000.0).0;
        // f = 1/tan(50*PI/360) = 2.144507
        approx(m[0], 2.144507);
        approx(m[5], 2.144507);
        approx(m[10], (4000.1) / (0.1 - 4000.0)); // -1.00005
        approx(m[11], -1.0);
        approx(m[14], (2.0 * 4000.0 * 0.1) / (0.1 - 4000.0)); // -0.200005
        // aspect scaling divides x.
        let wide = Mat4::perspective(50.0, 2.0, 0.1, 4000.0).0;
        approx(wide[0], 2.144507 / 2.0);
        approx(wide[5], 2.144507);
    }

    #[test]
    fn look_at_translates_along_z() {
        // Eye 10 units down +z looking at origin → identity rotation, z shifted.
        let v = Mat4::look_at([0.0, 0.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let m = v.0;
        approx(m[0], 1.0);
        approx(m[5], 1.0);
        approx(m[10], 1.0);
        approx(m[14], -10.0);
        // Origin lands at (0,0,-10) in view space.
        let p = v.transform4(0.0, 0.0, 0.0);
        approx(p[0], 0.0);
        approx(p[1], 0.0);
        approx(p[2], -10.0);
        approx(p[3], 1.0);
    }

    #[test]
    fn project_center_and_cull() {
        let proj = Mat4::perspective(50.0, 1.0, 0.1, 4000.0);
        let view = Mat4::look_at([0.0, 0.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let mvp = proj.mul(&view);
        // Scene origin projects to the exact screen center.
        let pr = project(&mvp, [0.0, 0.0, 0.0], 800.0, 600.0).unwrap();
        approx(pr.x, 400.0);
        approx(pr.y, 300.0);
        // A point behind the camera is culled.
        assert!(project(&mvp, [0.0, 0.0, 20.0], 800.0, 600.0).is_none());
    }

    #[test]
    fn view_depth_orders_far_behind_near() {
        let view = Mat4::look_at([0.0, 0.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let near = view_depth(&view, [0.0, 0.0, 5.0]); // closer to eye
        let far = view_depth(&view, [0.0, 0.0, -5.0]); // farther from eye
        assert!(far < near, "far ({far}) should be more negative than near ({near})");
    }
}
