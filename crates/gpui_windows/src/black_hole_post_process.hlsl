// Full window "black hole" post-process.
//
// The pass samples a copy of the already rendered window (premultiplied BGRA)
// and warps it as if it were seen through the gravitational field of a
// Schwarzschild black hole, then blends the result back over the untouched
// window contents using `black_hole_intensity`.
//
// Ray bending in the near field integrates the Binet form of the null geodesic
// equation, following the approach of the MIT licensed `ghostty-blackhole`
// shader; the far field uses the analytic weak deflection expansion so that the
// whole window still bends without paying for the integration everywhere.

cbuffer BlackHolePostProcessParams: register(b0) {
    // Center of the shadow, normalized to the window size, top-left origin.
    float2 black_hole_center;
    // Radius of the shadow, normalized to the window height.
    float black_hole_radius;
    // Animation time, in seconds.
    float black_hole_time;
    // Blend factor against the untouched window contents, in 0..1.
    float black_hole_intensity;
    float2 black_hole_viewport_size;
    float black_hole_pad;
};

Texture2D<float4> t_window: register(t0);
SamplerState s_window: register(s0);

static const float PI = 3.141592653589793;

// Geometrized units with G = c = M = 1: the horizon sits at r = 2, the photon
// sphere at r = 3 and the critical impact parameter is 3 * sqrt(3).
static const float B_CRIT = 5.196152422706632;
static const int GEODESIC_STEPS = 32;

// Above this impact parameter the analytic far field deflection is accurate to
// better than a pixel, and the ray can no longer reach the accretion disk.
static const float NEAR_FIELD_B = 40.0;

// Effective lens distance, in units of M. Converts a deflection angle into a
// screen space displacement; larger values bend the window more.
static const float LENS_DISTANCE = 17.0;

// Thin accretion disk, in units of M. The inner edge is the innermost stable
// circular orbit of a Schwarzschild black hole.
static const float DISK_INNER = 6.0;
static const float DISK_OUTER = 18.0;
// 0 is edge on, PI / 2 is face on.
static const float DISK_TILT = 0.32;
static const float DISK_SPIN = 0.75;
static const float DISK_BRIGHTNESS = 1.35;

struct BlackHolePostProcessVertexOutput {
    float4 position: SV_Position;
    float2 texture_coords: TEXCOORD0;
};

