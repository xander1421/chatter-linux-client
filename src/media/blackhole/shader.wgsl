// Gargantua-style Black Hole Shader
// Inspired by the Interstellar movie's visualization
//
// Features:
// - Gravitational lensing via raymarching
// - Glowing accretion disk with temperature gradient
// - Star field background
// - Doppler beaming effects

struct Uniforms {
    time: f32,
}

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// Proper fullscreen triangle vertex shader
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    // Generate fullscreen triangle vertices
    // vertex 0: (-1, -1), vertex 1: (3, -1), vertex 2: (-1, 3)
    let x = f32(i32(vertex_index & 1u) * 4 - 1);
    let y = f32(i32(vertex_index >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // UV coordinates: [0, 1] range
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

// ============================================================================
// Constants
// ============================================================================

const PI: f32 = 3.14159265359;
const SCHWARZSCHILD_RADIUS: f32 = 1.0;
const DISK_INNER: f32 = 2.5;
const DISK_OUTER: f32 = 12.0;
const RAY_STEPS: i32 = 128;
const STEP_SIZE: f32 = 0.15;

// ============================================================================
// Noise functions
// ============================================================================

fn hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn hash3(p: vec3<f32>) -> f32 {
    var p3 = fract(p * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn noise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);

    return mix(
        mix(
            mix(hash3(i), hash3(i + vec3<f32>(1.0, 0.0, 0.0)), u.x),
            mix(hash3(i + vec3<f32>(0.0, 1.0, 0.0)), hash3(i + vec3<f32>(1.0, 1.0, 0.0)), u.x),
            u.y
        ),
        mix(
            mix(hash3(i + vec3<f32>(0.0, 0.0, 1.0)), hash3(i + vec3<f32>(1.0, 0.0, 1.0)), u.x),
            mix(hash3(i + vec3<f32>(0.0, 1.0, 1.0)), hash3(i + vec3<f32>(1.0, 1.0, 1.0)), u.x),
            u.y
        ),
        u.z
    );
}

// ============================================================================
// Star field - simple scattered stars on black background
// ============================================================================

fn stars(dir: vec3<f32>) -> vec3<f32> {
    // Spherical coordinates for star placement
    let theta = atan2(dir.z, dir.x);
    let phi = asin(clamp(dir.y, -1.0, 1.0));

    var brightness = 0.0;

    // Layer 1: Dense dim stars
    let grid1 = vec2<f32>(theta * 15.0, phi * 15.0);
    let cell1 = floor(grid1);
    let h1 = hash(cell1);
    if h1 > 0.96 {
        let d = length(fract(grid1) - 0.5);
        brightness += smoothstep(0.1, 0.0, d) * 0.5;
    }

    // Layer 2: Medium stars
    let grid2 = vec2<f32>(theta * 8.0, phi * 8.0);
    let cell2 = floor(grid2);
    let h2 = hash(cell2 + 100.0);
    if h2 > 0.97 {
        let d = length(fract(grid2) - 0.5);
        brightness += smoothstep(0.15, 0.0, d) * 0.8;
    }

    // Layer 3: Bright stars
    let grid3 = vec2<f32>(theta * 4.0, phi * 4.0);
    let cell3 = floor(grid3);
    let h3 = hash(cell3 + 200.0);
    if h3 > 0.98 {
        let d = length(fract(grid3) - 0.5);
        brightness += smoothstep(0.2, 0.0, d) * 1.5;
    }

    // Slight color variation
    let color_seed = hash(floor(vec2<f32>(theta * 10.0, phi * 10.0)) + 500.0);
    var star_color = vec3<f32>(1.0);
    if color_seed < 0.3 {
        star_color = vec3<f32>(0.8, 0.85, 1.0); // Blue tint
    } else if color_seed > 0.7 {
        star_color = vec3<f32>(1.0, 0.95, 0.85); // Yellow tint
    }

    return star_color * brightness;
}

// ============================================================================
// Accretion disk color - Interstellar style orange/white glow
// ============================================================================

fn disk_color(r: f32, angle: f32, hit_pos: vec3<f32>, ray_dir: vec3<f32>, time: f32) -> vec3<f32> {
    // Normalized radius (0 at inner edge, 1 at outer edge)
    let r_norm = (r - DISK_INNER) / (DISK_OUTER - DISK_INNER);

    // Keplerian rotation
    let omega = pow(r, -1.5) * 0.4;
    let rotated_angle = angle + omega * time;

    // Temperature gradient: hot white inside, cooler orange outside
    // Inner: ~10000K (white-yellow), Outer: ~3000K (deep orange)
    let temp = 1.0 - pow(r_norm, 0.6);

    // Doppler effect from orbital motion
    let orbital_dir = normalize(vec3<f32>(-hit_pos.z, 0.0, hit_pos.x));
    let v_orbital = sqrt(0.5 / r);
    let doppler = 1.0 + dot(orbital_dir, -ray_dir) * v_orbital * 2.0;

    // Base color: orange to white gradient
    var color: vec3<f32>;
    if temp > 0.7 {
        // Inner disk: white-yellow
        color = mix(vec3<f32>(1.0, 0.85, 0.5), vec3<f32>(1.0, 1.0, 0.95), (temp - 0.7) / 0.3);
    } else if temp > 0.4 {
        // Middle disk: golden orange
        color = mix(vec3<f32>(1.0, 0.5, 0.1), vec3<f32>(1.0, 0.85, 0.5), (temp - 0.4) / 0.3);
    } else {
        // Outer disk: deep orange to red
        color = mix(vec3<f32>(0.8, 0.2, 0.0), vec3<f32>(1.0, 0.5, 0.1), temp / 0.4);
    }

    // Turbulence in the disk
    let turb_coord = vec3<f32>(r * 2.0, rotated_angle, time * 0.1);
    let turbulence = noise(turb_coord) * 0.25 + 0.75;

    // Subtle spiral structure
    let spiral = sin(rotated_angle * 3.0 - r * 0.8) * 0.15 + 0.85;

    // Radial brightness falloff (inner is MUCH brighter)
    let radial_intensity = exp(-r_norm * 1.5) * 0.8 + 0.2;

    // Inner edge glow (very bright)
    let inner_glow = pow(max(0.0, 1.0 - (r - DISK_INNER) / 2.0), 3.0);

    // Combine factors
    let base_intensity = turbulence * spiral * radial_intensity * clamp(doppler, 0.5, 2.0);
    let total_intensity = base_intensity * 5.0 + inner_glow * 8.0;

    return color * total_intensity;
}

// ============================================================================
// Ray marching with gravitational lensing
// ============================================================================

fn trace(origin: vec3<f32>, dir: vec3<f32>, time: f32) -> vec3<f32> {
    var pos = origin;
    var ray_dir = normalize(dir);
    var prev_y = pos.y;
    var accumulated = vec3<f32>(0.0);
    var disk_hit = false;

    for (var i = 0; i < RAY_STEPS; i++) {
        let r = length(pos);

        // Fell into black hole
        if r < SCHWARZSCHILD_RADIUS * 1.1 {
            return accumulated;
        }

        // Escaped to infinity
        if r > 50.0 {
            let star_contrib = stars(ray_dir);
            if disk_hit {
                return accumulated + star_contrib * 0.3;
            }
            return star_contrib;
        }

        // Gravitational bending: stronger near the hole
        let bend = 1.5 * SCHWARZSCHILD_RADIUS / (r * r);
        let to_center = -normalize(pos);
        ray_dir = normalize(ray_dir + to_center * bend * STEP_SIZE);

        // Check disk crossing (y = 0 plane)
        let new_pos = pos + ray_dir * STEP_SIZE;

        if prev_y * new_pos.y < 0.0 {
            // Crossed the disk plane - interpolate to find hit point
            let t = -pos.y / (new_pos.y - pos.y);
            let hit = pos + (new_pos - pos) * t;
            let hit_r = length(hit.xz);

            if hit_r > DISK_INNER && hit_r < DISK_OUTER {
                let angle = atan2(hit.z, hit.x);
                let disk = disk_color(hit_r, angle, hit, ray_dir, time);

                if !disk_hit {
                    accumulated = disk;
                    disk_hit = true;
                } else {
                    // Secondary image (light wrapped around)
                    accumulated = accumulated + disk * 0.3;
                }
            }
        }

        prev_y = new_pos.y;
        pos = new_pos;
    }

    // Didn't escape or hit hole - return accumulated
    return accumulated + stars(ray_dir) * 0.2;
}

// ============================================================================
// Fragment shader
// ============================================================================

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Normalized device coordinates: [-1, 1]
    let ndc = (in.uv - 0.5) * 2.0;
    let aspect = 640.0 / 480.0;
    let uv = vec2<f32>(ndc.x * aspect, ndc.y);

    // Camera: positioned to show the black hole prominently
    // Looking at the black hole from above and in front
    let cam_pos = vec3<f32>(0.0, 6.0, -18.0);
    let cam_target = vec3<f32>(0.0, 0.0, 0.0);
    let cam_up = vec3<f32>(0.0, 1.0, 0.0);

    // Camera basis vectors
    let forward = normalize(cam_target - cam_pos);
    let right = normalize(cross(forward, cam_up));
    let up = cross(right, forward);

    // Ray direction with FOV
    let fov = 0.8;
    let ray_dir = normalize(forward + uv.x * right * fov + uv.y * up * fov);

    // Trace the ray
    var color = trace(cam_pos, ray_dir, uniforms.time);

    // Photon ring glow at the edge of the black hole shadow
    let to_center = normalize(-cam_pos);
    let view_angle = dot(ray_dir, to_center);
    let shadow_size = SCHWARZSCHILD_RADIUS * 2.6 / length(cam_pos);

    // Ring at shadow boundary
    let ring = smoothstep(shadow_size * 0.9, shadow_size, view_angle) *
               smoothstep(shadow_size * 1.15, shadow_size * 1.02, view_angle);
    color = color + vec3<f32>(1.0, 0.7, 0.4) * ring * 2.0;

    // Tone mapping (filmic)
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    color = clamp((color * (a * color + b)) / (color * (c * color + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));

    // Gamma
    color = pow(color, vec3<f32>(1.0 / 2.2));

    // Subtle vignette
    let vignette = 1.0 - length(ndc) * 0.15;
    color = color * vignette;

    return vec4<f32>(color, 1.0);
}
