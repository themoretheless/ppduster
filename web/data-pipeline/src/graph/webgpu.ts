export interface SurfaceEdge {
  fromX: number;
  fromY: number;
  toX: number;
  toY: number;
  color: [number, number, number, number];
}

export interface SurfaceScene {
  panX: number;
  panY: number;
  zoom: number;
  edges: SurfaceEdge[];
  theme: "light" | "dark";
}

type GpuState = {
  device: any;
  context: any;
  pipeline: any;
  format: string;
};

const SHADER = `
struct VertexOutput {
  @builtin(position) position: vec4f,
  @location(0) color: vec4f,
};

@vertex
fn vertex_main(
  @location(0) position: vec2f,
  @location(1) color: vec4f,
) -> VertexOutput {
  var output: VertexOutput;
  output.position = vec4f(position, 0.0, 1.0);
  output.color = color;
  return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4f {
  return input.color;
}
`;

export class FlowSurfaceRenderer {
  private gpu: GpuState | null = null;
  private fallback: CanvasRenderingContext2D | null = null;
  private disposed = false;

  constructor(private readonly canvas: HTMLCanvasElement) {}

  async init(): Promise<"webgpu" | "canvas"> {
    const gpuApi = (navigator as Navigator & { gpu?: any }).gpu;
    if (gpuApi) {
      try {
        const adapter = await gpuApi.requestAdapter({ powerPreference: "high-performance" });
        const device = await adapter?.requestDevice();
        const context = this.canvas.getContext("webgpu") as any;
        if (device && context) {
          const format = gpuApi.getPreferredCanvasFormat();
          context.configure({ device, format, alphaMode: "opaque" });
          const module = device.createShaderModule({ code: SHADER });
          const pipeline = device.createRenderPipeline({
            layout: "auto",
            vertex: {
              module,
              entryPoint: "vertex_main",
              buffers: [
                {
                  arrayStride: 24,
                  attributes: [
                    { shaderLocation: 0, offset: 0, format: "float32x2" },
                    { shaderLocation: 1, offset: 8, format: "float32x4" },
                  ],
                },
              ],
            },
            fragment: {
              module,
              entryPoint: "fragment_main",
              targets: [
                {
                  format,
                  blend: {
                    color: { srcFactor: "src-alpha", dstFactor: "one-minus-src-alpha" },
                    alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
                  },
                },
              ],
            },
            primitive: { topology: "line-list" },
          });
          this.gpu = { device, context, pipeline, format };
          device.lost.then(() => {
            if (!this.disposed) this.activateFallback();
          });
          return "webgpu";
        }
      } catch {
        // A software fallback keeps the editor usable when WebGPU is unavailable.
      }
    }

    this.activateFallback();
    return "canvas";
  }

  render(scene: SurfaceScene) {
    if (this.disposed) return;
    this.resize();
    if (this.gpu) this.renderGpu(scene);
    else this.renderFallback(scene);
  }

  destroy() {
    this.disposed = true;
    this.gpu?.device?.destroy?.();
    this.gpu = null;
    this.fallback = null;
  }

  private activateFallback() {
    this.gpu = null;
    this.fallback = this.canvas.getContext("2d");
  }

