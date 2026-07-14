//! `OrbitCamera` — the drag-to-orbit / shift-drag-pan / wheel-dolly camera with
//! inertial damping, ported from `OrbitCamera` in `weight-grid-gl.ts:110-257`.
//!
//! Input deltas accumulate into pending `d_yaw` / `d_pitch` / `d_pan`, which
//! [`OrbitCamera::update`] consumes gradually (damping 0.08) so motion keeps
//! settling for a few frames after the pointer stops — the same inertia the
//! WebGL loop had. Dolly (wheel) applies immediately, matching the TS.

use super::math::{Mat4, Vec3};

/// World height of a full-magnitude bar (weight-grid-3d.ts `HEIGHT_SCALE`);
/// the initial framing pulls back proportional to it.
pub const HEIGHT_SCALE: f32 = 12.0;

const MAX_PITCH: f32 = 1.53; // just short of the poles (weight-grid-gl.ts:108)

#[derive(Debug, Clone)]
pub struct OrbitCamera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub fov_y: f32,
    pub min_dist: f32,
    pub max_dist: f32,
    pub damping: f32,

    d_yaw: f32,
    d_pitch: f32,
    d_pan: Vec3,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        OrbitCamera {
            target: [0.0, 0.0, 0.0],
            yaw: std::f32::consts::FRAC_PI_4, // π/4
            pitch: 0.6,
            dist: 120.0,
            fov_y: 50.0,
            min_dist: 0.5,
            max_dist: 3500.0,
            damping: 0.08,
            d_yaw: 0.0,
            d_pitch: 0.0,
            d_pan: [0.0, 0.0, 0.0],
        }
    }
}

