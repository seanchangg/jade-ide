// ─────────────────────────────────────────────────────────────────────────────
// weight-grid-gl — minimal WebGL2 scaffolding for WeightGrid3D.
//
// Provides the pieces three.js used to supply, sized to exactly what the
// weight grid needs:
//   • column-major mat4 perspective / lookAt helpers
//   • OrbitCamera — drag to orbit with inertial damping, right-/shift-drag to
//     pan, wheel to dolly (an OrbitControls stand-in)
//   • compileProgram — shader compile + link with error surfacing
//   • LabelTexture — text rendered into a 2D canvas and uploaded as a GL
//     texture, drawn by the caller as a camera-facing billboard
// ─────────────────────────────────────────────────────────────────────────────

export type Vec3 = [number, number, number];

// ── Matrices (column-major, WebGL convention) ────────────────────────────────

export function mat4Perspective(
  out: Float32Array,
  fovYDeg: number,
  aspect: number,
  near: number,
  far: number
): Float32Array {
  const f = 1 / Math.tan((fovYDeg * Math.PI) / 360);
  out.fill(0);
  out[0] = f / aspect;
  out[5] = f;
  out[10] = (far + near) / (near - far);
  out[11] = -1;
  out[14] = (2 * far * near) / (near - far);
  return out;
}

export function mat4LookAt(out: Float32Array, eye: Vec3, target: Vec3, up: Vec3): Float32Array {
  let zx = eye[0] - target[0];
  let zy = eye[1] - target[1];
  let zz = eye[2] - target[2];
  let len = Math.hypot(zx, zy, zz) || 1;
  zx /= len;
  zy /= len;
  zz /= len;
  let xx = up[1] * zz - up[2] * zy;
  let xy = up[2] * zx - up[0] * zz;
  let xz = up[0] * zy - up[1] * zx;
  len = Math.hypot(xx, xy, xz) || 1;
  xx /= len;
  xy /= len;
  xz /= len;
  const yx = zy * xz - zz * xy;
  const yy = zz * xx - zx * xz;
  const yz = zx * xy - zy * xx;
  out[0] = xx;
  out[1] = yx;
  out[2] = zx;
  out[3] = 0;
  out[4] = xy;
  out[5] = yy;
  out[6] = zy;
  out[7] = 0;
  out[8] = xz;
  out[9] = yz;
  out[10] = zz;
  out[11] = 0;
  out[12] = -(xx * eye[0] + xy * eye[1] + xz * eye[2]);
  out[13] = -(yx * eye[0] + yy * eye[1] + yz * eye[2]);
  out[14] = -(zx * eye[0] + zy * eye[1] + zz * eye[2]);
  out[15] = 1;
  return out;
}

// ── Shaders ──────────────────────────────────────────────────────────────────

export function compileProgram(
  gl: WebGL2RenderingContext,
  vsSrc: string,
  fsSrc: string
): WebGLProgram {
  const compile = (type: number, src: string): WebGLShader => {
    const sh = gl.createShader(type)!;
    gl.shaderSource(sh, src);
    gl.compileShader(sh);
    if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(sh);
      gl.deleteShader(sh);
      throw new Error(`shader compile failed: ${log}`);
    }
    return sh;
  };
  const vs = compile(gl.VERTEX_SHADER, vsSrc);
  const fs = compile(gl.FRAGMENT_SHADER, fsSrc);
  const prog = gl.createProgram()!;
  gl.attachShader(prog, vs);
  gl.attachShader(prog, fs);
  gl.linkProgram(prog);
  gl.deleteShader(vs);
  gl.deleteShader(fs);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
    const log = gl.getProgramInfoLog(prog);
    gl.deleteProgram(prog);
    throw new Error(`program link failed: ${log}`);
  }
  return prog;
}

// ── Orbit camera ─────────────────────────────────────────────────────────────

const MAX_PITCH = 1.53; // just short of the poles (bars extend below the plane)

export class OrbitCamera {
  target: Vec3 = [0, 0, 0];
  yaw = Math.PI / 4;
  pitch = 0.6;
  dist = 120;
  fovY = 50;
  minDist = 0.5;
  maxDist = 3500;
  dampingFactor = 0.08;
  /** Fired on any input so the owner can wake its render loop. */
  onChange: () => void = () => {};

  // Pending input deltas, consumed gradually by update() for inertia.
  private dYaw = 0;
  private dPitch = 0;
  private dPan: Vec3 = [0, 0, 0];
  private mode: 'rotate' | 'pan' | null = null;
  private lastX = 0;
  private lastY = 0;
  private canvas: HTMLCanvasElement | null = null;

  attach(canvas: HTMLCanvasElement): void {
    this.canvas = canvas;
    canvas.addEventListener('pointerdown', this.onPointerDown);
    canvas.addEventListener('pointermove', this.onPointerMove);
    canvas.addEventListener('pointerup', this.onPointerEnd);
    canvas.addEventListener('pointercancel', this.onPointerEnd);
    canvas.addEventListener('wheel', this.onWheel, { passive: false });
    canvas.addEventListener('contextmenu', this.onContextMenu);
  }

  detach(): void {
    const c = this.canvas;
    if (!c) return;
    c.removeEventListener('pointerdown', this.onPointerDown);
    c.removeEventListener('pointermove', this.onPointerMove);
    c.removeEventListener('pointerup', this.onPointerEnd);
    c.removeEventListener('pointercancel', this.onPointerEnd);
    c.removeEventListener('wheel', this.onWheel);
    c.removeEventListener('contextmenu', this.onContextMenu);
    this.canvas = null;
  }

