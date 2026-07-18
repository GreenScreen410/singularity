// Singularity - a geodesic-traced black hole over the live desktop.
//
// Faithful WGSL port of s0xDk/ghostty-blackhole (MIT), itself after Eric
// Bruneton's "Real-time High-Quality Rendering of Non-Rotating Black Holes".
// Each pixel's null geodesic is integrated numerically in 3D - the Binet-form
// photon acceleration  a = -(3/2) h² x / r⁵  reproduces exact Schwarzschild
// bending. Everything falls out of that integration:
//
//   * shadow        - rays under b_crit = (3√3/2) r_s spiral into the horizon
//   * lensing       - escaped rays are projected back onto the desktop "sky"
//                     plane: your screen bends, magnifies, mirrors in the ring
//   * photon ring   - rays winding near the r = 1.5 r_s photon sphere
//   * accretion disk -  a thin tilted Keplerian disk the ray may cross several
//                     times (the far side arcs over and under the shadow);
//                     blackbody color from a Shakura-Sunyaev temperature
//                     profile, Doppler-shifted and beamed
//
// The terminal-specific modes (token/pomodoro/cursor decode) are replaced by
// a slow self-drift; the desktop capture plays the role of the lensed sky.

// The disk "look" rides in the uniform buffer so the tray menu can switch
// presets live (values crossfaded on the CPU side).
struct Uniforms {
    resolution: vec2<f32>,
    time: f32,
    has_desktop: f32,
    temp: f32,          // hottest-annulus temperature, Kelvin
    incl: f32,          // disk inclination, rad (0 face-on, 1.57 edge-on)
    roll: f32,          // screen-plane rotation of the system
    inner: f32,         // disk inner edge, r_s
    outer: f32,         // disk outer edge, r_s
    opac: f32,          // near-disk opacity toward the background
    dopp: f32,          // Doppler mix (0 none, 1 full)
    beam: f32,          // beaming exponent
    gain: f32,          // disk emission brightness
    contr: f32,         // streak contrast
    wind: f32,          // spiral winding tightness
    speed: f32,         // streak speed; negative reverses orbit
    expo: f32,          // tonemap exposure
    star: f32,          // lensed starfield gain
    hole_radius: f32,   // shadow radius, fraction of screen height
    drift_speed: f32,   // wander speed multiplier
    drift_x: f32,       // horizontal wander amplitude (0..0.5)
    drift_y: f32,       // vertical wander amplitude   (0..0.5)
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var desktop_tex: texture_2d<f32>;
@group(0) @binding(2) var desktop_samp: sampler;

struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VSOut {
    var verts = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = verts[vid];
    var out: VSOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    out.uv  = vec2<f32>(xy.x * 0.5 + 0.5, 1.0 - (xy.y * 0.5 + 0.5));
    return out;
}

// ---------------------------------------------------------------- tunables --
// Disk look, size and drift come from the uniforms (tray menu / config file);
// only the hole-independent knobs stay compile-time.
const LENS_DEPTH: f32    = 13.0;   // hole-to-sky-plane distance in r_s - bigger = bends harder
const N_STEPS: i32       = 48;     // geodesic steps per pixel (perf dial)
const B_CRIT: f32        = 2.5980762; // critical impact parameter, r_s

// ------------------------------------------------------------------- helpers --
fn gmod(x: f32, y: f32) -> f32 { return x - y * floor(x / y); }

fn mirrorUV(uvin: vec2<f32>) -> vec2<f32> {
    let m = uvin - 2.0 * floor(uvin / 2.0);
    return 1.0 - abs(1.0 - m);
}

fn rot(v: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * v.x - s * v.y, s * v.x + c * v.y);
}

// unit Lissajous wander: incommensurate sines, never visibly repeats
fn lissa(t: f32) -> vec2<f32> {
    return vec2<f32>(0.75 * sin(t * 0.37) + 0.25 * sin(t * 0.83 + 1.0),
                     0.70 * sin(t * 0.54 + 2.1) + 0.30 * sin(t * 1.07));
}