impl OrbitCamera {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place the camera at `target + offset` and cancel in-flight motion
    /// (weight-grid-gl.ts:154-162).
    pub fn set_from_offset(&mut self, offset: Vec3) {
        let len = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2])
            .sqrt()
            .max(1e-6);
        self.dist = len.clamp(self.min_dist, self.max_dist);
        self.pitch = (offset[1] / len).asin();
        self.yaw = offset[0].atan2(offset[2]);
        self.d_yaw = 0.0;
        self.d_pitch = 0.0;
        self.d_pan = [0.0, 0.0, 0.0];
    }

    /// Initial framing for a `rows × cols` matrix (weight-grid-3d.ts:814-821):
    /// `dist = max(rows,cols,4)·1.3 + HEIGHT_SCALE·2`, offset `(0.55,0.7,0.85)·dist`.
    pub fn frame(&mut self, rows: u32, cols: u32) {
        let max_dim = rows.max(cols).max(4) as f32;
        let dist = max_dim * 1.3 + HEIGHT_SCALE * 2.0;
        self.target = [0.0, 0.0, 0.0];
        self.set_from_offset([dist * 0.55, dist * 0.7, dist * 0.85]);
    }

    pub fn eye(&self) -> Vec3 {
        let cp = self.pitch.cos();
        [
            self.target[0] + self.dist * cp * self.yaw.sin(),
            self.target[1] + self.dist * self.pitch.sin(),
            self.target[2] + self.dist * cp * self.yaw.cos(),
        ]
    }

    pub fn view_matrix(&self) -> Mat4 {
        Mat4::look_at(self.eye(), self.target, [0.0, 1.0, 0.0])
    }

    // ── Input (accumulates pending deltas) ────────────────────────────────────

    /// Left-drag orbit: `dx`/`dy` are pixel deltas, `h` the viewport height.
    pub fn orbit(&mut self, dx: f32, dy: f32, h: f32) {
        let h = h.max(1.0);
        self.d_yaw -= 2.0 * std::f32::consts::PI * dx / h;
        self.d_pitch -= 2.0 * std::f32::consts::PI * dy / h;
    }

    /// Shift/right-drag pan in the camera plane (weight-grid-gl.ts:220-234).
    pub fn pan(&mut self, dx: f32, dy: f32, h: f32) {
        let h = h.max(1.0);
        let td = self.dist * (self.fov_y * std::f32::consts::PI / 360.0).tan();
        let rx = self.yaw.cos();
        let rz = -self.yaw.sin();
        let sp = self.pitch.sin();
        let cp = self.pitch.cos();
        let ux = -sp * self.yaw.sin();
        let uy = cp;
        let uz = -sp * self.yaw.cos();
        let px = (2.0 * dx * td) / h;
        let py = (2.0 * dy * td) / h;
        self.d_pan[0] += -rx * px + ux * py;
        self.d_pan[1] += uy * py;
        self.d_pan[2] += -rz * px + uz * py;
    }

    /// Wheel dolly `dist *= exp(delta_y · 0.0015)`, clamped (applies at once).
    pub fn dolly(&mut self, delta_y: f32) {
        self.dist = (self.dist * (delta_y * 0.0015).exp()).clamp(self.min_dist, self.max_dist);
    }

    /// Apply damped input deltas; returns `true` while motion is still settling
    /// (weight-grid-gl.ts:178-199). The owner keeps repainting until this is
    /// `false` (the idle-stopping contract).
    pub fn update(&mut self) -> bool {
        let f = self.damping;
        self.yaw += self.d_yaw * f;
        self.pitch = (self.pitch + self.d_pitch * f).clamp(-MAX_PITCH, MAX_PITCH);
        self.target[0] += self.d_pan[0] * f;
        self.target[1] += self.d_pan[1] * f;
        self.target[2] += self.d_pan[2] * f;
        let k = 1.0 - f;
        self.d_yaw *= k;
        self.d_pitch *= k;
        self.d_pan[0] *= k;
        self.d_pan[1] *= k;
        self.d_pan[2] *= k;
        let pan_mag = (self.d_pan[0] * self.d_pan[0]
            + self.d_pan[1] * self.d_pan[1]
            + self.d_pan[2] * self.d_pan[2])
            .sqrt();
        let moving =
            self.d_yaw.abs() + self.d_pitch.abs() > 1e-4 || pan_mag > self.dist * 1e-4;
        if !moving {
            self.d_yaw = 0.0;
            self.d_pitch = 0.0;
            self.d_pan = [0.0, 0.0, 0.0];
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "expected {b}, got {a}");
    }

    #[test]
    fn defaults_match_spec() {
        let c = OrbitCamera::new();
        approx(c.yaw, std::f32::consts::FRAC_PI_4);
        approx(c.pitch, 0.6);
        approx(c.dist, 120.0);
        approx(c.fov_y, 50.0);
        approx(c.damping, 0.08);
    }

    #[test]
    fn set_from_offset_round_trips_to_eye() {
        let mut c = OrbitCamera::new();
        c.set_from_offset([60.0, 60.0, 90.0]);
        let e = c.eye();
        approx(e[0], 60.0);
        approx(e[1], 60.0);
        approx(e[2], 90.0);
    }

    #[test]
    fn dolly_clamps_range() {
        let mut c = OrbitCamera::new();
        for _ in 0..500 {
            c.dolly(100.0); // zoom way out
        }
        approx(c.dist, 3500.0);
        for _ in 0..1000 {
            c.dolly(-100.0); // zoom way in
        }
        approx(c.dist, 0.5);
    }

    #[test]
    fn damping_settles_after_orbit() {
        let mut c = OrbitCamera::new();
        c.orbit(50.0, 0.0, 600.0);
        let y0 = c.yaw;
        assert!(c.update()); // still moving
        assert_ne!(c.yaw, y0); // yaw advanced
        // A finite number of frames returns it to rest.
        let mut frames = 1;
        while c.update() {
            frames += 1;
            assert!(frames < 10_000, "damping never settled");
        }
        assert!(frames > 1);
    }

    #[test]
    fn frame_pulls_back_proportionally() {
        let mut c = OrbitCamera::new();
        c.frame(64, 64);
        // Framing scalar d = max(64,64,4)*1.3 + 24 = 107.2; the eye lands at
        // (0.55, 0.7, 0.85)·d and camera dist becomes |offset| (as in the TS,
        // where setFromOffset re-derives dist from the offset length).
        let d = 64.0f32 * 1.3 + 24.0;
        let e = c.eye();
        approx(e[0], 0.55 * d);
        approx(e[1], 0.7 * d);
        approx(e[2], 0.85 * d);
        approx(c.dist, d * (0.55f32 * 0.55 + 0.7 * 0.7 + 0.85 * 0.85).sqrt());
        assert_eq!(c.target, [0.0, 0.0, 0.0]);
        // Small matrices floor at max(.,4).
        c.frame(1, 1);
        let d_small = 4.0f32 * 1.3 + 24.0;
        approx(c.eye()[1], 0.7 * d_small);
    }
}
