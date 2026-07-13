// ─────────────────────────────────────────────────────────────────────────────
// WeightGrid3D — full-panel 3D visualization of a live weight matrix,
// rendered with raw WebGL2 (no three.js).
//
// Each matrix cell becomes one bar of a single instanced unit-box draw:
//   • bar HEIGHT  ∝ |value|  (magnitude, normalized by the frame's max-abs)
//   • bar COLOR   = diverging blue → white → red colormap centered at 0
//                   (blue = negative, red = positive)
// Per-instance data is 6 floats (grid x, grid z, signed height, r, g, b); the
// vertex shader stretches a shared unit box, so applying a frame is one
// bufferSubData + one drawElementsInstanced. Axis text is drawn as
// canvas-rendered textures on camera-facing billboard quads.
//
// It keeps its own ring buffer (last N frames per buffer name) fed from the
// 'tensorFrame' store event, independent of training-view's map. A buffer
// selector + step scrubber let the user replay how a matrix evolved during
// training; live mode auto-advances when the scrubber sits at the newest frame.
//
// Performance: the render loop only runs while the overlay is VISIBLE and
// something is dirty (a new frame was applied OR the orbit camera is still
// moving). When idle it stops its rAF loop; destroy() deletes all GL resources
// and releases the context (WEBGL_lose_context) to avoid leaking GPU contexts.
// ─────────────────────────────────────────────────────────────────────────────

import '../styles/weight-grid.css';
import { store } from '../state';
import { telemetryRegistry } from '../panels/telemetry-panel';
import type { TensorFrame } from '../panels/telemetry-panel';
import { OrbitCamera, LabelTexture, compileProgram, mat4Perspective } from './weight-grid-gl';

const RING = 64; // frames retained per buffer name
const HEIGHT_SCALE = 12; // world height of a full-magnitude bar
const MIN_BAR = 0.02; // floor so near-zero cells still register a footprint
const MAX_CELLS = 256 * 256; // instance cap; larger frames get subsampled
const FOV_Y = 50;

// ── Shaders ──────────────────────────────────────────────────────────────────

// Instanced bars: a_pos is the shared unit box (xz ∈ [±0.45], y ∈ [0,1]);
// a_cell = (grid x, grid z, signed height). Scaling y by a signed height grows
// negative bars below the plane; the normal's y flips with it for lighting.
const BARS_VS = `#version 300 es
precision highp float;
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_nrm;
layout(location = 2) in vec3 a_cell;
layout(location = 3) in vec3 a_color;
uniform mat4 u_proj;
uniform mat4 u_view;
out vec3 v_nrm;
out vec3 v_color;
void main() {
  float h = a_cell.z;
  float s = h < 0.0 ? -1.0 : 1.0;
  vec3 world = vec3(a_pos.x + a_cell.x, a_pos.y * h, a_pos.z + a_cell.y);
  v_nrm = vec3(a_nrm.x, a_nrm.y * s, a_nrm.z);
  v_color = a_color;
  gl_Position = u_proj * u_view * vec4(world, 1.0);
}`;

// Lambert with the same three-light rig as before: ambient 0.65 plus
// directionals 0.9 from (40,80,30) and 0.3 from (-30,40,-50), pre-normalized.
const BARS_FS = `#version 300 es
precision highp float;
in vec3 v_nrm;
in vec3 v_color;
out vec4 o_color;
void main() {
  vec3 n = normalize(v_nrm);
  const vec3 L1 = vec3(0.4240, 0.8480, 0.3180);
  const vec3 L2 = vec3(-0.4243, 0.5657, -0.7071);
  float light = 0.65 + 0.9 * max(dot(n, L1), 0.0) + 0.3 * max(dot(n, L2), 0.0);
  o_color = vec4(v_color * min(light, 1.0), 1.0);
}`;

const LINES_VS = `#version 300 es
precision highp float;
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_color;
uniform mat4 u_proj;
uniform mat4 u_view;
out vec3 v_color;
void main() {
  v_color = a_color;
  gl_Position = u_proj * u_view * vec4(a_pos, 1.0);
}`;

// Premultiplied output to match the (ONE, ONE_MINUS_SRC_ALPHA) blend mode.
const LINES_FS = `#version 300 es
precision highp float;
in vec3 v_color;
uniform float u_opacity;
out vec4 o_color;
void main() {
  o_color = vec4(v_color * u_opacity, u_opacity);
}`;

// Billboards: offset the label's view-space position by the quad corner so it
// always faces the camera while keeping a world-space size.
const LABEL_VS = `#version 300 es
precision highp float;
layout(location = 0) in vec2 a_corner;
uniform mat4 u_proj;
uniform mat4 u_view;
uniform vec3 u_center;
uniform vec2 u_scale;
out vec2 v_uv;
void main() {
  vec4 viewPos = u_view * vec4(u_center, 1.0);
  viewPos.xy += a_corner * u_scale;
  v_uv = vec2(a_corner.x + 0.5, 0.5 - a_corner.y);
  gl_Position = u_proj * viewPos;
}`;

