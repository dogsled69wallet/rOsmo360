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
    // width, height, zoom (tan of half the vertical FOV), then the mapping:
    // 0 perspective to 1 little planet, negative for equirect.
    screen: vec4f,
    // rolling-shutter steps, 1 if the surface re-encodes to sRGB, then the
    // view's top-left corner in framebuffer pixels.
    misc: vec4f,
    lenses: array<Lens, 2>,
    // rig_from_view at instants spread over the readout, three rows each.
    views: array<vec4f, 99>,
    // Exposure matching (exposure.rs): the first lens's axis and two ring
    // directions, fade-in angles in .w of the first two; then per lens, per
    // azimuth bin, RGB gains.
    seam: array<vec4f, 3>,
    gains: array<vec4f, 64>,
    // Seam alignment shifts (align.rs), radians, four cells per vector.
    field: array<vec4f, 224>,
    // Disagreement cut strength per azimuth bin (align.rs), four per vector.
    cut: array<vec4f, 32>,
}

const BINS: u32 = 32u;
// Must match align.rs.
const CELLS_PHI: u32 = 128u;
const CELLS_ALPHA: u32 = 7u;
const CELL_ALPHA_LO: f32 = 84.0;
const CELL_ALPHA_HI: f32 = 96.0;
const TAPER_LO: f32 = 83.0;
const TAPER_HI: f32 = 97.0;
const TAPER_WIDTH: f32 = 12.0;

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

// Matches Camera::ray in view.rs. The azimuthal mappings put the ray at θ
// from the view axis where s = sin θ / (d + cos θ): d = 0 is perspective,
// d = 1 the little planet (stereographic), and between them a morph.
fn screen_ray(pos: vec2f) -> vec3f {
    let aspect = u.screen.x / u.screen.y;
    let ndc = vec2f(pos.x / u.screen.x * 2.0 - 1.0, pos.y / u.screen.y * 2.0 - 1.0);
    let d = u.screen.w;
    if d < 0.0 {
        // The whole sphere, flat.
        let lon = ndc.x * PI;
        let lat = -ndc.y * PI / 2.0;
        return vec3f(cos(lat) * sin(lon), -sin(lat), cos(lat) * cos(lon));
    }
    let q = vec2f(ndc.x * aspect, ndc.y) * u.screen.z;
    let s = max(length(q), 1e-9);
    let th = atan(s) + asin(clamp(s * d / sqrt(1.0 + s * s), -1.0, 1.0));
    return vec3f(sin(th) * q / s, cos(th));
}

fn field_cell(x: i32, y: u32) -> f32 {
    let c = min(y, CELLS_ALPHA - 1u) * CELLS_PHI + u32((x + i32(CELLS_PHI)) % i32(CELLS_PHI));
    return u.field[c / 4u][c % 4u];
}

// Matches Aligner::warp in align.rs: where lens `lens` looks for output
// direction `d`.
fn align_warp(lens: u32, d: vec3f) -> vec3f {
    let axis = u.seam[0].xyz;
    let ca = clamp(dot(d, axis), -1.0, 1.0);
    let alpha = acos(ca);
    let a = degrees(alpha);
    let taper = max(1.0 - max(max(TAPER_LO - a, a - TAPER_HI), 0.0) / TAPER_WIDTH, 0.0);
    if taper <= 0.0 {
        return d;
    }
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(CELLS_PHI) - 0.5;
    let y = clamp((a - CELL_ALPHA_LO) / (CELL_ALPHA_HI - CELL_ALPHA_LO) * f32(CELLS_ALPHA - 1u), 0.0, f32(CELLS_ALPHA - 1u));
    let x0 = floor(x);
    let y0 = floor(y);
    let fx = x - x0;
    let fy = y - y0;
    let xi = i32(x0);
    let yi = u32(y0);
    let top = mix(field_cell(xi, yi), field_cell(xi + 1, yi), fx);
    let bot = mix(field_cell(xi, yi + 1u), field_cell(xi + 1, yi + 1u), fx);
    let half = 0.5 * mix(top, bot, fy) * taper;
    let shifted = select(alpha + half, alpha - half, lens == 0u);
    let ring = vec3f(cos(phi) * u.seam[1].xyz + sin(phi) * u.seam[2].xyz);
    return cos(shifted) * axis + sin(shifted) * ring;
}