fn hash21(pin: vec2<f32>) -> f32 {
    var p = fract(pin * vec2<f32>(234.34, 435.345));
    p = p + dot(p, p + 34.23);
    return fract(p.x * p.y);
}

// value noise whose y lattice wraps every perY cells - the disk's angular
// dimension, so streaks tile seamlessly across the atan branch cut
fn vnoiseWrapY(p: vec2<f32>, perY: f32) -> f32 {
    let i = floor(p);
    var f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    let y0 = gmod(i.y, perY);
    let y1 = gmod(i.y + 1.0, perY);
    return mix(mix(hash21(vec2<f32>(i.x, y0)),       hash21(vec2<f32>(i.x + 1.0, y0)), f.x),
               mix(hash21(vec2<f32>(i.x, y1)),       hash21(vec2<f32>(i.x + 1.0, y1)), f.x),
               f.y);
}

// blackbody color from temperature in Kelvin (Tanner Helland fit, normalized)
fn blackbody(T: f32) -> vec3<f32> {
    let t = clamp(T, 1500.0, 40000.0) / 100.0;
    var r = 1.0;
    if (t > 66.0) { r = clamp(1.292936 * pow(t - 60.0, -0.1332047), 0.0, 1.0); }
    var g = 0.0;
    if (t <= 66.0) { g = clamp(0.3900816 * log(t) - 0.6318414, 0.0, 1.0); }
    else           { g = clamp(1.1298909 * pow(t - 60.0, -0.0755148), 0.0, 1.0); }
    var b = 1.0;
    if (t < 66.0) {
        if (t <= 19.0) { b = 0.0; }
        else { b = clamp(0.5432068 * log(t - 10.0) - 1.1962540, 0.0, 1.0); }
    }
    return vec3<f32>(r, g, b);
}

// sparse procedural starfield indexed by (bent) ray direction
fn stars(d: vec3<f32>) -> vec3<f32> {
    let sph = vec2<f32>(atan2(d.x, -d.z), asin(clamp(d.y, -1.0, 1.0)));
    let g   = sph * 40.0;
    let id  = floor(g);
    let h   = hash21(id);
    if (h < 0.92) { return vec3<f32>(0.0); }
    let f   = fract(g) - 0.5;
    let off = (vec2<f32>(hash21(id + 17.3), hash21(id + 31.7)) - 0.5) * 0.7;
    let spark = smoothstep(0.10, 0.0, length(f - off));
    let tw    = 0.7 + 0.3 * sin(u.time * (0.5 + 2.0 * hash21(id + 5.1)) + 40.0 * h);
    let tint  = mix(vec3<f32>(1.0, 0.82, 0.60), vec3<f32>(0.75, 0.85, 1.0), hash21(id + 2.9));
    return tint * spark * tw * ((h - 0.92) / 0.08);
}

// live desktop when available; procedural test pattern otherwise
fn background(uvin: vec2<f32>) -> vec3<f32> {
    let uvm = mirrorUV(uvin);
    if (u.has_desktop > 0.5) {
        return textureSampleLevel(desktop_tex, desktop_samp, uvm, 0.0).rgb;
    }
    let grad = vec3<f32>(uvm.x, uvm.y, 0.6);
    let cx = step(0.5, fract(uvm.x * 16.0));
    let cy = step(0.5, fract(uvm.y * 9.0));
    let c  = abs(cx - cy);
    let checker = mix(vec3<f32>(0.05, 0.06, 0.09), vec3<f32>(0.14, 0.16, 0.22), c);
    return mix(checker, grad, 0.35);
}