const LABEL_FS = `#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
out vec4 o_color;
void main() {
  o_color = texture(u_tex, v_uv);
}`;

// Unit box for one bar: 0.9×0.9 footprint, y ∈ [0,1] so instances scale from
// the plane. Interleaved position+normal, 4 verts per face, 36 indices.
function buildBoxGeometry(): { verts: Float32Array; indices: Uint16Array } {
  const X = 0.45;
  const Z = 0.45;
  const faces: Array<{ n: number[]; c: number[][] }> = [
    { n: [0, 1, 0], c: [[-X, 1, -Z], [-X, 1, Z], [X, 1, Z], [X, 1, -Z]] },
    { n: [0, -1, 0], c: [[-X, 0, -Z], [X, 0, -Z], [X, 0, Z], [-X, 0, Z]] },
    { n: [1, 0, 0], c: [[X, 0, -Z], [X, 1, -Z], [X, 1, Z], [X, 0, Z]] },
    { n: [-1, 0, 0], c: [[-X, 0, -Z], [-X, 0, Z], [-X, 1, Z], [-X, 1, -Z]] },
    { n: [0, 0, 1], c: [[-X, 0, Z], [X, 0, Z], [X, 1, Z], [-X, 1, Z]] },
    { n: [0, 0, -1], c: [[-X, 0, -Z], [-X, 1, -Z], [X, 1, -Z], [X, 0, -Z]] },
  ];
  const verts = new Float32Array(24 * 6);
  const indices = new Uint16Array(36);
  let vo = 0;
  let io = 0;
  faces.forEach((face, f) => {
    for (const corner of face.c) {
      verts[vo++] = corner[0];
      verts[vo++] = corner[1];
      verts[vo++] = corner[2];
      verts[vo++] = face.n[0];
      verts[vo++] = face.n[1];
      verts[vo++] = face.n[2];
    }
    const base = f * 4;
    indices[io++] = base;
    indices[io++] = base + 1;
    indices[io++] = base + 2;
    indices[io++] = base;
    indices[io++] = base + 2;
    indices[io++] = base + 3;
  });
  return { verts, indices };
}

export class WeightGrid3D {
  private overlay: HTMLElement;
  private canvas: HTMLCanvasElement;
  private select: HTMLSelectElement;
  private scrubber: HTMLInputElement;
  private readout: HTMLElement;
  private rowsInput: HTMLInputElement;
  private colsInput: HTMLInputElement;
  private resSelect: HTMLSelectElement;

  // Per-buffer ring buffer of decoded frames.
  private buffers = new Map<string, TensorFrame[]>();
  private selected: string | null = null;
  private index = 0; // index into the selected buffer's frames
  private live = true; // auto-advance to newest frame when true

  private visible = false;
  private disposed = false;
  private unsub: () => void;

  // WebGL (created lazily on first open()).
  private gl: WebGL2RenderingContext | null = null;
  private camera: OrbitCamera | null = null;
  private barsProg: WebGLProgram | null = null;
  private linesProg: WebGLProgram | null = null;
  private labelProg: WebGLProgram | null = null;
  private barsU: Record<'proj' | 'view', WebGLUniformLocation | null> = { proj: null, view: null };
  private linesU: Record<'proj' | 'view' | 'opacity', WebGLUniformLocation | null> = {
    proj: null,
    view: null,
    opacity: null,
  };
  private labelU: Record<'proj' | 'view' | 'center' | 'scale' | 'tex', WebGLUniformLocation | null> = {
    proj: null,
    view: null,
    center: null,
    scale: null,
    tex: null,
  };
  private boxVao: WebGLVertexArrayObject | null = null;
  private boxVbo: WebGLBuffer | null = null;
  private boxIbo: WebGLBuffer | null = null;
  private instanceVbo: WebGLBuffer | null = null;
  private instanceData: Float32Array | null = null;
  private gridVao: WebGLVertexArrayObject | null = null;
  private gridVbo: WebGLBuffer | null = null;
  private gridVertexCount = 0;
  private axisVao: WebGLVertexArrayObject | null = null;
  private axisVbo: WebGLBuffer | null = null;
  private axisVertexCount = 0;
  private quadVao: WebGLVertexArrayObject | null = null;
  private quadVbo: WebGLBuffer | null = null;
  private labels: LabelTexture[] = [];
  private yValueLabel: LabelTexture | null = null;
  private yNegValueLabel: LabelTexture | null = null;
  private axesBuilt = false;
  private lastYLabel = '';
  private proj = new Float32Array(16);
  private view = new Float32Array(16);
  private rows = 0;
  private cols = 0;
  private axisSrcRows = 0;
  private axisSrcCols = 0;

