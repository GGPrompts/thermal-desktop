// Thermal Palette — boosted for visibility
const vec3 c_void   = vec3(0.05, 0.00, 0.10);     // Dark purple base
const vec3 c_deep   = vec3(0.18, 0.02, 0.32);     // Visible purple clouds
const vec3 c_cold   = vec3(0.30, 0.18, 0.55);     // Brighter cold purple
const vec3 c_accent = vec3(0.769, 0.710, 0.992);  // #c4b5fd (Rare brights)
const vec3 c_hot    = vec3(0.937, 0.267, 0.267);  // #ef4444 (Thermal hotspots)

// Random function for noise
float random(in vec2 st) {
    return fract(sin(dot(st.xy, vec2(12.9898,78.233))) * 43758.5453123);
}

// 2D Noise based on Morgan McGuire @morgan3d
// https://www.shadertoy.com/view/4dS3Wd
float noise(in vec2 st) {
    vec2 i = floor(st);
    vec2 f = fract(st);

    // Four corners in 2D of a tile
    float a = random(i);
    float b = random(i + vec2(1.0, 0.0));
    float c = random(i + vec2(0.0, 1.0));
    float d = random(i + vec2(1.0, 1.0));

    // Smooth Interpolation
    vec2 u = f * f * (3.0 - 2.0 * f);

    // Mix 4 corners percentages
    return mix(a, b, u.x) +
            (c - a) * u.y * (1.0 - u.x) +
            (d - b) * u.x * u.y;
}

// Fractal Brownian Motion
#define OCTAVES 4
float fbm(in vec2 st) {
    float value = 0.0;
    float amplitude = 0.5;
    // Rotation matrix to reduce grid artifacts
    mat2 rot = mat2(cos(0.5), sin(0.5), -sin(0.5), cos(0.50));

    for (int i = 0; i < OCTAVES; i++) {
        value += amplitude * noise(st);
        st = rot * st * 2.0 + vec2(100.0, 0.0);
        amplitude *= 0.5;
    }
    return value;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    // Normalize coordinates
    vec2 st = fragCoord / iResolution.xy;
    st.x *= iResolution.x / iResolution.y; // Correct aspect ratio

    // Time factor - very slow for ambient feel
    float t = iTime * 0.05;

    // Layered FBM for organic cloud movement
    // q is the base warp
    vec2 q = vec2(0.);
    q.x = fbm(st + 0.01 * t);
    q.y = fbm(st + vec2(1.0));

    // r is the secondary warp based on q
    vec2 r = vec2(0.);
    r.x = fbm(st + 1.0 * q + vec2(1.7, 9.2) + 0.15 * t);
    r.y = fbm(st + 1.0 * q + vec2(8.3, 2.8) + 0.126 * t);

    // Final noise value
    float f = fbm(st + r);

    // Start with the void color
    vec3 color = c_void;

    // Mix 1: Deep Purple Clouds
    float cloud_mask = smoothstep(0.2, 0.8, f);
    color = mix(color, c_deep, cloud_mask);

    // Mix 2: Cold Purple Details — boosted
    float cold_mask = smoothstep(0.3, 0.7, f);
    float detail_noise = noise(st * 4.0 - t * 0.1);
    color = mix(color, c_cold, cold_mask * detail_noise * 0.8);

    // Mix 3: Faint Accent Wisps
    float wisp_noise = noise(st * 8.0 + vec2(0.0, -t * 0.5));
    float accent_mask = smoothstep(0.6, 0.9, f) * smoothstep(0.5, 1.0, wisp_noise);
    color = mix(color, c_accent, accent_mask * 0.15);

    // Mix 4: Hot Spots (Thermal Bloom)
    float hot_mask = smoothstep(0.75, 1.0, f) * smoothstep(0.6, 1.0, r.x);
    color = mix(color, c_hot, hot_mask * 0.10);

    // Vignette - subtle darkening at edges
    vec2 uv = fragCoord / iResolution.xy;
    float dist = distance(uv, vec2(0.5));
    float vignette = smoothstep(0.8, 0.2, dist * 0.8);
    color = mix(color * 0.7, color, vignette);

    fragColor = vec4(color, 1.0);
}
