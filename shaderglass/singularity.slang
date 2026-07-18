#version 450

// =====================================================================
//  Singularity  -  a slowly drifting black hole for your Windows desktop
//
//  A ShaderGlass (RetroArch .slang) effect. It warps the captured desktop
//  (Source) like light bending around a black hole, and the hole wanders
//  across the screen on its own. No external data, no hooks.
//
//  All the knobs live in the TUNABLES block below. Edit, save, then use
//  Shader > Recent imports in ShaderGlass to reload instantly.
// =====================================================================

layout(push_constant) uniform Push
{
    vec4 SourceSize;    // (w, h, 1/w, 1/h) of the captured desktop
    vec4 OriginalSize;
    vec4 OutputSize;
    uint FrameCount;    // increments every rendered frame -> our clock
} params;

layout(std140, set = 0, binding = 0) uniform UBO
{
    mat4 MVP;
} global;

#pragma stage vertex
layout(location = 0) in vec4 Position;
layout(location = 1) in vec2 TexCoord;
layout(location = 0) out vec2 vTexCoord;

void main()
{
    gl_Position = global.MVP * Position;
    vTexCoord   = TexCoord;
}

#pragma stage fragment
layout(location = 0) in vec2 vTexCoord;
layout(location = 0) out vec4 FragColor;
layout(set = 0, binding = 2) uniform sampler2D Source;

// ----------------------------- TUNABLES ------------------------------
const float HORIZON     = 0.060;  // black-core radius (in screen-heights)
const float LENS        = 1.00;   // lensing strength (0 = off, ~1 = strong)
const float RING_SCALE  = 1.10;   // photon-ring radius = HORIZON * this
const float RING_WIDTH  = 0.010;  // thickness of the glowing photon ring
const float RING_BRIGHT = 1.40;   // photon-ring brightness
const vec3  RING_COLOR  = vec3(1.00, 0.75, 0.45);  // warm glow color

const float DISK_BRIGHT = 0.35;   // accretion-disk glow (set 0.0 to disable)
const float DISK_SPIN   = 3.0;    // how fast the disk swirls

const float DRIFT_SPEED = 0.0009; // how fast the hole wanders (per frame)
const float DRIFT_X     = 0.30;   // horizontal wander amplitude (0..0.5)
const float DRIFT_Y     = 0.20;   // vertical wander amplitude   (0..0.5)
// ---------------------------------------------------------------------

void main()
{
    float aspect = params.SourceSize.x / max(params.SourceSize.y, 1.0);

    // ---- slow self-drift of the hole (Lissajous path, stays on-screen) ----
    float t = mod(float(params.FrameCount), 200000.0) * DRIFT_SPEED;
    vec2  c = vec2(0.5 + DRIFT_X * sin(t * 0.70),
                   0.5 + DRIFT_Y * cos(t * 1.13));

    // ---- aspect-corrected vector from the hole to this pixel ----
    vec2  p   = (vTexCoord - c) * vec2(aspect, 1.0);
    float d   = length(p);
    vec2  dir = (d > 1e-6) ? p / d : vec2(0.0);

    // ---- gravitational lensing: remap how far out we sample the desktop ----
    // Far away  -> warped ~= d  (no distortion, cheap identity fetch).
    // Near HORIZON -> warped shrinks, pulling the background into a ring.
    float k      = (HORIZON * HORIZON) / (d * d);
    float warped = max(d * (1.0 - LENS * k), 0.0);

    vec2 sampleUV = c + (dir * warped) / vec2(aspect, 1.0);
    sampleUV      = clamp(sampleUV, vec2(0.0), vec2(1.0));

    vec3 col = texture(Source, sampleUV).rgb;

    // ---- accretion-disk swirl (colored glow band around the hole) ----
    if (DISK_BRIGHT > 0.0) {
        float ang   = atan(p.y, p.x);
        float band  = smoothstep(HORIZON * 2.40, HORIZON * RING_SCALE, d)
                    * smoothstep(HORIZON * 0.90, HORIZON * RING_SCALE, d);
        float swirl = 0.5 + 0.5 * sin(ang * 2.0 - t * DISK_SPIN + d * 60.0);
        col += RING_COLOR * band * swirl * DISK_BRIGHT;
    }

    // ---- photon ring (Einstein ring glow just outside the horizon) ----
    float rr   = d - HORIZON * RING_SCALE;
    float ring = exp(-(rr * rr) / (2.0 * RING_WIDTH * RING_WIDTH));
    col += RING_COLOR * ring * RING_BRIGHT;

    // ---- event horizon: the black core ----
    float core = smoothstep(HORIZON, HORIZON * 0.94, d); // 1 inside, 0 outside
    col *= (1.0 - core);

    FragColor = vec4(col, 1.0);
}