  // Render scheduling.
  private needsRender = false;
  private looping = false;
  private raf = 0;
  private ro: ResizeObserver | null = null;

  constructor(parent: HTMLElement) {
    this.overlay = document.createElement('div');
    this.overlay.className = 'wg3d-overlay';

    // Toolbar
    const toolbar = document.createElement('div');
    toolbar.className = 'wg3d-toolbar';

    const title = document.createElement('span');
    title.className = 'wg3d-title';
    title.textContent = '3D WEIGHTS';
    toolbar.appendChild(title);

    this.select = document.createElement('select');
    this.select.className = 'wg3d-select';
    this.select.title = 'Buffer';
    this.select.addEventListener('change', () => this.selectBuffer(this.select.value));
    toolbar.appendChild(this.select);

    // Tensor shape override: rows × cols, sent to the probe as a hint (the
    // shape of a raw MTLBuffer is otherwise guessed as a square).
    const makeDimInput = (title: string): HTMLInputElement => {
      const input = document.createElement('input');
      input.type = 'number';
      input.min = '1';
      input.className = 'wg3d-dim';
      input.title = title;
      input.placeholder = title;
      input.addEventListener('change', () => this.applyShape());
      input.addEventListener('keydown', (e) => e.stopPropagation()); // keep Escape for the field
      return input;
    };
    this.rowsInput = makeDimInput('rows');
    const dimSep = document.createElement('span');
    dimSep.className = 'wg3d-dim-sep';
    dimSep.textContent = '×';
    this.colsInput = makeDimInput('cols');
    toolbar.appendChild(this.rowsInput);
    toolbar.appendChild(dimSep);
    toolbar.appendChild(this.colsInput);

    // Streaming resolution for the selected buffer (default 256, the full
    // detail the probe streams; drop it here when overhead matters).
    this.resSelect = document.createElement('select');
    this.resSelect.className = 'wg3d-select wg3d-res';
    this.resSelect.title = 'Streaming resolution (max cells per dimension)';
    for (const dim of [64, 128, 256]) {
      const opt = document.createElement('option');
      opt.value = String(dim);
      opt.textContent = `≤${dim}`;
      this.resSelect.appendChild(opt);
    }
    this.resSelect.addEventListener('change', () => {
      if (!this.selected) return;
      telemetryRegistry.setMaxDim('buffer', this.selected, parseInt(this.resSelect.value, 10));
    });
    toolbar.appendChild(this.resSelect);

    this.scrubber = document.createElement('input');
    this.scrubber.type = 'range';
    this.scrubber.className = 'wg3d-scrubber';
    this.scrubber.min = '0';
    this.scrubber.max = '0';
    this.scrubber.value = '0';
    this.scrubber.title = 'Training step';
    this.scrubber.addEventListener('input', () => this.onScrub());
    toolbar.appendChild(this.scrubber);

    this.readout = document.createElement('span');
    this.readout.className = 'wg3d-readout';
    this.readout.textContent = 'no data';
    toolbar.appendChild(this.readout);

    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'wg3d-close';
    close.title = 'Close';
    close.textContent = '×';
    close.addEventListener('click', () => this.close());
    toolbar.appendChild(close);

    this.overlay.appendChild(toolbar);

    this.canvas = document.createElement('canvas');
    this.canvas.className = 'wg3d-canvas';
    this.overlay.appendChild(this.canvas);

    parent.appendChild(this.overlay);

    // Fill the ring buffer even while hidden (cheap array pushes); only apply to
    // the instance buffer / request renders when visible.
    this.unsub = store.on('tensorFrame', (frame: TensorFrame) => this.onFrame(frame));

    // Close on Escape while open.
    this.overlay.tabIndex = -1;
    this.overlay.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') this.close();
    });
  }

  // ── Public API ─────────────────────────────────────────────────────────────

  /** Show the overlay, initialize GL lazily, and optionally focus a buffer. */
  open(bufferName?: string): void {
    if (this.disposed) return;
    this.initGL();
    this.overlay.classList.add('visible');
    this.visible = true;

    if (bufferName) {
      this.selectBuffer(bufferName);
    } else if (!this.selected && this.buffers.size > 0) {
      this.selectBuffer(this.buffers.keys().next().value as string);
    } else if (this.selected) {
      this.syncSelectEl();
      this.applyCurrent();
    }

    this.resize();
    this.overlay.focus();
    this.requestRender();
  }

  /** Hide the overlay and stop the render loop (keeps ring buffers + GL alive). */
  close(): void {
    this.visible = false;
    this.overlay.classList.remove('visible');
    this.needsRender = false;
    this.looping = false;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
  }

  /** Point the view at a specific buffer (opens nothing on its own). */
  focusBuffer(name: string): void {
    if (this.buffers.has(name)) this.selectBuffer(name);
  }

  get isOpen(): boolean {
    return this.visible;
  }

  /** Full teardown: dispose GL resources, drop the DOM, unsubscribe. */
  destroy(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.close();
    this.unsub();
    this.ro?.disconnect();
    this.ro = null;
    this.camera?.detach();
    this.camera = null;
    const gl = this.gl;
    if (gl) {
      this.disposeAxes();
      for (const p of [this.barsProg, this.linesProg, this.labelProg]) {
        if (p) gl.deleteProgram(p);
      }
      for (const b of [this.boxVbo, this.boxIbo, this.instanceVbo, this.gridVbo, this.axisVbo, this.quadVbo]) {
        if (b) gl.deleteBuffer(b);
      }
      for (const v of [this.boxVao, this.gridVao, this.axisVao, this.quadVao]) {
        if (v) gl.deleteVertexArray(v);
      }
      gl.getExtension('WEBGL_lose_context')?.loseContext();
    }
    this.gl = null;
    this.barsProg = this.linesProg = this.labelProg = null;
    this.overlay.remove();
  }

  // ── Data ingestion ──────────────────────────────────────────────────────────

  private onFrame(frame: TensorFrame): void {
    let frames = this.buffers.get(frame.name);
    if (!frames) {
      frames = [];
      this.buffers.set(frame.name, frames);
      this.addOption(frame.name);
      // Auto-focus the first buffer we ever see if nothing is selected.
      if (!this.selected) this.selectBuffer(frame.name);
    }
    frames.push(frame);
    if (frames.length > RING) frames.splice(0, frames.length - RING);

    if (!this.visible || frame.name !== this.selected) return;

    if (this.live) {
      this.index = frames.length - 1;
      this.applyCurrent();
    } else {
      // Not live: keep the scrubber's max in sync but don't move the view.
      this.syncScrubber();
    }
  }

  private addOption(name: string): void {
    // Dedupe — a name may resurface after renames or reopened sessions.
    for (let i = 0; i < this.select.options.length; i++) {
      if (this.select.options[i].value === name) return;
    }
    const opt = document.createElement('option');
    opt.value = name;
    opt.textContent = name;
    this.select.appendChild(opt);
  }

  // ── Buffer selector + scrubber ───────────────────────────────────────────────

  private selectBuffer(name: string): void {
    // Trust the request even if no frames have arrived yet (they may still be
    // streaming in); applyCurrent shows "(no frames)" until they do.
    this.selected = name;
    this.addOption(name);
    this.syncSelectEl();
    this.syncDimInputs();
    const frames = this.buffers.get(name) || [];
    this.live = true;
    this.index = Math.max(0, frames.length - 1);
    this.applyCurrent();
  }

  // Populate the shape fields from the stored hint, or the latest frame's
  // source dimensions as a placeholder.
  private syncDimInputs(): void {
    if (!this.selected) return;
    this.resSelect.value = String(telemetryRegistry.getMaxDim('buffer', this.selected));
    const hint = telemetryRegistry.getShape('buffer', this.selected);
    if (hint) {
      this.rowsInput.value = String(hint.rows);
      this.colsInput.value = String(hint.cols);
      return;
    }
    this.rowsInput.value = '';
    this.colsInput.value = '';
    const frames = this.buffers.get(this.selected);
    const last = frames?.[frames.length - 1];
    if (last) {
      this.rowsInput.placeholder = String(last.srcRows ?? last.rows);
      this.colsInput.placeholder = String(last.srcCols ?? last.cols);
    }
  }

  private applyShape(): void {
    if (!this.selected) return;
    const rows = parseInt(this.rowsInput.value, 10);
    const cols = parseInt(this.colsInput.value, 10);
    if (!Number.isFinite(rows) || !Number.isFinite(cols) || rows < 1 || cols < 1) return;
    // Probe applies the hint to FUTURE frames (and re-emits immediately when
    // the program is still running).
    telemetryRegistry.setShape('buffer', this.selected, rows, cols);
  }

  // Sync the <select> UI to this.selected by index — assigning .value fails
  // silently (keeps the previous selection) if anything mismatches.
  private syncSelectEl(): void {
    const opts = this.select.options;
    for (let i = 0; i < opts.length; i++) {
      if (opts[i].value === this.selected) {
        if (this.select.selectedIndex !== i) this.select.selectedIndex = i;
        return;
      }
    }
  }

  private onScrub(): void {
    const frames = this.selectedFrames();
    if (!frames.length) return;
    const idx = Math.min(frames.length - 1, Math.max(0, parseInt(this.scrubber.value, 10) || 0));
    this.index = idx;
    // Live only while parked at the newest frame; drag back → freeze.
    this.live = idx >= frames.length - 1;
    this.applyCurrent();
  }

  private selectedFrames(): TensorFrame[] {
    return (this.selected && this.buffers.get(this.selected)) || [];
  }

  private syncScrubber(): void {
    const frames = this.selectedFrames();
    const max = Math.max(0, frames.length - 1);
    this.scrubber.max = String(max);
    this.scrubber.disabled = max === 0;
    if (this.live) this.scrubber.value = String(this.index);
  }

  private applyCurrent(): void {
    const frames = this.selectedFrames();
    if (!frames.length) {
      this.readout.textContent = this.selected ? `${this.selected}  (no frames)` : 'no data';
      return;
    }
    this.index = Math.min(this.index, frames.length - 1);
    const frame = frames[this.index];
    this.scrubber.max = String(frames.length - 1);
    this.scrubber.value = String(this.index);
    this.scrubber.disabled = frames.length < 2;
    this.applyFrame(frame);
    let mn = Infinity, mx = -Infinity;
    for (let i = 0; i < frame.data.length; i++) {
      const v = frame.data[i];
      if (v < mn) mn = v;
      if (v > mx) mx = v;
    }
    const srcR = frame.srcRows ?? frame.rows;
    const srcC = frame.srcCols ?? frame.cols;
    const dims =
      srcR !== frame.rows || srcC !== frame.cols
        ? `${srcR}×${srcC} (shown ${frame.rows}×${frame.cols})`
        : `${frame.rows}×${frame.cols}`;
    this.readout.textContent =
      `${frame.name}  ${dims}  step ${frame.step}  ` +
      `${Number.isFinite(mn) ? `[${fmtAxis(mn)}…${fmtAxis(mx)}]  ` : ''}` +
      `[${this.index + 1}/${frames.length}]${this.live ? ' • live' : ''}`;
  }

  // ── WebGL setup ──────────────────────────────────────────────────────────────

  private initGL(): void {
    if (this.gl) return;

    const gl = this.canvas.getContext('webgl2', {
      antialias: true,
      alpha: true,
      premultipliedAlpha: true,
    });
    if (!gl) {
      this.readout.textContent = 'WebGL2 unavailable';
      return;
    }
    try {
      this.barsProg = compileProgram(gl, BARS_VS, BARS_FS);
      this.linesProg = compileProgram(gl, LINES_VS, LINES_FS);
      this.labelProg = compileProgram(gl, LABEL_VS, LABEL_FS);
    } catch (err) {
      console.error('[wg3d] WebGL init failed', err);
      this.readout.textContent = 'WebGL init failed';
      return;
    }
    this.gl = gl;

    this.barsU = {
      proj: gl.getUniformLocation(this.barsProg, 'u_proj'),
      view: gl.getUniformLocation(this.barsProg, 'u_view'),
    };
    this.linesU = {
      proj: gl.getUniformLocation(this.linesProg, 'u_proj'),
      view: gl.getUniformLocation(this.linesProg, 'u_view'),
      opacity: gl.getUniformLocation(this.linesProg, 'u_opacity'),
    };
    this.labelU = {
      proj: gl.getUniformLocation(this.labelProg, 'u_proj'),
      view: gl.getUniformLocation(this.labelProg, 'u_view'),
      center: gl.getUniformLocation(this.labelProg, 'u_center'),
      scale: gl.getUniformLocation(this.labelProg, 'u_scale'),
      tex: gl.getUniformLocation(this.labelProg, 'u_tex'),
    };

    // Shared unit-box geometry + persistent (re-sizable) instance buffer.
    const box = buildBoxGeometry();
    this.boxVbo = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.boxVbo);
    gl.bufferData(gl.ARRAY_BUFFER, box.verts, gl.STATIC_DRAW);
    this.boxIbo = gl.createBuffer();
    this.instanceVbo = gl.createBuffer();
    this.boxVao = gl.createVertexArray();
    gl.bindVertexArray(this.boxVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.boxVbo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 24, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 3, gl.FLOAT, false, 24, 12);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceVbo);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribPointer(2, 3, gl.FLOAT, false, 24, 0);
    gl.vertexAttribDivisor(2, 1);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 3, gl.FLOAT, false, 24, 12);
    gl.vertexAttribDivisor(3, 1);
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.boxIbo);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, box.indices, gl.STATIC_DRAW);
    gl.bindVertexArray(null);

    // Line layouts (reference grid + value axis); contents upload on build.
    this.gridVbo = gl.createBuffer();
    this.gridVao = this.makeLineVao(gl, this.gridVbo);
    this.axisVbo = gl.createBuffer();
    this.axisVao = this.makeLineVao(gl, this.axisVbo);

    // Billboard quad for labels.
    this.quadVbo = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.quadVbo);
    gl.bufferData(
      gl.ARRAY_BUFFER,
      new Float32Array([-0.5, -0.5, 0.5, -0.5, -0.5, 0.5, 0.5, 0.5]),
      gl.STATIC_DRAW
    );
    this.quadVao = gl.createVertexArray();
    gl.bindVertexArray(this.quadVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.quadVbo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    // Negative-height bars mirror their winding — skip culling entirely.
    gl.disable(gl.CULL_FACE);

    this.camera = new OrbitCamera();
    this.camera.fovY = FOV_Y;
    this.camera.setFromOffset([60, 60, 90]);
    // Any camera motion (drag / wheel) requests a render; damping keeps the
    // loop alive until it settles.
    this.camera.onChange = () => this.requestRender();
    this.camera.attach(this.canvas);

    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(this.overlay);
  }

  private makeLineVao(
    gl: WebGL2RenderingContext,
    vbo: WebGLBuffer | null
  ): WebGLVertexArrayObject | null {
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 24, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 3, gl.FLOAT, false, 24, 12);
    gl.bindVertexArray(null);
    return vao;
  }

  private resize(): void {
    if (!this.gl) return;
    const w = this.overlay.clientWidth || 1;
    const h = this.overlay.clientHeight || 1;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.canvas.width = Math.max(1, Math.round(w * dpr));
    this.canvas.height = Math.max(1, Math.round(h * dpr));
    this.requestRender();
  }

  // ── Instanced bars + reference grid ──────────────────────────────────────────

  private buildBars(rows: number, cols: number): void {
    const gl = this.gl;
    if (!gl) return;
    this.disposeAxes();
    this.rows = rows;
    this.cols = cols;
    this.instanceData = new Float32Array(rows * cols * 6);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceVbo);
    gl.bufferData(gl.ARRAY_BUFFER, this.instanceData.byteLength, gl.DYNAMIC_DRAW);
    this.buildGrid(rows, cols);
    this.frameCamera(rows, cols);
  }

  // Reference grid on the y=0 plane, scaled to the (possibly rectangular)
  // matrix so grid cells align with bar footprints.
  private buildGrid(rows: number, cols: number): void {
    const gl = this.gl;
    if (!gl) return;
    const size = Math.max(rows, cols);
    const sx = cols / size;
    const sz = rows / size;
    const half = size / 2;
    const CENTER = [0x44 / 255, 0x44 / 255, 0x55 / 255];
    const LINE = [0x2a / 255, 0x2a / 255, 0x34 / 255];
    const verts = new Float32Array((size + 1) * 4 * 6);
    let o = 0;
    const push = (x: number, z: number, rgb: number[]): void => {
      verts[o++] = x;
      verts[o++] = 0;
      verts[o++] = z;
      verts[o++] = rgb[0];
      verts[o++] = rgb[1];
      verts[o++] = rgb[2];
    };
    for (let i = 0; i <= size; i++) {
      const k = -half + i;
      const rgb = i === size / 2 ? CENTER : LINE;
      push(-half * sx, k * sz, rgb);
      push(half * sx, k * sz, rgb);
      push(k * sx, -half * sz, rgb);
      push(k * sx, half * sz, rgb);
    }
    gl.bindBuffer(gl.ARRAY_BUFFER, this.gridVbo);
    gl.bufferData(gl.ARRAY_BUFFER, verts, gl.STATIC_DRAW);
    this.gridVertexCount = (size + 1) * 4;
  }

  // ── Axes (index labels on row/col edges + value scale) ─────────────────────

  private disposeAxes(): void {
    const gl = this.gl;
    if (gl) {
      for (const l of this.labels) l.dispose(gl);
    }
    this.labels = [];
    this.yValueLabel = null;
    this.yNegValueLabel = null;
    this.axisVertexCount = 0;
    this.axesBuilt = false;
    this.lastYLabel = '';
  }

  // Row/col index labels along two grid edges (in SOURCE tensor indices, not
  // pooled ones), axis titles, and a value axis at the back-left corner whose
  // top label shows the current frame's max |v| (the full-height bar value).
  private buildAxes(): void {
    this.disposeAxes();
    const gl = this.gl;
    if (!gl || this.rows === 0 || this.cols === 0) return;
    const rows = this.rows;
    const cols = this.cols;
    const srcR = this.axisSrcRows || rows;
    const srcC = this.axisSrcCols || cols;

    const size = Math.max(2, Math.max(rows, cols) * 0.055);
    const margin = Math.max(2.5, Math.max(rows, cols) * 0.07);

    const add = (text: string, x: number, y: number, z: number): LabelTexture => {
      const label = new LabelTexture(gl, text);
      label.w = size * 4;
      label.h = size;
      label.x = x;
      label.y = y;
      label.z = z;
      this.labels.push(label);
      return label;
    };

    const TICKS = 4;
    for (let t = 0; t <= TICKS; t++) {
      // Column indices along the X axis (front edge).
      add(
        String(Math.round((t / TICKS) * (srcC - 1))),
        (t / TICKS) * (cols - 1) - cols / 2,
        0,
        rows / 2 + margin
      );
      // Row indices along the Z axis (left edge).
      add(
        String(Math.round((t / TICKS) * (srcR - 1))),
        -cols / 2 - margin,
        0,
        (t / TICKS) * (rows - 1) - rows / 2
      );
    }

    add('col', 0, 0, rows / 2 + margin * 2.4);
    add('row', -cols / 2 - margin * 2.4, 0, 0);

    // Value axis: vertical line at the back-left corner spanning BOTH
    // directions (negative values render below the plane), labels at ±max
    // and 0 at the plane.
    const cx = -cols / 2 - margin;
    const cz = -rows / 2 - margin;
    const r = 0x66 / 255, g = 0x6a / 255, b = 0x7a / 255;
    const axis = new Float32Array([cx, -HEIGHT_SCALE, cz, r, g, b, cx, HEIGHT_SCALE, cz, r, g, b]);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.axisVbo);
    gl.bufferData(gl.ARRAY_BUFFER, axis, gl.STATIC_DRAW);
    this.axisVertexCount = 2;

    add('0', cx, 0, cz);
    this.yValueLabel = add('', cx, HEIGHT_SCALE + size * 0.9, cz);
    this.yNegValueLabel = add('', cx, -HEIGHT_SCALE - size * 0.9, cz);
    this.axesBuilt = true;
  }

  private updateYLabel(maxAbs: number): void {
    const gl = this.gl;
    if (!gl || !this.yValueLabel || !this.yNegValueLabel) return;
    const mag = maxAbs > 0 ? fmtAxis(maxAbs) : '0';
    if (mag === this.lastYLabel) return;
    this.lastYLabel = mag;
    this.yValueLabel.setText(gl, `+${mag}`);
    this.yNegValueLabel.setText(gl, `−${mag}`);
  }

  private frameCamera(rows: number, cols: number): void {
    if (!this.camera) return;
    const maxDim = Math.max(rows, cols, 4);
    const dist = maxDim * 1.3 + HEIGHT_SCALE * 2;
    // Bars extend both above and below the plane — orbit around y=0.
    this.camera.target = [0, 0, 0];
    this.camera.setFromOffset([dist * 0.55, dist * 0.7, dist * 0.85]);
  }

  private applyFrame(frame: TensorFrame): void {
    // Subsample if a frame somehow exceeds the instance cap.
    let rows = Math.max(1, frame.rows | 0);
    let cols = Math.max(1, frame.cols | 0);
    let stride = 1;
    if (rows * cols > MAX_CELLS) {
      stride = Math.ceil(Math.sqrt((rows * cols) / MAX_CELLS));
      rows = Math.ceil(frame.rows / stride);
      cols = Math.ceil(frame.cols / stride);
    }
    const srcCols = frame.cols | 0;
    const data = frame.data;
    if (data.length < frame.rows * frame.cols) return;

    if (!this.instanceData || rows !== this.rows || cols !== this.cols) {
      this.buildBars(rows, cols);
    }
    // Axes label SOURCE indices — rebuild when the source shape changes (a
    // bars rebuild also clears them via disposeAxes).
    const frameSrcR = frame.srcRows ?? frame.rows;
    const frameSrcC = frame.srcCols ?? frame.cols;
    if (!this.axesBuilt || frameSrcR !== this.axisSrcRows || frameSrcC !== this.axisSrcCols) {
      this.axisSrcRows = frameSrcR;
      this.axisSrcCols = frameSrcC;
      this.buildAxes();
    }
    const gl = this.gl;
    const inst = this.instanceData;
    if (!gl || !inst) return;

    // Normalize by max absolute value across THIS frame (guard divide-by-zero).
    let maxAbs = 0;
    for (let r = 0; r < rows; r++) {
      const sr = r * stride;
      for (let c = 0; c < cols; c++) {
        const v = data[sr * srcCols + c * stride];
        const a = v < 0 ? -v : v;
        if (a > maxAbs && Number.isFinite(a)) maxAbs = a;
      }
    }
    const invMax = maxAbs > 0 ? 1 / maxAbs : 0;
    this.updateYLabel(maxAbs);

    let o = 0;
    for (let r = 0; r < rows; r++) {
      const sr = r * stride;
      for (let c = 0; c < cols; c++) {
        const v = data[sr * srcCols + c * stride];
        const finite = Number.isFinite(v);
        const normAbs = finite ? (v < 0 ? -v : v) * invMax : 0;
        const height = Math.max(MIN_BAR, normAbs * HEIGHT_SCALE);
        inst[o++] = c - cols / 2;
        inst[o++] = r - rows / 2;
        // Two-ended: negative values extend BELOW the y=0 plane.
        inst[o++] = finite && v < 0 ? -height : height;
        // Diverging colormap centered at 0: t = v/maxAbs ∈ [-1,1];
        // t < 0 → white → blue ;  t > 0 → white → red.
        if (invMax === 0 || !finite) {
          inst[o++] = 1;
          inst[o++] = 1;
          inst[o++] = 1;
        } else {
          let t = v * invMax;
          if (t > 1) t = 1;
          else if (t < -1) t = -1;
          if (t >= 0) {
            inst[o++] = 1;
            inst[o++] = 1 - t;
            inst[o++] = 1 - t;
          } else {
            inst[o++] = 1 + t;
            inst[o++] = 1 + t;
            inst[o++] = 1;
          }
        }
      }
    }
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceVbo);
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, inst);
    this.requestRender();
  }

  // ── Render loop (idle-stopping) ──────────────────────────────────────────────

  private requestRender(): void {
    if (!this.visible || !this.gl) return;
    this.needsRender = true;
    if (this.looping) return;
    this.looping = true;
    this.raf = requestAnimationFrame(this.animate);
  }

  private animate = (): void => {
    if (!this.gl || !this.visible) {
      this.looping = false;
      return;
    }
    // camera.update() applies inertial damping and reports whether it is still
    // settling — so the loop keeps ticking until the motion dies down.
    if (this.camera?.update()) this.needsRender = true;
    if (this.needsRender) {
      this.needsRender = false;
      this.draw();
      this.raf = requestAnimationFrame(this.animate);
    } else {
      // Nothing to draw and no motion → stop until the next dirty event.
      this.looping = false;
      this.raf = 0;
    }
  };

  private draw(): void {
    const gl = this.gl;
    if (!gl || !this.camera) return;
    const w = this.canvas.width || 1;
    const h = this.canvas.height || 1;
    gl.viewport(0, 0, w, h);
    gl.clearColor(0, 0, 0, 0);
    gl.enable(gl.DEPTH_TEST);
    gl.depthMask(true);
    gl.disable(gl.BLEND);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);

    mat4Perspective(this.proj, FOV_Y, w / h, 0.1, 4000);
    this.camera.viewMatrix(this.view);

    // Bars (opaque).
    const count = this.rows * this.cols;
    if (count > 0 && this.barsProg) {
      gl.useProgram(this.barsProg);
      gl.uniformMatrix4fv(this.barsU.proj, false, this.proj);
      gl.uniformMatrix4fv(this.barsU.view, false, this.view);
      gl.bindVertexArray(this.boxVao);
      gl.drawElementsInstanced(gl.TRIANGLES, 36, gl.UNSIGNED_SHORT, 0, count);
    }

    // Reference grid + value axis (blended, depth-tested).
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    if (this.gridVertexCount > 0 && this.linesProg) {
      gl.useProgram(this.linesProg);
      gl.uniformMatrix4fv(this.linesU.proj, false, this.proj);
      gl.uniformMatrix4fv(this.linesU.view, false, this.view);
      gl.uniform1f(this.linesU.opacity, 0.35);
      gl.bindVertexArray(this.gridVao);
      gl.drawArrays(gl.LINES, 0, this.gridVertexCount);
      if (this.axisVertexCount > 0) {
        gl.uniform1f(this.linesU.opacity, 0.8);
        gl.bindVertexArray(this.axisVao);
        gl.drawArrays(gl.LINES, 0, this.axisVertexCount);
      }
    }

    // Labels last: billboards, no depth test so they read over the bars.
    gl.disable(gl.DEPTH_TEST);
    if (this.labels.length > 0 && this.labelProg) {
      gl.useProgram(this.labelProg);
      gl.uniformMatrix4fv(this.labelU.proj, false, this.proj);
      gl.uniformMatrix4fv(this.labelU.view, false, this.view);
      gl.uniform1i(this.labelU.tex, 0);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindVertexArray(this.quadVao);
      for (const label of this.labels) {
        gl.uniform3f(this.labelU.center, label.x, label.y, label.z);
        gl.uniform2f(this.labelU.scale, label.w, label.h);
        gl.bindTexture(gl.TEXTURE_2D, label.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      }
    }
    gl.bindVertexArray(null);
  }
}

// Compact numeric formatting for axis / readout labels.
function fmtAxis(v: number): string {
  if (!Number.isFinite(v)) return '?';
  const a = Math.abs(v);
  if (a !== 0 && (a >= 10000 || a < 0.001)) return v.toExponential(1);
  return String(Math.round(v * 1000) / 1000);
}

/** Convenience factory: create (hidden) and return a WeightGrid3D on `parent`. */
export function mountWeightGrid3D(parent: HTMLElement): WeightGrid3D {
  return new WeightGrid3D(parent);
}