// Matches stitch::cut_share: where the lenses still disagree after
// alignment, cut to the lens that owns that side of the seam.
const CUT_FROM: f32 = 0.06;
const CUT_FULL: f32 = 0.18;
const CUT_EDGE: f32 = 0.08;

fn cut_bin(i: i32) -> f32 {
    let b = u32((i + i32(CELLS_PHI)) % i32(CELLS_PHI));
    return u.cut[b / 4u][b % 4u];
}

// Matches Field::cut_at: how far the cut may act at direction `d`.
fn cut_strength(d: vec3f) -> f32 {
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(CELLS_PHI) - 0.5;
    let i = floor(x);
    return mix(cut_bin(i32(i)), cut_bin(i32(i) + 1), x - i);
}

fn cut_share(share: f32, a: vec3f, b: vec3f, strength: f32) -> f32 {
    let d = abs(a - b);
    let diff = max(d.x, max(d.y, d.z));
    let cut = smoothstep(0.5 - CUT_EDGE, 0.5 + CUT_EDGE, share);
    return mix(share, cut, smoothstep(CUT_FROM, CUT_FULL, diff) * strength);
}

// Matches Seam::gain in exposure.rs.
fn exposure_gain(lens: u32, d: vec3f, theta: f32) -> vec3f {
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(BINS) - 0.5;
    let i = floor(x);
    let f = x - i;
    let b0 = u32(i32(i) + i32(BINS)) % BINS;
    let b1 = (b0 + 1u) % BINS;
    let g = mix(u.gains[lens * BINS + b0].rgb, u.gains[lens * BINS + b1].rgb, f);
    let fade = smoothstep(u.seam[0].w, u.seam[1].w, theta);
    return mix(vec3f(1.0), g, fade);
}

@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let v = rows(u.user0, u.user1, u.user2, screen_ray(pos.xy - u.misc.zw));
    let steps = u32(u.misc.x);
    let mid = view_at(steps / 2u, v);
    var col = array<vec3f, 2>(vec3f(0.0), vec3f(0.0));
    var wt = array<f32, 2>(0.0, 0.0);
    for (var i = 0u; i < 2u; i++) {
        var h = project(i, align_warp(i, mid));
        if !h.ok {
            continue;
        }
        if steps > 1u {
            let k = u32(round(clamp(h.row, 0.0, 1.0) * f32(steps - 1u)));
            h = project(i, align_warp(i, view_at(k, v)));
            if !h.ok {
                continue;
            }
        }
        let l = u.lenses[i];
        let w = clamp((l.lim.y - h.theta) / max(l.lim.z, 1e-6), 0.0, 1.0);
        if w <= 0.0 {
            continue;
        }
        col[i] = nv12(l.a1.w, h.uv) * exposure_gain(i, mid, h.theta);
        wt[i] = w;
    }
    let wsum = wt[0] + wt[1];
    if wsum <= 0.0 {
        return vec4f(0.0, 0.0, 0.0, 1.0);
    }
    var share = wt[0] / wsum;
    if wt[0] > 0.0 && wt[1] > 0.0 {
        share = cut_share(share, col[0], col[1], cut_strength(mid));
    }
    var rgb = col[0] * share + col[1] * (1.0 - share);
    if u.misc.y > 0.5 {
        // The surface will sRGB-encode; hand it linear light.
        rgb = select(pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb / 12.92, rgb <= vec3f(0.04045));
    }
    return vec4f(rgb, 1.0);
}