  /** Place the camera at target + offset and cancel any in-flight motion. */
  setFromOffset(offset: Vec3): void {
    const len = Math.hypot(offset[0], offset[1], offset[2]) || 1;
    this.dist = Math.min(this.maxDist, Math.max(this.minDist, len));
    this.pitch = Math.asin(offset[1] / len);
    this.yaw = Math.atan2(offset[0], offset[2]);
    this.dYaw = 0;
    this.dPitch = 0;
    this.dPan = [0, 0, 0];
  }

  eye(): Vec3 {
    const cp = Math.cos(this.pitch);
    return [
      this.target[0] + this.dist * cp * Math.sin(this.yaw),
      this.target[1] + this.dist * Math.sin(this.pitch),
      this.target[2] + this.dist * cp * Math.cos(this.yaw),
    ];
  }

  viewMatrix(out: Float32Array): Float32Array {
    return mat4LookAt(out, this.eye(), this.target, [0, 1, 0]);
  }

  /** Apply damped input deltas; returns true while motion is still settling. */
  update(): boolean {
    const f = this.dampingFactor;
    this.yaw += this.dYaw * f;
    this.pitch = Math.min(MAX_PITCH, Math.max(-MAX_PITCH, this.pitch + this.dPitch * f));
    this.target[0] += this.dPan[0] * f;
    this.target[1] += this.dPan[1] * f;
    this.target[2] += this.dPan[2] * f;
    const k = 1 - f;
    this.dYaw *= k;
    this.dPitch *= k;
    this.dPan[0] *= k;
    this.dPan[1] *= k;
    this.dPan[2] *= k;
    const panMag = Math.hypot(this.dPan[0], this.dPan[1], this.dPan[2]);
    const moving = Math.abs(this.dYaw) + Math.abs(this.dPitch) > 1e-4 || panMag > this.dist * 1e-4;
    if (!moving) {
      this.dYaw = 0;
      this.dPitch = 0;
      this.dPan = [0, 0, 0];
    }
    return moving;
  }

  private onPointerDown = (e: PointerEvent): void => {
    if (e.button !== 0 && e.button !== 2) return;
    this.mode = e.button === 2 || e.shiftKey ? 'pan' : 'rotate';
    this.lastX = e.clientX;
    this.lastY = e.clientY;
    this.canvas?.setPointerCapture(e.pointerId);
    e.preventDefault();
  };

  private onPointerMove = (e: PointerEvent): void => {
    if (!this.mode || !this.canvas) return;
    const dx = e.clientX - this.lastX;
    const dy = e.clientY - this.lastY;
    this.lastX = e.clientX;
    this.lastY = e.clientY;
    const h = this.canvas.clientHeight || 1;
    if (this.mode === 'rotate') {
      this.dYaw -= (2 * Math.PI * dx) / h;
      this.dPitch -= (2 * Math.PI * dy) / h;
    } else {
      // Pan in the camera plane, scaled so a full-height drag spans the view.
      const td = this.dist * Math.tan((this.fovY * Math.PI) / 360);
      const rx = Math.cos(this.yaw);
      const rz = -Math.sin(this.yaw);
      const sp = Math.sin(this.pitch);
      const cp = Math.cos(this.pitch);
      const ux = -sp * Math.sin(this.yaw);
      const uy = cp;
      const uz = -sp * Math.cos(this.yaw);
      const px = (2 * dx * td) / h;
      const py = (2 * dy * td) / h;
      this.dPan[0] += -rx * px + ux * py;
      this.dPan[1] += uy * py;
      this.dPan[2] += -rz * px + uz * py;
    }
    this.onChange();
  };

  private onPointerEnd = (e: PointerEvent): void => {
    if (!this.mode) return;
    this.mode = null;
    if (this.canvas?.hasPointerCapture(e.pointerId)) {
      this.canvas.releasePointerCapture(e.pointerId);
    }
  };

  private onWheel = (e: WheelEvent): void => {
    e.preventDefault();
    const dy = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaY;
    this.dist = Math.min(this.maxDist, Math.max(this.minDist, this.dist * Math.exp(dy * 0.0015)));
    this.onChange();
  };

  private onContextMenu = (e: Event): void => {
    e.preventDefault();
  };
}

// ── Text labels ──────────────────────────────────────────────────────────────

export class LabelTexture {
  readonly tex: WebGLTexture;
  // World position of the label center and world-unit billboard size.
  x = 0;
  y = 0;
  z = 0;
  w = 1;
  h = 1;

  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;

  constructor(gl: WebGL2RenderingContext, text: string) {
    this.canvas = document.createElement('canvas');
    this.canvas.width = 256;
    this.canvas.height = 64;
    this.ctx = this.canvas.getContext('2d')!;
    this.tex = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    this.setText(gl, text);
  }

  setText(gl: WebGL2RenderingContext, text: string): void {
    const ctx = this.ctx;
    ctx.clearRect(0, 0, 256, 64);
    ctx.font = '30px "JetBrains Mono", Menlo, monospace';
    ctx.fillStyle = 'rgba(190, 196, 214, 0.95)';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(text, 128, 32);
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    // Premultiplied upload pairs with (ONE, ONE_MINUS_SRC_ALPHA) blending.
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, this.canvas);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  }

  dispose(gl: WebGL2RenderingContext): void {
    gl.deleteTexture(this.tex);
  }
}
