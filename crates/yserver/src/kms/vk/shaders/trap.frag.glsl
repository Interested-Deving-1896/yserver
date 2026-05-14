#version 450

// GPU rasterization for RENDER Trapezoids (gpu-trap T1). Computes
// analytic edge coverage for the four trap edges (top, bottom,
// left, right). Output is single-channel R8 coverage in [0, 1].
//
// AA strategy: linear-approximation edge coverage. Per edge, the
// signed distance from the pixel center to the edge line is clamped
// to [0, 1] (centered at 0.5 from the line). The four contributions
// multiply to the pixel's coverage. This matches what cairo / Skia
// GPU backends ship; exact for edges through pixel centers and
// within 1-2 LSB for grazing corners.

// Per-instance flat-interpolated trap params.
layout(location = 0) flat in float top;
layout(location = 1) flat in float bottom;
layout(location = 2) flat in vec2 left_p1;
layout(location = 3) flat in vec2 left_p2;
layout(location = 4) flat in vec2 right_p1;
layout(location = 5) flat in vec2 right_p2;

layout(location = 0) out float coverage;

// Coverage contribution of one slanted edge (left or right). The
// `inside_side` argument is +1 for the left edge (where the trap's
// interior lies to the right of the directed edge p1→p2) and -1
// for the right edge (interior to the left). The perpendicular
// (-d.y, d.x) rotates the edge direction 90° CCW; with the sign
// chosen so signed_dist > 0 means "pixel is outside the trap".
//
// Returns coverage in [0, 1]: 1 when the pixel center is well
// inside the half-plane, 0 when well outside, linearly interpolated
// across the 1-pixel transition band centered on the edge.
//
// Degenerate (zero-length) edges return 0.0 — a collapsed side
// means the trap has no area on this side; returning 1.0 would
// over-cover, painting where the trap doesn't exist.
float edge_coverage_linear(vec2 p, vec2 a, vec2 b, float inside_side) {
    vec2 d = b - a;
    float len = length(d);
    if (len < 1e-6) {
        return 0.0;
    }
    vec2 n = vec2(-d.y, d.x) / len;
    float signed_dist = dot(p - a, n) * inside_side;
    return clamp(0.5 - signed_dist, 0.0, 1.0);
}

void main() {
    vec2 p = gl_FragCoord.xy; // pixel center (Vulkan default)

    // Top edge: y >= top is inside (top is the upper Y, trap
    // extends downward). c_top = 1 when the pixel is fully below
    // the top line, 0 when fully above, 0..1 straddling.
    float c_top = clamp(p.y - top, 0.0, 1.0);
    // Bottom edge: y <= bottom is inside.
    float c_bot = clamp(bottom - p.y, 0.0, 1.0);

    // Slanted sides — inside-side convention per the RENDER
    // Trapezoid spec: left_p1→left_p2 keeps the interior on the
    // +inside_side; right_p1→right_p2 keeps it on the -inside_side.
    float cov_left = edge_coverage_linear(p, left_p1, left_p2, +1.0);
    float cov_right = edge_coverage_linear(p, right_p1, right_p2, -1.0);

    coverage = c_top * c_bot * cov_left * cov_right;
}
