//! 3D weight-grid visualization (feature inventory §7.2) — a self-contained
//! Phase-4 module ported from the Electron app's raw-WebGL2 `WeightGrid3D`.
//!
//! Layout of the port:
//!   * [`math`]   — column-major mat4 perspective / look-at + screen projection,
//!                  ported verbatim from `weight-grid-gl.ts` and unit-tested.
//!   * [`camera`] — the orbit camera (yaw/pitch/dist/pan + inertial damping).
//!   * [`grid`]   — [`WeightGrid3D`]: the 64-frame-per-buffer ring, the buffer
//!                  selector + live/freeze scrubber, sqrt-stride subsample,
//!                  diverging colormap, and frame→bars conversion (all pure).
//!   * [`render`] — the GPUI overlay: a `size_full()` canvas that projects and
//!                  depth-sorts bars and paints their shaded faces as filled
//!                  paths, plus the toolbar and projected label overlays.
//!
//! The rest of the app touches this module in exactly three tiny spots (see
//! `app.rs`): it holds one [`WeightGrid3D`], feeds every tensor frame into its
//! ring on the telemetry apply path, and overlays [`render::overlay`] while
//! `visible`.

pub mod camera;
pub mod grid;
pub mod math;
pub mod render;

pub use grid::WeightGrid3D;