// ---------------------------------------------------------------------- main --
@fragment
fn fs_main(in: VSOut) -> @location(0) vec4<f32> {
    let uv     = in.uv;
    let aspect = u.resolution.x / max(u.resolution.y, 1.0);
    let t      = u.time;

    let rin  = max(u.inner, 1.6);
    let rout = max(u.outer, rin + 0.5);
    let rh   = u.hole_radius;            // shadow radius in screen units

    // slow self-drift (Lissajous, never repeats)
    let center = vec2<f32>(0.5, 0.5)
               + lissa(t * 0.12 * u.drift_speed) * vec2<f32>(u.drift_x, u.drift_y);

    // aspect-corrected frame centered on the hole (y in screen heights)
    let p    = (uv - center) * vec2<f32>(aspect, 1.0);
    let plen = length(p);

    // screen <-> world: shadow's angular size is B_CRIT r_s at rh screen units
    let W  = B_CRIT / max(rh, 1e-4);
    let pr = rot(vec2<f32>(p.x, -p.y), u.roll) * W;
    let b  = length(pr);                 // impact parameter, r_s units

    // fade real 1/b lensing a few disk diameters out so the whole screen
    // doesn't shimmer as the hole drifts (deliberately unphysical)
    let window = exp(-pow(plen / (7.0 * rh), 2.0));

    let bmax = rout + 3.0;               // rays beyond this can't touch the disk
    let Z0   = max(14.0, rout + 5.0);    // camera distance (shared with tracer)

    // ================= far field: analytic weak deflection ==================
    // finite-camera fitted mapping - sub-1% displacement match at the handoff
    // circle, so there is no visible seam against the integrated region
    if (b >= bmax) {
        let uu   = Z0 * inverseSqrt(Z0 * Z0 + b * b);
        let defl = (2.0 / (W * W)) / max(plen, 1e-4)
                 * (1.29 * uu + 0.07) * max(LENS_DEPTH - 2.14 * uu + 0.75, 0.0)
                 * window;
        let dir = p / max(plen, 1e-5);
        // mild chromatic aberration: blue bends a touch more than red
        let ab = 0.035 * smoothstep(1.0, 2.0, b / bmax);
        let sp_r = p - dir * defl * (1.0 - ab);
        let sp_g = p - dir * defl;
        let sp_b = p - dir * defl * (1.0 + ab);
        let col = vec3<f32>(
            background(center + sp_r / vec2<f32>(aspect, 1.0)).r,
            background(center + sp_g / vec2<f32>(aspect, 1.0)).g,
            background(center + sp_b / vec2<f32>(aspect, 1.0)).b,
        );
        let d = normalize(vec3<f32>(-(pr / b) * (2.0 / b), -1.0));
        return vec4<f32>(col + stars(d) * u.star * window, 1.0);
    }

    // ====================== near field: trace the geodesic ==================
    // Parallel rays from a distant camera at +z; hole at origin, r_s = 1.
    var x = vec3<f32>(pr, Z0);
    var v = vec3<f32>(0.0, 0.0, -1.0);
    let h2 = dot(pr, pr);                // conserved angular momentum²

    // disk plane: normal tilted DISK_INCL about the screen x-axis
    let ci = cos(u.incl);
    let si = sin(u.incl);
    let n  = vec3<f32>(0.0, si, ci);
    let e2 = vec3<f32>(0.0, ci, -si);    // in-plane axis completing (x̂, e2, n)
    let sdir = select(1.0, -1.0, u.speed < 0.0);
    let spd  = abs(u.speed);

    var emitc = vec3<f32>(0.0);          // accumulated disk light (HDR)
    var trans = 1.0;                     // transmittance toward the background
    var captured = false;
    var sPrev = dot(x, n);
    var xPrev = x;

    for (var i: i32 = 0; i < N_STEPS; i = i + 1) {
        var r2 = dot(x, x);
        if (r2 < 1.0) { captured = true; break; }         // through the horizon
        if (x.z < -Z0 && v.z < 0.0) { break; }            // escaped out the back
        if (r2 > 4.0 * Z0 * Z0) { break; }                // flung far sideways
        var r = sqrt(r2);
        // step scales with radius: fine near the photon sphere, coarse far out
        let dt = clamp(0.16 * r, 0.03, 1.5);
        // leapfrog (kick-drift-kick) keeps near-critical orbits stable
        var a = -1.5 * h2 * x / (r2 * r2 * r);
        v = v + a * (0.5 * dt);
        x = x + v * dt;
        r2 = dot(x, x);
        r  = sqrt(r2);
        a  = -1.5 * h2 * x / (r2 * r2 * r);
        v = v + a * (0.5 * dt);

        // ---- thin-disk crossing: the ray pierced the disk plane ----
        let s = dot(x, n);
        if (s * sPrev < 0.0 && trans > 0.02) {
            let tc = sPrev / (sPrev - s);
            let xc = mix(xPrev, x, tc);
            let rc = length(xc);
            if (rc > rin && rc < rout) {
                let band = smoothstep(rin, rin * 1.25, rc)
                         * (1.0 - smoothstep(rout * 0.70, rout, rc));

                // disk-plane polar coords for the streak texture
                let phi   = atan2(dot(xc, e2), xc.x);
                let turns = phi / 6.2831853;
                let kep   = pow(rin / rc, 1.5);
                // √(1 − 1.5/r): time runs slower for the inner orbits
                let gloc  = sqrt(max(1.0 - 1.5 / rc, 0.02));
                let swirl = rc * u.wind * 0.12 - t * kep * spd * gloc * sdir;
                var streaks = vnoiseWrapY(vec2<f32>(rc * 2.8, turns * 19.0 + swirl * 3.0), 19.0) * 0.65
                            + vnoiseWrapY(vec2<f32>(rc * 1.0, turns * 9.0  + swirl * 1.5 + 7.0), 9.0) * 0.35;
                streaks = 0.35 + u.contr * streaks * streaks;

                // relativistic Doppler + gravitational shift for circular-orbit
                // gas: g = √(1 − 1.5/r) / (1 − β·k̂)
                let gasdir = normalize(cross(n, xc)) * sdir;
                let beta   = clamp(inverseSqrt(max(2.0 * (rc - 1.0), 0.2)), 0.0, 0.99);
                var g = gloc / max(1.0 + beta * dot(gasdir, normalize(v)), 0.05);
                g = mix(1.0, g, u.dopp);

                // Shakura-Sunyaev temperature profile, peak normalized to 1
                let xpr   = max(1.0 - sqrt(rin / rc), 0.0);
                let tprof = pow(rin / rc, 0.75) * pow(xpr, 0.25) / 0.488;
                let cbb   = blackbody(u.temp * tprof * g);      // shifted color
                let boost = pow(g, u.beam);                     // beaming

                let density = band * streaks;
                emitc = emitc + trans * cbb * (u.gain * 2.2 * density * tprof * tprof * boost);
                trans = trans * (1.0 - clamp(u.opac * density, 0.0, 1.0));
            }
        }
        sPrev = s;
        xPrev = x;
    }
    // rays still wound up near the photon sphere when the budget ran out
    if (!captured && dot(x, x) < 4.0) { captured = true; }

    // ---- background: where did the escaped ray come from? ----
    var bg = vec3<f32>(0.0);
    if (!captured) {
        let d = normalize(v);
        bg = bg + stars(d) * u.star * window;
        if (d.z < -0.05) {
            // project the exit ray onto the desktop sky plane at z = -LENS_DEPTH
            let tpl = (-LENS_DEPTH - x.z) / d.z;
            let hp  = x + d * tpl;
            let q   = rot(hp.xy, -u.roll) / W;
            let sp  = vec2<f32>(q.x, -q.y);
            // fade the *displacement*, never the color - no seam anywhere
            let suv = center + (p + (sp - p) * window) / vec2<f32>(aspect, 1.0);
            // rays bent past ~90° never reach the sky plane; fade them out
            let toward = smoothstep(0.05, 0.35, -d.z);
            bg = bg + background(suv) * toward;
        }
    }

    // disk light is HDR; tonemap it on top of the (untouched) desktop sample
    let col = bg * trans + (vec3<f32>(1.0) - exp(-emitc * u.expo));
    return vec4<f32>(col, 1.0);
}
