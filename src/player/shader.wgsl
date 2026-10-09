// Real-time port of stitch.rs: screen ray -> user view -> stabilised rig ray
// (re-aimed per sensor row) -> both lenses -> feathered blend of NV12 pictures.

struct Lens {
    // cam_from_rig, rows.
    r0: vec4f,
    r1: vec4f,
    r2: vec4f,
    // ThetaPoly k1..k4.
    k: vec4f,
    // k5, p1, p2, skew.
    k5p: vec4f,
    // fx, fy, cx, cy.
    fc: vec4f,
    // Calibration pixels -> texture coordinates: rows (a, b, t); .w of a0 is
    // the mirror flag, .w of a1 the texture slot.
    a0: vec4f,
    a1: vec4f,
    // fov_limit, blend limit, blend width, unused.
    lim: vec4f,
    // calib width, calib height, unused, unused.
    calib: vec4f,
}

struct Uniforms {
    // world_from_screen-ray rotation of the user's free look, rows.
    user0: vec4f,
    user1: vec4f,
    user2: vec4f,
    // width, height, zoom (tan of half the vertical FOV), projection mode.
    screen: vec4f,
    // rolling-shutter steps, 1 if the surface re-encodes to sRGB, unused...
    misc: vec4f,
    lenses: array<Lens, 2>,
    // rig_from_view at instants spread over the readout, three rows each.
    views: array<vec4f, 99>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var y0: texture_2d<f32>;
@group(0) @binding(3) var uv0: texture_2d<f32>;
@group(0) @binding(4) var y1: texture_2d<f32>;
@group(0) @binding(5) var uv1: texture_2d<f32>;
@group(0) @binding(6) var mask0: texture_2d<f32>;
@group(0) @binding(7) var mask1: texture_2d<f32>;

const PI: f32 = 3.14159265358979;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}

fn rows(a: vec4f, b: vec4f, c: vec4f, v: vec3f) -> vec3f {
    return vec3f(dot(a.xyz, v), dot(b.xyz, v), dot(c.xyz, v));
}

fn view_at(i: u32, v: vec3f) -> vec3f {
    return rows(u.views[i * 3u], u.views[i * 3u + 1u], u.views[i * 3u + 2u], v);
}

struct Hit {
    // Texture coordinates, then calibration-space row fraction.
    uv: vec2f,
    row: f32,
    theta: f32,
    ok: bool,
}

fn mask_at(i: u32, p: vec2i) -> f32 {
    if i == 0u {
        return textureLoad(mask0, p, 0).r;
    }
    return textureLoad(mask1, p, 0).r;
}

fn project(i: u32, d: vec3f) -> Hit {
    let l = u.lenses[i];
    var h: Hit;
    h.ok = false;
    let c = rows(l.r0, l.r1, l.r2, d);
    let rho = length(c.xy);
    let theta = atan2(rho, c.z);
    h.theta = theta;
    if theta > l.lim.x {
        return h;
    }
    let t2 = theta * theta;
    let td = theta * (1.0 + t2 * (l.k.x + t2 * (l.k.y + t2 * (l.k.z + t2 * (l.k.w + t2 * l.k5p.x)))));
    var x = 0.0;
    var y = 0.0;
    if rho > 1e-9 {
        x = td * c.x / rho;
        y = td * c.y / rho;
    }
    let td2 = td * td;
    let p1 = l.k5p.y;
    let p2 = l.k5p.z;
    let xd = x + 2.0 * p1 * x * y + p2 * (td2 + 2.0 * x * x);
    let yd = y + p1 * (td2 + 2.0 * y * y) + 2.0 * p2 * x * y;
    var cu = l.fc.x * xd + l.k5p.w * yd + l.fc.z;
    let cv = l.fc.y * yd + l.fc.w;
    if l.a0.w > 0.5 {
        cu = 2.0 * l.fc.z - cu;
    }
    if cu < 0.0 || cv < 0.0 || cu >= l.calib.x || cv >= l.calib.y {
        return h;
    }
    if mask_at(i, vec2i(i32(cu), i32(cv))) < 0.5 {
        return h;
    }
    h.uv = vec2f(dot(l.a0.xy, vec2f(cu, cv)) + l.a0.z, dot(l.a1.xy, vec2f(cu, cv)) + l.a1.z);
    h.row = cv / l.calib.y;
    h.ok = true;
    return h;
}

fn nv12(slot: f32, uv: vec2f) -> vec3f {
    var yy: f32;
    var c: vec2f;
    if slot < 0.5 {
        yy = textureSampleLevel(y0, samp, uv, 0.0).r;
        c = textureSampleLevel(uv0, samp, uv, 0.0).rg;
    } else {
        yy = textureSampleLevel(y1, samp, uv, 0.0).r;
        c = textureSampleLevel(uv1, samp, uv, 0.0).rg;
    }
    // BT.709, limited range.
    let Y = (yy - 16.0 / 255.0) * (255.0 / 219.0);
    let U = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    let V = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    return clamp(vec3f(Y + 1.5748 * V, Y - 0.1873 * U - 0.4681 * V, Y + 1.8556 * U), vec3f(0.0), vec3f(1.0));
}

fn screen_ray(pos: vec2f) -> vec3f {
    let aspect = u.screen.x / u.screen.y;
    let ndc = vec2f(pos.x / u.screen.x * 2.0 - 1.0, pos.y / u.screen.y * 2.0 - 1.0);
    let mode = u32(u.screen.w);
    if mode == 1u {
        // Little planet: stereographic around the view axis.
        let p = vec2f(ndc.x * aspect, ndc.y) * u.screen.z * 2.0;
        let r = max(length(p), 1e-9);
        let th = 2.0 * atan(r / 2.0);
        return vec3f(sin(th) * p / r, cos(th));
    }
    if mode == 2u {
        // The whole sphere, flat.
        let lon = ndc.x * PI;
        let lat = -ndc.y * PI / 2.0;
        return vec3f(cos(lat) * sin(lon), -sin(lat), cos(lat) * cos(lon));
    }
    return normalize(vec3f(ndc.x * aspect * u.screen.z, ndc.y * u.screen.z, 1.0));
}

@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let v = rows(u.user0, u.user1, u.user2, screen_ray(pos.xy));
    let steps = u32(u.misc.x);
    let mid = view_at(steps / 2u, v);
    var acc = vec3f(0.0);
    var wsum = 0.0;
    for (var i = 0u; i < 2u; i++) {
        var h = project(i, mid);
        if !h.ok {
            continue;
        }
        if steps > 1u {
            let k = u32(round(clamp(h.row, 0.0, 1.0) * f32(steps - 1u)));
            h = project(i, view_at(k, v));
            if !h.ok {
                continue;
            }
        }
        let l = u.lenses[i];
        let w = clamp((l.lim.y - h.theta) / max(l.lim.z, 1e-6), 0.0, 1.0);
        if w <= 0.0 {
            continue;
        }
        acc += nv12(l.a1.w, h.uv) * w;
        wsum += w;
    }
    if wsum <= 0.0 {
        return vec4f(0.0, 0.0, 0.0, 1.0);
    }
    var rgb = acc / wsum;
    if u.misc.y > 0.5 {
        // The surface will sRGB-encode; hand it linear light.
        rgb = select(pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb / 12.92, rgb <= vec3f(0.04045));
    }
    return vec4f(rgb, 1.0);
}
