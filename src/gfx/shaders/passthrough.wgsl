// Fullscreen-triangle passthrough: sample the NES framebuffer and blit
// to the swap chain. The NES framebuffer is stored row-by-row with
// scanline 0 at the top; wgpu textures are Y-down, so a direct upload
// plus the standard uv flip in the vertex stage yields an upright image.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOutput {
    // Classic fullscreen triangle — one triangle that covers all of NDC:
    //   vid=0 → (-1,-1)  bottom-left
    //   vid=1 → ( 3,-1)  past the right edge
    //   vid=2 → (-1, 3)  past the top edge
    // Only fragments inside [-1,1]² are rasterized.
    let x = f32(i32(vid & 1u) << 2) - 1.0;
    let y = f32(i32(vid & 2u) << 1) - 1.0;
    // NDC y = 1 is top; texture v = 0 is top. Flip so the image is
    // upright on screen.
    let u = (x + 1.0) * 0.5;
    let v = 1.0 - (y + 1.0) * 0.5;

    var out: VertexOutput;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(u, v);
    return out;
}

@group(0) @binding(0) var t_framebuffer: texture_2d<f32>;
@group(0) @binding(1) var s_framebuffer: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(t_framebuffer, s_framebuffer, in.uv);
}