BlackHolePostProcessVertexOutput black_hole_post_process_vertex(uint vertex_id: SV_VertexID) {
    // Same triangle strip winding as the rest of the gpui pipelines.
    float2 unit_vertex = float2(float(vertex_id & 1u), 0.5 * float(vertex_id & 2u));

    BlackHolePostProcessVertexOutput output;
    output.position = float4(unit_vertex * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    output.texture_coords = unit_vertex;
    return output;
}

// Turning point of a null geodesic with impact parameter `b`, as u = 1 / r.
//
// The radial equation (du/dphi)^2 = 1 / b^2 - u^2 + 2 u^3 turns around at the
// roots of 2 u^3 - u^2 + 1 / b^2. Of its three real roots the periapsis is the
// middle one, which the trigonometric solution of the depressed cubic gives in
// closed form.
float geodesic_periapsis(float b) {
    float arg = clamp(1.0 - 54.0 / (b * b), -1.0, 1.0);
    return 1.0 / 6.0 + (1.0 / 3.0) * cos(acos(arg) / 3.0 - 2.0 * PI / 3.0);
}

// The radial equation factored as (u_p - u) * g(u), so that the square root
// singularity at the turning point can be removed analytically.
float geodesic_g(float periapsis, float u) {
    return periapsis + u - 2.0 * (periapsis * periapsis + periapsis * u + u * u);
}

// Analytic deflection for a ray that stays far from the hole, to second order
// in 1 / b.
float far_field_deflection(float b) {
    return 4.0 / b + 15.0 * PI / (4.0 * b * b);
}

float3 disk_temperature_color(float t) {
    // Keep the body of the disk saturated enough to read over the App's light
    // surfaces, then let only the hottest filaments reach warm white.
    float3 ember = float3(0.22, 0.025, 0.003);
    float3 orange = float3(0.96, 0.20, 0.008);
    float3 gold = float3(1.0, 0.62, 0.10);
    float3 hot = float3(1.0, 0.95, 0.76);
    float3 color = lerp(ember, orange, smoothstep(0.0, 0.42, t));
    color = lerp(color, gold, smoothstep(0.38, 0.78, t));
    return lerp(color, hot, smoothstep(0.76, 1.0, t));
}

float2 rotate_screen(float2 value, float angle) {
    float sine = sin(angle);
    float cosine = cos(angle);
    return float2(
        value.x * cosine - value.y * sine,
        value.x * sine + value.y * cosine
    );
}

// A high-contrast projected disk supplements the geodesic crossings below.
// The traced disk provides the physically lensed higher-order images, while
// this layer guarantees that the primary orange-gold light belt remains legible
// over both dark and nearly-white application themes.
void stylized_accretion_disk(
    float2 offset,
    float radius,
    float time,
    out float3 color,
    out float opacity,
    out float glow
) {
    float2 q = rotate_screen(offset / max(radius, 1e-4), -0.18);
    float shadow_distance = length(q);

    // An almost edge-on annulus. Its near half crosses in front of the shadow;
    // its far half is hidden there and reappears as the lensed arcs below.
    float flatten = 0.17;
    float2 disk = float2(q.x, q.y / flatten);
    float rho = max(length(disk), 1e-4);
    float azimuth = atan2(disk.y, disk.x);
    float annulus =
        smoothstep(1.12, 1.40, rho) *
        (1.0 - smoothstep(2.78, 3.45, rho));

    // `time` is scaled by the App so this renderer's 18-second baseline maps to
    // the plugin's configured rotation period. Broad spiral arms and one orbiting
    // hot sector give the eye an unambiguous rotation cue.
    float rotation = time * (2.0 * PI / 18.0);
    float kepler = pow(1.35 / max(rho, 1.0), 0.68);
    float spiral_phase =
        azimuth - rotation * (0.72 + 0.78 * kepler) + rho * 1.52;
    float broad_arm = smoothstep(
        0.18,
        0.92,
        0.5 + 0.5 * sin(spiral_phase * 2.0)
    );
    float fine_filament = pow(
        saturate(0.5 + 0.5 * sin(spiral_phase * 7.0 + rho * 5.6)),
        2.2
    );
    float orbiting_hot_sector = pow(
        saturate(0.5 + 0.5 * cos(azimuth - rotation * 1.28 - 0.55)),
        7.0
    );
    float trailing_hot_sector = pow(
        saturate(0.5 + 0.5 * cos(azimuth - rotation * 0.86 + 2.35)),
        10.0
    );
    float streak = saturate(
        0.03 + 0.56 * pow(broad_arm, 1.9) +
        0.22 * fine_filament + 0.36 * orbiting_hot_sector +
        0.14 * trailing_hot_sector
    );

    float radial_heat = pow(saturate((3.45 - rho) / 2.33), 0.58);
    float approaching = 0.5 + 0.5 * disk.x / rho;
    float doppler = lerp(0.62, 1.34, smoothstep(0.0, 1.0, approaching));
    float3 ember = float3(0.16, 0.018, 0.002);
    float3 orange = float3(0.92, 0.16, 0.005);
    float3 gold = float3(1.0, 0.58, 0.08);
    float3 warm_white = float3(1.0, 0.96, 0.80);
    color = lerp(ember, orange, smoothstep(0.01, 0.38, radial_heat));
    color = lerp(
        color,
        gold,
        smoothstep(0.24, 0.76, radial_heat) * (0.28 + 0.72 * streak)
    );
    color = lerp(
        color,
        warm_white,
        radial_heat * (0.82 * orbiting_hot_sector + 0.34 * trailing_hot_sector)
    );
    float blue_hot = smoothstep(0.78, 1.0, approaching) *
        orbiting_hot_sector * radial_heat * 0.34;
    color = lerp(color, float3(0.82, 0.92, 1.0), blue_hot);
    color *= (0.74 + 0.18 * streak + 0.18 * orbiting_hot_sector) * doppler;

    float near_side = smoothstep(-0.06, 0.20, q.y);
    float outside_shadow = smoothstep(0.96, 1.08, shadow_distance);
    float direct_visibility = lerp(outside_shadow, 1.0, near_side);
    float direct_opacity = annulus * direct_visibility *
        (0.36 + 0.38 * streak + 0.24 * orbiting_hot_sector);

    // The far side is bent into a bright upper arc and a dimmer secondary image
    // below the shadow, matching the characteristic Interstellar-style silhouette.
    float arc_center = 1.12 + 0.06 * saturate(abs(q.x));
    float arc_line = exp(-pow((shadow_distance - arc_center) * 20.0, 2.0));
    float vertical_direction = q.y / max(shadow_distance, 1e-4);
    float upper_arc = smoothstep(0.04, 0.40, -vertical_direction);
    float lower_arc = 0.24 * smoothstep(0.20, 0.60, vertical_direction);
    float arc_opacity = arc_line * (upper_arc + lower_arc) *
        (0.40 + 0.38 * streak + 0.22 * orbiting_hot_sector);

    opacity = saturate(direct_opacity * 0.94 + arc_opacity * 0.86);

    float broad_annulus =
        smoothstep(0.96, 1.18, rho) *
        (1.0 - smoothstep(3.18, 3.85, rho));
    glow = saturate(
        broad_annulus * (0.04 + 0.08 * streak) +
        arc_line * (upper_arc + lower_arc) * 0.14
    );
}

// Emission picked up where the segment `p0 -> p1` of the traced geodesic
// crosses the plane of the thin accretion disk. `to_observer` is the unit
// direction the photon travels along on its way out to the viewer, and drives
// the relativistic Doppler asymmetry.
float3 disk_segment_emission(
    float3 p0,
    float3 p1,
    float3 to_observer,
    float3 disk_normal,
    float3 disk_x,
    float3 disk_y
) {
    float3 emission = float3(0.0, 0.0, 0.0);

    float h0 = dot(p0, disk_normal);
    float h1 = dot(p1, disk_normal);
    // A sign change means the segment crosses the plane of the disk. The whole
    // path is tested step by step, so a ray that winds around the hole picks up
    // several crossings.
    if (h0 * h1 < 0.0) {
        float3 hit = lerp(p0, p1, h0 / (h0 - h1));
        float rho = length(hit);
        if (rho >= DISK_INNER && rho <= DISK_OUTER) {
            // Keplerian orbital motion, v = sqrt(M / rho) with M = 1.
            float3 radial = hit / rho;
            float3 orbit = normalize(cross(disk_normal, radial));
            float speed = 1.0 / sqrt(rho);
            float beta = dot(orbit * speed, to_observer);
            float gamma = 1.0 / sqrt(max(1.0 - speed * speed, 1e-3));
            float doppler = 1.0 / max(gamma * (1.0 - beta), 1e-3);
            // Gravitational redshift of the emitting material.
            float redshift = sqrt(max(1.0 - 2.0 / rho, 1e-3));
            float boost = pow(clamp(doppler * redshift, 0.05, 6.0), 3.0);

            // Sheared Keplerian streaks: inner material laps outer material.
            float azimuth = atan2(dot(hit, disk_y), dot(hit, disk_x));
            float omega = pow(rho, -1.5);
            float phase = azimuth - omega * black_hole_time * DISK_SPIN * 12.0;
            float streak = 0.62 + 0.38 * sin(phase * 7.0 + rho * 0.35);
            streak *= 0.68 + 0.32 * sin(phase * 17.0 - rho * 1.1);
            streak *= 0.80 + 0.20 * sin(phase * 3.0 + rho * 0.9);

            // Temperature drops as rho^(-3/4) across the disk.
            float temperature = pow(DISK_INNER / rho, 0.75);
            float3 color = disk_temperature_color(temperature);

            float radial_profile =
                smoothstep(DISK_INNER, DISK_INNER * 1.18, rho) *
                smoothstep(DISK_OUTER, DISK_OUTER * 0.62, rho);

            emission = color *
                (boost * max(streak, 0.0) * radial_profile * temperature * DISK_BRIGHTNESS);
        }
    }

    return emission;
}

float4 black_hole_post_process_fragment(BlackHolePostProcessVertexOutput input): SV_Target {
    float2 uv = input.texture_coords;
    float4 original = t_window.SampleLevel(s_window, uv, 0.0);

    float intensity = saturate(black_hole_intensity);
    float radius = max(black_hole_radius, 1e-4);
    float aspect = max(black_hole_viewport_size.x, 1.0) / max(black_hole_viewport_size.y, 1.0);

    // Screen offset from the center, corrected for the window aspect ratio so
    // that it is measured in units of the window height.
    float2 offset = uv - black_hole_center;
    offset.x *= aspect;
    float screen_radius = max(length(offset), 1e-6);
    float2 direction = offset / screen_radius;

    // The shadow radius is pinned to the critical impact parameter, so screen
    // space and impact parameter are related by a single scale.
    float scale = B_CRIT / radius;
    float b = screen_radius * scale;

    float3 accretion_color;
    float accretion_opacity;
    float accretion_glow;
    stylized_accretion_disk(
        offset,
        radius,
        black_hole_time,
        accretion_color,
        accretion_opacity,
        accretion_glow
    );

    if (b <= B_CRIT) {
        // The ray spirals into the horizon. The near half of the accretion disk
        // can still lie between the observer and the shadow, so composite that
        // luminous belt over black instead of returning before the disk is drawn.
        float3 captured_color = lerp(float3(0.0, 0.0, 0.0), accretion_color, accretion_opacity);
        captured_color = lerp(
            captured_color,
            accretion_color,
            accretion_glow * 0.10 * (1.0 - accretion_opacity)
        );
        float4 captured = float4(captured_color, original.a);
        return lerp(original, captured, intensity);
    }

    float3 emission = float3(0.0, 0.0, 0.0);
    float deflection = 0.0;

    if (b >= NEAR_FIELD_B) {
        deflection = far_field_deflection(b);
    } else {
        // The orbit is symmetric about the periapsis, so a single sweep from
        // the turning point out to infinity covers both halves of the path.
        //
        // With u = periapsis * (1 - s^2) the integrand loses its square root
        // singularity and becomes dphi = 2 sqrt(u_p) ds / sqrt(g(u)), which a
        // fixed step midpoint rule integrates accurately.
        float periapsis = geodesic_periapsis(b);
        float step_size = 1.0 / float(GEODESIC_STEPS);
        float root_periapsis = 2.0 * sqrt(max(periapsis, 1e-6));

        float3 plane_radial = float3(direction, 0.0);
        float3 plane_forward = float3(0.0, 0.0, 1.0);

        float3 disk_normal = float3(0.0, cos(DISK_TILT), sin(DISK_TILT));
        float3 disk_x = float3(1.0, 0.0, 0.0);
        float3 disk_y = float3(0.0, sin(DISK_TILT), -cos(DISK_TILT));

        // Both halves start at the periapsis, where phi is zero.
        float3 outgoing_prev = plane_radial / max(periapsis, 1e-6);
        float3 incoming_prev = outgoing_prev;

        // The path is closest to the hole at the periapsis, so a ray that turns
        // around outside the disk can never reach it and only needs the bend.
        bool reaches_disk = 1.0 / max(periapsis, 1e-6) <= DISK_OUTER;

        float phi = 0.0;
        [loop]
        for (int i = 0; i < GEODESIC_STEPS; i++) {
            float s = (float(i) + 0.5) * step_size;
            float u = periapsis * (1.0 - s * s);
            float g = max(geodesic_g(periapsis, u), 1e-6);
            float dphi = root_periapsis * step_size / sqrt(g);
            float phi_mid = phi + 0.5 * dphi;
            phi += dphi;

            float r = 1.0 / max(u, 1e-5);
            float3 along = r * cos(phi_mid) * plane_radial;
            float3 across = r * sin(phi_mid) * plane_forward;
            // The far half winds behind the hole, which is what lenses the far
            // side of the disk up over the shadow; the near half comes back
            // towards the viewer.
            float3 outgoing = along + across;
            float3 incoming = along - across;

            if (reaches_disk) {
                emission += disk_segment_emission(
                    outgoing_prev, outgoing, normalize(outgoing_prev - outgoing),
                    disk_normal, disk_x, disk_y);
                emission += disk_segment_emission(
                    incoming_prev, incoming, normalize(incoming - incoming_prev),
                    disk_normal, disk_x, disk_y);
            }

            outgoing_prev = outgoing;
            incoming_prev = incoming;
        }

        // Total bend of the ray: the swept angle minus the straight line.
        deflection = max(2.0 * phi - PI, 0.0);
    }

    // Thin lens mapping: the image sits further out than the source it shows,
    // so step back towards the center to find what this pixel actually sees.
    // A negative radius lands on the opposite side, which is the second image.
    float shift = min(deflection * LENS_DISTANCE / scale, screen_radius + 2.0 * radius);
    float2 source = direction * (screen_radius - shift);
    source.x /= aspect;
    float4 lensed = t_window.SampleLevel(s_window, black_hole_center + source, 0.0);

    // Fade the last sliver above the critical impact parameter into the shadow,
    // where the deflection diverges and the lookup is meaningless anyway.
    float horizon = 1.0 - smoothstep(B_CRIT, B_CRIT * 1.03, b);
    lensed *= 1.0 - horizon;

    // Photon ring: rays grazing the photon sphere pile up into a thin gold arc.
    float ring = exp(-pow(max(b / B_CRIT - 1.0, 0.0) * 24.0, 1.3));
    emission += float3(1.0, 0.84, 0.58) * (ring * 0.18);

    // Tonemap the geodesically traced crossings instead of adding their HDR
    // values directly. Replacing the background under the disk preserves its
    // orange-gold color on light themes, where additive white would disappear.
    float3 physical_disk = float3(1.0, 1.0, 1.0) - exp(-max(emission, 0.0) * 0.55);
    float physical_opacity = saturate(
        max(physical_disk.r, max(physical_disk.g, physical_disk.b)) * 0.72
    );
    float3 effect_color = lerp(lensed.rgb, physical_disk, physical_opacity);
    effect_color = lerp(effect_color, accretion_color, accretion_opacity);
    effect_color = lerp(
        effect_color,
        accretion_color,
        accretion_glow * 0.08 * (1.0 - accretion_opacity)
    );

    // The window contents are premultiplied. Preserve their alpha, extending it
    // only where emissive disk light exists on a transparent surface.
    float disk_alpha = saturate(max(physical_opacity, accretion_opacity));
    float alpha = lerp(lensed.a, original.a, horizon);
    float4 effect = float4(
        effect_color,
        saturate(alpha + disk_alpha * (1.0 - alpha))
    );

    return lerp(original, effect, intensity);
}
