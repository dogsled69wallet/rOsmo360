// Shared by the stitch (shader.wgsl) and the overlays (overlay.wgsl), each
// source having this appended: `Grade::uniforms`' three rows in g0..g2.

// Mirrors `Grade::apply` in grade.rs: exposure and white balance on light,
// tone and colour on display values, then the vignette, opacity and fade.
fn grade_by(g0: vec4f, g1: vec4f, g2: vec4f, rgb: vec3f, at: vec2f, aspect: f32) -> vec3f {
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    let wb = vec3f(exp2(g0.y), exp2(-g0.z), exp2(-g0.y));
    var c = pow(pow(max(rgb, vec3f(0.0)), vec3f(2.2)) * g0.x * wb, vec3f(1.0 / 2.2));
    var l = dot(c, luma);
    let lift = 0.25 * (g1.y * (1.0 - smoothstep(0.0, 0.6, l)) + g1.x * smoothstep(0.4, 1.0, l));
    if l > 1e-4 {
        c = c * max((l + lift) / l, 0.0);
    } else {
        c = max(c + vec3f(lift), vec3f(0.0));
    }
    c = (c - vec3f(0.5)) * g0.w + vec3f(0.5);
    l = dot(c, luma);
    let spread = max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b);
    let s = (1.0 + g1.z) * (1.0 + g1.w * (1.0 - clamp(spread, 0.0, 1.0)));
    c = vec3f(l) + (c - vec3f(l)) * s;
    let r = length(vec2f(at.x * aspect, at.y)) / sqrt(aspect * aspect + 1.0);
    let keep = (1.0 - g2.x * smoothstep(0.3, 1.0, r)) * g2.w * (1.0 - g2.y);
    return clamp(c * keep, vec3f(0.0), vec3f(1.0));
}