  private resize() {
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    const width = Math.max(1, Math.floor(this.canvas.clientWidth * ratio));
    const height = Math.max(1, Math.floor(this.canvas.clientHeight * ratio));
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
      if (this.gpu) {
        this.gpu.context.configure({
          device: this.gpu.device,
          format: this.gpu.format,
          alphaMode: "opaque",
        });
      }
    }
  }

  private renderGpu(scene: SurfaceScene) {
    if (!this.gpu) return;
    const vertices = this.buildVertices(scene);
    const { device, context, pipeline } = this.gpu;
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view: context.getCurrentTexture().createView(),
          clearValue: scene.theme === "dark"
            ? { r: 0.055, g: 0.067, b: 0.078, a: 1 }
            : { r: 0.956, g: 0.952, b: 0.929, a: 1 },
          loadOp: "clear",
          storeOp: "store",
        },
      ],
    });

    pass.setPipeline(pipeline);
    if (vertices.length) {
      const buffer = device.createBuffer({
        size: Math.max(4, vertices.byteLength),
        usage: 0x20 | 0x08,
        mappedAtCreation: true,
      });
      new Float32Array(buffer.getMappedRange()).set(vertices);
      buffer.unmap();
      pass.setVertexBuffer(0, buffer);
      pass.draw(vertices.length / 6);
    }
    pass.end();
    device.queue.submit([encoder.finish()]);
  }

  private buildVertices(scene: SurfaceScene) {
    const width = this.canvas.clientWidth;
    const height = this.canvas.clientHeight;
    const values: number[] = [];
    const toNdcX = (value: number) => (value / width) * 2 - 1;
    const toNdcY = (value: number) => 1 - (value / height) * 2;
    const addLine = (
      x1: number,
      y1: number,
      x2: number,
      y2: number,
      color: [number, number, number, number],
    ) => {
      values.push(toNdcX(x1), toNdcY(y1), ...color, toNdcX(x2), toNdcY(y2), ...color);
    };

    const spacing = 32 * scene.zoom;
    const startX = ((scene.panX % spacing) + spacing) % spacing;
    const startY = ((scene.panY % spacing) + spacing) % spacing;
    for (let x = startX, index = 0; x < width; x += spacing, index += 1) {
      const major = Math.round((x - scene.panX) / spacing) % 4 === 0;
      const grid: [number, number, number] = scene.theme === "dark" ? [0.55, 0.58, 0.63] : [0.42, 0.43, 0.39];
      addLine(x, 0, x, height, [...grid, major ? 0.17 : 0.075]);
    }
    for (let y = startY, index = 0; y < height; y += spacing, index += 1) {
      const major = Math.round((y - scene.panY) / spacing) % 4 === 0;
      const grid: [number, number, number] = scene.theme === "dark" ? [0.55, 0.58, 0.63] : [0.42, 0.43, 0.39];
      addLine(0, y, width, y, [...grid, major ? 0.17 : 0.075]);
    }

    for (const edge of scene.edges) {
      const distance = Math.max(80, Math.abs(edge.toX - edge.fromX) * 0.52);
      let previousX = edge.fromX;
      let previousY = edge.fromY;
      for (let step = 1; step <= 28; step += 1) {
        const t = step / 28;
        const inverse = 1 - t;
        const nextX =
          inverse ** 3 * edge.fromX +
          3 * inverse ** 2 * t * (edge.fromX + distance) +
          3 * inverse * t ** 2 * (edge.toX - distance) +
          t ** 3 * edge.toX;
        const nextY = inverse ** 3 * edge.fromY + 3 * inverse ** 2 * t * edge.fromY + 3 * inverse * t ** 2 * edge.toY + t ** 3 * edge.toY;
        addLine(previousX, previousY, nextX, nextY, edge.color);
        addLine(previousX, previousY + 1, nextX, nextY + 1, [edge.color[0], edge.color[1], edge.color[2], edge.color[3] * 0.45]);
        previousX = nextX;
        previousY = nextY;
      }
    }

    return new Float32Array(values);
  }

  private renderFallback(scene: SurfaceScene) {
    const context = this.fallback;
    if (!context) return;
    const ratio = this.canvas.width / Math.max(1, this.canvas.clientWidth);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    const width = this.canvas.clientWidth;
    const height = this.canvas.clientHeight;
    context.fillStyle = scene.theme === "dark" ? "#0e1114" : "#f4f3ed";
    context.fillRect(0, 0, width, height);

    const spacing = 32 * scene.zoom;
    const startX = ((scene.panX % spacing) + spacing) % spacing;
    const startY = ((scene.panY % spacing) + spacing) % spacing;
    context.lineWidth = 1;
    for (let x = startX; x < width; x += spacing) {
      context.strokeStyle = scene.theme === "dark" ? "rgba(151, 158, 171, .11)" : "rgba(86, 91, 84, .11)";
      context.beginPath();
      context.moveTo(x, 0);
      context.lineTo(x, height);
      context.stroke();
    }
    for (let y = startY; y < height; y += spacing) {
      context.strokeStyle = scene.theme === "dark" ? "rgba(151, 158, 171, .11)" : "rgba(86, 91, 84, .11)";
      context.beginPath();
      context.moveTo(0, y);
      context.lineTo(width, y);
      context.stroke();
    }

    for (const edge of scene.edges) {
      const distance = Math.max(80, Math.abs(edge.toX - edge.fromX) * 0.52);
      const [r, g, b, a] = edge.color;
      context.strokeStyle = `rgba(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)}, ${a})`;
      context.lineWidth = 2.5;
      context.beginPath();
      context.moveTo(edge.fromX, edge.fromY);
      context.bezierCurveTo(edge.fromX + distance, edge.fromY, edge.toX - distance, edge.toY, edge.toX, edge.toY);
      context.stroke();
    }
  }
}
