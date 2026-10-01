// The l3i hero: a night sky drawn live with three.js behind the project page's header.
//
// Three layers, back to front. A full-screen shader draws stars and layered purple mist, two
// domain-warped noise fields drifting at different speeds, lit by the moon's position on screen.
// A sphere is the moon: basalt-filled basins, aged impacts, terraced walls, central peaks and
// fresh ejecta are baked once into a data texture. Spherical height derivatives supply relief
// normals; bounded spherical height-field traces shadow the relief, and a Lommel-Seeliger/
// Lambert blend gives the stone a powdery, airless appearance rather than a glossy highlight.
// Violet earthshine and phase-linked observer-side haze retain the site's palette.
// Dust rises through the moonlight as point sprites.
//
// The palette is read from the site's CSS tokens, so sass/brand.sass stays the single owner of
// the colours. The canvas is inert until the hero is on screen, stops when the tab is hidden, caps
// the device pixel ratio, and under prefers-reduced-motion draws one frame and stops. The still
// that sass/brand.sass draws stays until the first frame is on the canvas, which fades in over
// it. Without WebGL nothing is added and the still remains.
//
// The template loads this module through [extra.hero] in config.toml and gives the hero an empty
// [data-dw-hero-art] behind the text, which the canvas fills. A hero without one gets the canvas
// as its own first child.

import * as THREE from './vendor/three.module.min.js';

function cssColor(name, fallback) {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const color = new THREE.Color(fallback);
  if (raw) {
    try { color.set(raw); } catch { /* an unparsable token keeps the fallback */ }
  }
  return color;
}

function cssLength(name, fallback) {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const value = Number.parseFloat(raw);
  return Number.isFinite(value) ? value : fallback;
}

const NOISE = /* glsl */ `
  float hash21(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
  }
  float hash31(vec3 p) {
    p = fract(p * 0.3183099 + 0.1);
    p *= 17.0;
    return fract(p.x * p.y * p.z * (p.x + p.y + p.z));
  }
  float noise2(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash21(i), hash21(i + vec2(1.0, 0.0)), u.x),
               mix(hash21(i + vec2(0.0, 1.0)), hash21(i + vec2(1.0, 1.0)), u.x), u.y);
  }
  float noise3(vec3 x) {
    vec3 i = floor(x);
    vec3 f = fract(x);
    f = f * f * (3.0 - 2.0 * f);
    return mix(mix(mix(hash31(i), hash31(i + vec3(1.0, 0.0, 0.0)), f.x),
                   mix(hash31(i + vec3(0.0, 1.0, 0.0)), hash31(i + vec3(1.0, 1.0, 0.0)), f.x), f.y),
               mix(mix(hash31(i + vec3(0.0, 0.0, 1.0)), hash31(i + vec3(1.0, 0.0, 1.0)), f.x),
                   mix(hash31(i + vec3(0.0, 1.0, 1.0)), hash31(i + vec3(1.0, 1.0, 1.0)), f.x), f.y), f.z);
  }
  float fbm2(vec2 p) {
    float v = 0.0;
    float a = 0.5;
    mat2 m = mat2(1.6, 1.2, -1.2, 1.6);
    for (int i = 0; i < 5; i++) {
      v += a * noise2(p);
      p = m * p;
      a *= 0.5;
    }
    return v;
  }
  float fbm3(vec3 p) {
    float v = 0.0;
    float a = 0.5;
    for (int i = 0; i < 5; i++) {
      v += a * noise3(p);
      p = p * 2.03 + vec3(1.7, 9.2, 3.1);
      a *= 0.5;
    }
    return v;
  }
  // Three octaves: for detail that must stay wider than a pixel on a small disc.
  float fbm3Coarse(vec3 p) {
    float v = 0.0;
    float a = 0.5;
    for (int i = 0; i < 3; i++) {
      v += a * noise3(p);
      p = p * 2.03 + vec3(1.7, 9.2, 3.1);
      a *= 0.5;
    }
    return v;
  }
`;

// The sky: stars, then two layers of domain-warped mist, lit by the moon.
const SKY_VERTEX = /* glsl */ `
  varying vec2 vUv;
  void main() {
    vUv = uv;
    gl_Position = vec4(position.xy, 0.0, 1.0);
  }
`;

const SKY_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec2 vUv;
  uniform float uTime;
  uniform vec2 uResolution;
  uniform vec2 uMoon;        // the moon's centre, in aspect-corrected uv
  uniform float uMoonRadius; // its radius in the same units
  uniform float uMoonlight;  // illuminated fraction, shared with the observer-side haze
  uniform vec2 uDrift;       // pointer parallax
  uniform vec3 uBgTop;
  uniform vec3 uBgBottom;
  uniform vec3 uMistDeep;
  uniform vec3 uMistBright;
  uniform vec3 uAccent;
  uniform float uNarrow;     // 1 on phones: the text runs the whole width
  ${NOISE}

  void main() {
    float aspect = uResolution.x / uResolution.y;
    vec2 p = vec2(vUv.x * aspect, vUv.y);
    vec3 col = mix(uBgBottom, uBgTop, smoothstep(0.0, 1.0, vUv.y));

    // Stars: sparse hashed pinpoints in three tiles of unrelated size, twinkling, thinning
    // toward the horizon and away from the moon's glare.
    float stars = 0.0;
    for (int layer = 0; layer < 3; layer++) {
      float scale = 90.0 + 70.0 * float(layer);
      vec2 g = (p + uDrift * (0.12 + 0.06 * float(layer))) * scale + float(layer) * 31.7;
      vec2 cell = floor(g);
      vec2 f = fract(g) - 0.5;
      float h = hash21(cell + float(layer) * 7.3);
      vec2 offset = vec2(hash21(cell + 1.1), hash21(cell + 2.2)) - 0.5;
      float d = length(f - offset * 0.7);
      float twinkle = 0.55 + 0.45 * sin(uTime * (0.8 + 1.7 * h) + h * 40.0);
      float star = (1.0 - smoothstep(0.0, 0.09, d)) * step(0.965, h) * twinkle;
      stars += star * (0.5 + 0.5 * float(layer));
    }
    stars *= smoothstep(0.05, 0.5, vUv.y);
    float moonDist = length(p - uMoon);
    stars *= smoothstep(uMoonRadius * 1.1, uMoonRadius * 2.6, moonDist);
    col += stars * mix(vec3(1.0), uAccent, 0.35) * 0.9;

    // Mist: a slow field warped by a faster one, twice, at two scales. Denser at the horizon,
    // thinner over the text on the left, brighter within the moon's light.
    vec2 q = p * 1.35 + uDrift * 0.25;
    float t = uTime * 0.045;
    vec2 warp = vec2(fbm2(q + vec2(t, -t * 0.7)), fbm2(q + vec2(-t * 0.6, t * 0.4) + 5.2));
    float mistA = fbm2(q + 1.8 * warp + vec2(t * 0.8, t * 0.2));
    vec2 q2 = p * 0.7 - uDrift * 0.15;
    vec2 warp2 = vec2(fbm2(q2 - vec2(t * 0.5, t * 0.3) + 9.1), fbm2(q2 + vec2(t * 0.2, -t * 0.5) + 3.7));
    float mistB = fbm2(q2 + 2.2 * warp2 - vec2(t * 0.3, 0.0));
    float mist = smoothstep(0.32, 0.9, mistA * 0.65 + mistB * 0.55);
    float horizon = mix(0.25, 1.0, pow(1.0 - vUv.y, 1.6));
    mist *= horizon;
    float shade = mix(smoothstep(0.0, 0.62, vUv.x) * 0.8 + 0.2, 0.55, uNarrow);
    mist *= shade;

    // The moon lights the mist: nearer is brighter and bluer-white, far is deep purple.
    float glow = (exp(-moonDist * 2.1) + 0.35 * exp(-moonDist * 0.9)) * uMoonlight;
    vec3 mistColor = mix(uMistDeep, uMistBright, clamp(mist * 1.2, 0.0, 1.0));
    mistColor = mix(mistColor, mix(uMistBright, vec3(1.0), 0.35), clamp(glow * 0.9, 0.0, 1.0));
    col = mix(col, mistColor, clamp(mist * (0.55 + 0.45 * glow), 0.0, 0.95));

    // The halo: moonlight scattered by the mist itself, strongest where the mist is.
    float halo = exp(-max(moonDist - uMoonRadius, 0.0) * 3.2);
    col += uAccent * halo * (0.015 + 0.07 * mist) * uMoonlight;
    col += mix(uAccent, vec3(1.0), 0.5) * exp(-max(moonDist - uMoonRadius, 0.0) * 14.0) * 0.025 * uMoonlight;

    // A hint of grain, so the gradients never band.
    col += (hash21(gl_FragCoord.xy + fract(uTime)) - 0.5) * 0.012;
    gl_FragColor = vec4(col, 1.0);
    #include <colorspace_fragment>
  }
`;

// Linear data, not display colour: R = encoded height, G = basalt coverage,
// B = reflectance variation/ejecta, A = cavity visibility for indirect light only.
// All geological noise is sampled in object space, so the longitude seam joins exactly.
const BAKE_VERTEX = /* glsl */ `
  varying vec2 vUv;
  void main() {
    vUv = uv;
    gl_Position = vec4(position.xy, 0.0, 1.0);
  }
`;

const BAKE_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec2 vUv;
  ${NOISE}

  // Project a thin shell of seeded lattice points onto the sphere. Unlike intersecting a
  // 3D bowl field with the surface, every surviving impact has its centre on the ground.
  // The shell thickness plus the largest ejecta radius is < one cell: 27 neighbours suffice.
  // Return relief, fresh ejecta and cavity strength independently.
  vec3 craters(vec3 p, float scale, float density, float seed, float flooded) {
    vec3 q = p * scale;
    vec3 i = floor(q);
    vec3 result = vec3(0.0);
    for (int x = -1; x <= 1; x++) {
      for (int y = -1; y <= 1; y++) {
        for (int z = -1; z <= 1; z++) {
          vec3 c = i + vec3(float(x), float(y), float(z));
          vec3 key = c + seed;
          if (hash31(key + 19.1) > density) continue;
          vec3 o = vec3(hash31(key), hash31(key + 7.1), hash31(key + 13.7));
          vec3 centre = c + o;
          float centreLength = length(centre);
          if (abs(centreLength - scale) > 0.45) continue;
          centre *= scale / centreLength;
          float radius = 0.12 + 0.12 * hash31(key + 3.3);
          vec3 delta = q - centre;
          float d2 = dot(delta, delta) / (radius * radius);
          if (d2 > 4.41) continue;
          float d = sqrt(d2);
          float fresh = smoothstep(0.35, 0.95, hash31(key + 29.0));
          // Basalt buries old impacts; young impacts remain cut into the plains.
          float preserved = mix(1.0, mix(0.12, 1.0, fresh), flooded);
          float wall = smoothstep(0.23, 1.0, d);
          float bowl = -0.17 * (1.0 - wall);
          float rimDistance = (d - 1.0) / mix(0.18, 0.09, fresh);
          float rim = exp(-rimDistance * rimDistance) * 0.065;
          float complex = 1.0 - smoothstep(8.0, 18.0, scale);
          float peak = (1.0 - smoothstep(0.0, 0.24, d)) * 0.075 * complex;
          float terrace = sin(d * 38.0) * 0.009 * complex * fresh
            * smoothstep(0.35, 0.5, d) * (1.0 - smoothstep(0.8, 1.0, d));
          float envelope = 1.0 - smoothstep(1.15, 2.1, d);
          result.x += (bowl + rim + peak + terrace) * envelope * preserved;
          // Directional streaks fade into the surrounding stone; never darken a floor
          // merely because it is low. Direct-light visibility is a separate operation.
          vec3 radial = delta / max(length(delta), 0.0001);
          float rays = pow(noise3(radial * 19.0 + key), 4.0);
          result.y += fresh * preserved * envelope * smoothstep(0.9, 1.08, d)
            * (0.16 + 1.4 * rays);
          result.z += (1.0 - smoothstep(0.35, 1.05, d)) * 0.45 * preserved;
        }
      }
    }
    return result;
  }

  // Selected ancient basins, not an unrelated dark noise mask. The same shape lowers
  // the terrain, raises a worn mountain ring, and determines where basalt accumulated.
  vec2 basin(vec3 p, vec3 centre, float radius) {
    float d = length(p - normalize(centre)) / radius;
    d += (fbm3Coarse(p * 13.0) - 0.5) * 0.12;
    // Old basins have broad, broken margins, not a sharply outlined circular stain.
    float floorMask = 1.0 - smoothstep(0.40, 1.18, d);
    float ringDistance = (d - 1.08) / 0.22;
    float ring = exp(-ringDistance * ringDistance);
    return vec2(-0.12 * floorMask + 0.025 * ring, floorMask);
  }

  void main() {
    float lon = (vUv.x - 0.5) * 6.2831853;
    float lat = (vUv.y - 0.5) * 3.14159265;
    vec3 p = vec3(cos(lat) * cos(lon), sin(lat), cos(lat) * sin(lon));
    // A shared, continuous spherical warp creates lobes and channels between basins.
    // Apply it to both relief and coverage so colour still follows the geology.
    vec3 basinWarp = vec3(
      fbm3Coarse(p * 4.5 + vec3(3.1, 0.0, 0.0)),
      fbm3Coarse(p * 4.5 + vec3(0.0, 7.3, 0.0)),
      fbm3Coarse(p * 4.5 + vec3(0.0, 0.0, 11.7))
    ) - vec3(0.4375);
    vec3 basinPoint = normalize(p + basinWarp * 0.85);
    vec2 basins = basin(basinPoint, vec3(-0.45, 0.3, 1.0), 0.55);
    basins += basin(basinPoint, vec3(0.35, 0.55, 1.0), 0.34);
    basins += basin(basinPoint, vec3(0.65, -0.25, 0.7), 0.27);
    basins += basin(basinPoint, vec3(-0.5, -0.15, -1.0), 0.47);
    basins += basin(basinPoint, vec3(1.0, 0.45, -0.4), 0.32);
    float flooded = clamp(basins.y, 0.0, 1.0);
    // Different lava flows and exposed regolith interrupt uniform basalt coverage.
    // Keep this material variation separate from crater preservation and ground height.
    float lava = fbm3(p * 18.0 + vec3(8.0, 2.0, 5.0));
    float maria = flooded * (0.60 + 0.30 * lava);
    vec3 large = craters(p, 3.6, 0.8, 2.0, flooded);
    vec3 medium = craters(p, 9.0, 0.85, 17.0, flooded);
    vec3 small = craters(p, 21.0, 0.9, 41.0, flooded);
    vec3 micro = craters(p, 43.0, 0.9, 67.0, flooded);
    float highlands = (fbm3(p * 7.0) - 0.5) * 0.10;
    float h = basins.x + highlands * (1.0 - flooded * 0.92)
      + large.x + medium.x * 0.40 + small.x * 0.17 + micro.x * 0.075;
    float ejecta = large.y + medium.y * 0.75 + small.y * 0.45;
    float reflectance = clamp(0.48 + (fbm3Coarse(p * 32.0) - 0.5) * 0.20 + ejecta, 0.0, 1.0);
    float cavity = clamp(1.0 - large.z - medium.z * 0.6 - small.z * 0.3, 0.3, 1.0);
    // Use most of the available byte range; decode with (r - 0.5) * 0.8.
    gl_FragColor = vec4(clamp(h / 0.8 + 0.5, 0.0, 1.0), maria, reflectance, cavity);
  }
`;

const MOON_VERTEX = /* glsl */ `
  varying vec3 vObject;
  void main() {
    vObject = position;
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const MOON_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec3 vObject;
  uniform sampler2D uSurface;
  uniform float uNormalStep;
  uniform float uSurfaceLod;
  uniform int uShadowSteps;
  uniform float uShadowBias;
  uniform vec3 uLight;  // in object space
  uniform vec3 uView;   // in object space
  uniform vec3 uShadow;
  uniform vec3 uLit;
  uniform vec3 uMaria;

  vec2 equirect(vec3 p) {
    // atan(0, 0) is undefined; longitude is immaterial at the exact pole.
    float longitude = dot(p.xz, p.xz) > 1e-12 ? atan(p.z, p.x) : 0.0;
    return vec2(longitude / 6.2831853 + 0.5, asin(clamp(p.y, -1.0, 1.0)) / 3.14159265 + 0.5);
  }
  vec4 surfaceAt(vec2 uv) {
    // Three r180 requires WebGL2. Explicit LOD avoids false coarse mip selection at
    // the longitude seam, and undefined implicit derivatives inside shadow branches.
    return textureLod(uSurface, uv, uSurfaceLod);
  }
  float heightAt(vec2 uv) {
    return (surfaceAt(uv).r - 0.5) * 0.8;
  }

  const float RELIEF = 0.035;

  float terrainVisibility(vec3 n, vec3 light, float height) {
    float elevation = dot(n, light);
    if (elevation <= -0.012 || elevation >= 0.65) return 1.0;
    vec3 origin = n * (1.0 + height * RELIEF);
    float visibility = 1.0;
    // Short steps resolve nearby rims; progressively longer ones reach distant ridges.
    // This samples a spherical radial height field, not a flat tangent-plane proxy.
    for (int i = 0; i < 16; i++) {
      if (i >= uShadowSteps) break;
      float stepIndex = float(i + 1);
      float distance = 0.003 * stepIndex + 0.0006 * stepIndex * stepIndex;
      vec3 ray = origin + light * distance;
      float radius = length(ray);
      // The encoded height cannot exceed 0.4. Once outside this shell, an
      // outward-moving ray cannot hit terrain again.
      if (radius > 1.0 + 0.4 * RELIEF && dot(ray, light) > 0.0) break;
      float terrain = 1.0 + heightAt(equirect(ray / radius)) * RELIEF;
      float clearance = radius - terrain + uShadowBias;
      // Approximate the sun's finite angular radius, not an exact area-light integral.
      float softness = 0.00008 + distance * 0.00465;
      visibility = min(visibility, smoothstep(-softness, softness, clearance));
    }
    // Fade out the tracing approximation where sunlight is steep enough that
    // normal-based shading is sufficient, without a visible branch boundary.
    return mix(visibility, 1.0, smoothstep(0.40, 0.65, elevation));
  }

  void main() {
    vec3 n = normalize(vObject);
    vec2 uv = equirect(n);
    vec4 surface = surfaceAt(uv);
    float maria = surface.g;
    // Equal angular offsets on the sphere avoid equirectangular pole singularities.
    // Enlarge the footprint on small discs so unresolved relief does not sparkle.
    vec3 reference = abs(n.y) < 0.9 ? vec3(0.0, 1.0, 0.0) : vec3(1.0, 0.0, 0.0);
    vec3 east = normalize(cross(reference, n));
    vec3 north = cross(n, east);
    float stepSize = uNormalStep;
    float sx = heightAt(equirect(normalize(n + east * stepSize)))
      - heightAt(equirect(normalize(n - east * stepSize)));
    float sy = heightAt(equirect(normalize(n + north * stepSize)))
      - heightAt(equirect(normalize(n - north * stepSize)));
    // Heights are fractions of the unit sphere's radius, not arbitrary bump intensity.
    vec3 bumped = normalize(n - (east * sx + north * sy) * (RELIEF / (2.0 * stepSize)));

    vec3 albedo = mix(uLit, uMaria, maria * 0.65);
    albedo *= 0.78 + 0.44 * surface.b;

    vec3 light = normalize(uLight);
    vec3 view = normalize(uView);
    float mu0 = max(dot(bumped, light), 0.0);
    float mu = max(dot(bumped, view), 0.0);
    float terminator = smoothstep(-0.008, 0.012, dot(n, light));
    // Lommel-Seeliger single scattering blended with Lambert multiple scattering.
    // This is an artistic regolith approximation, not a calibrated Hapke BRDF.
    float lunar = mix(mu0 / max(mu0 + mu, 0.025), mu0, 0.28);
    float phase = acos(clamp(dot(light, view), -1.0, 1.0));
    float opposition = 1.0 + 0.16 * exp(-phase / 0.09);
    float visibility = terrainVisibility(n, light, (surface.r - 0.5) * 0.8);
    vec3 col = albedo * lunar * opposition * terminator * visibility * 1.15;
    // Weak observer-facing earthshine; cavity visibility attenuates only this indirect term.
    col += albedo * uShadow * (0.2 + 0.8 * mu) * surface.a;
    gl_FragColor = vec4(col, 1.0);
    #include <colorspace_fragment>
  }
`;

// Observer-side haze, not a lunar atmosphere or an emissive shell. The billboard
// stays in the camera plane; only its brightness follows the illuminated fraction.
const CORONA_VERTEX = /* glsl */ `
  varying vec2 vUv;
  void main() {
    vUv = uv;
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const CORONA_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec2 vUv;
  uniform vec3 uAccent;
  uniform float uMoonlight;
  void main() {
    float d = length(vUv - 0.5) * 2.0;
    float edge = 1.0 - smoothstep(0.7, 1.0, d);
    float a = (exp(-d * d * 5.5) * 0.07 + exp(-d * 9.0) * 0.04) * edge * uMoonlight;
    gl_FragColor = vec4(mix(uAccent, vec3(1.0), 0.25), a);
    #include <colorspace_fragment>
  }
`;

// Dust: point sprites rising through the light, swaying, blinking.
const DUST_VERTEX = /* glsl */ `
  attribute float aSeed;
  attribute float aSize;
  varying float vAlpha;
  uniform float uTime;
  uniform float uHeight;
  uniform float uPixelRatio;
  uniform vec3 uMoonPosition;
  void main() {
    float rise = mod(position.y + uTime * (0.08 + 0.12 * aSeed) + aSeed * uHeight, uHeight) - uHeight * 0.5;
    float sway = sin(uTime * (0.4 + aSeed) + aSeed * 31.0) * 0.12;
    vec3 p = vec3(position.x + sway, rise, position.z);
    float near = clamp(1.0 - length(p - uMoonPosition) / 4.5, 0.0, 1.0);
    vAlpha = (0.25 + 0.75 * near) * (0.55 + 0.45 * sin(uTime * (1.5 + 2.0 * aSeed) + aSeed * 90.0));
    vec4 mv = modelViewMatrix * vec4(p, 1.0);
    gl_PointSize = aSize * uPixelRatio * (0.8 + 0.4 * aSeed);
    gl_Position = projectionMatrix * mv;
  }
`;

const DUST_FRAGMENT = /* glsl */ `
  precision highp float;
  varying float vAlpha;
  uniform vec3 uAccent;
  void main() {
    float d = length(gl_PointCoord - 0.5) * 2.0;
    float a = (1.0 - smoothstep(0.1, 1.0, d)) * vAlpha;
    gl_FragColor = vec4(mix(uAccent, vec3(1.0), 0.4) * a, a);
    #include <colorspace_fragment>
  }
`;

function start(hero, art) {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;

  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: false, powerPreference: 'low-power' });
  } catch {
    return; // no WebGL: the still in brand.sass stays
  }
  const canvas = renderer.domElement;
  canvas.className = 'l3i-sky';
  canvas.setAttribute('aria-hidden', 'true');
  if (art) art.append(canvas);
  else hero.prepend(canvas);

  const accent = cssColor('--dw-accent', '#c9a4ff');
  const bg0 = cssColor('--dw-bg-0', '#0a0710');
  const bg1 = cssColor('--dw-bg-1', '#100b19');
  const pageWidth = cssLength('--dw-width-page', 1600);
  const rem = Number.parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;

  const scene = new THREE.Scene();
  // Orthographic: a pixel is a fixed fraction of a world unit, so the moon lands exactly where
  // the layout puts it, and a sphere off the axis still projects as a disc.
  const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 50);
  camera.position.set(0, 0, 7);
  const HALF_HEIGHT = 1.875;

  // Palette derived from the tokens: mist from black-violet to the accent, the moon's stone in
  // the accent's hue.
  const mistDeep = bg0.clone().lerp(accent, 0.28);
  const mistBright = accent.clone().lerp(new THREE.Color('#ffffff'), 0.12);
  const skyTop = bg1.clone().lerp(accent, 0.06);
  const skyBottom = bg0.clone();
  const litStone = new THREE.Color('#c7c3c0').lerp(accent, 0.12);
  const maria = new THREE.Color('#555360').lerp(accent, 0.08);
  const shadow = accent.clone().multiplyScalar(0.028);

  const sky = new THREE.Mesh(
    new THREE.PlaneGeometry(2, 2),
    new THREE.ShaderMaterial({
      vertexShader: SKY_VERTEX,
      fragmentShader: SKY_FRAGMENT,
      depthTest: false,
      depthWrite: false,
      uniforms: {
        uTime: { value: 0 },
        uResolution: { value: new THREE.Vector2(1, 1) },
        uMoon: { value: new THREE.Vector2(1.2, 0.5) },
        uMoonRadius: { value: 0.2 },
        uMoonlight: { value: 0.7 },
        uDrift: { value: new THREE.Vector2(0, 0) },
        uBgTop: { value: skyTop },
        uBgBottom: { value: skyBottom },
        uMistDeep: { value: mistDeep },
        uMistBright: { value: mistBright },
        uAccent: { value: accent },
        uNarrow: { value: 0 },
      },
    }),
  );
  sky.frustumCulled = false;
  sky.renderOrder = -10;
  scene.add(sky);

  const moonGroup = new THREE.Group();
  scene.add(moonGroup);

  // Bake the surface: 2048 by 1024 texels, half floats where the platform renders to them
  // (WebGL2 with float colour buffers), bytes otherwise; the height is stored in 0..1 either way.
  const halfFloat = renderer.extensions.has('EXT_color_buffer_float') || renderer.extensions.has('EXT_color_buffer_half_float');
  const bakeSize = new THREE.Vector2(2048, 1024);
  const surface = new THREE.WebGLRenderTarget(bakeSize.x, bakeSize.y, {
    type: halfFloat ? THREE.HalfFloatType : THREE.UnsignedByteType,
    format: THREE.RGBAFormat,
    minFilter: THREE.LinearMipmapLinearFilter,
    magFilter: THREE.LinearFilter,
    generateMipmaps: true,
    wrapS: THREE.RepeatWrapping,
    wrapT: THREE.ClampToEdgeWrapping,
    depthBuffer: false,
    stencilBuffer: false,
  });
  {
    const bakeScene = new THREE.Scene();
    const bakeQuad = new THREE.Mesh(
      new THREE.PlaneGeometry(2, 2),
      new THREE.ShaderMaterial({ vertexShader: BAKE_VERTEX, fragmentShader: BAKE_FRAGMENT, depthTest: false, depthWrite: false }),
    );
    bakeQuad.frustumCulled = false;
    bakeScene.add(bakeQuad);
    renderer.setRenderTarget(surface);
    renderer.render(bakeScene, camera);
    renderer.setRenderTarget(null);
    bakeQuad.geometry.dispose();
    bakeQuad.material.dispose();
  }

  const moonUniforms = {
    uSurface: { value: surface.texture },
    uNormalStep: { value: 2 * Math.PI / bakeSize.x },
    uSurfaceLod: { value: 0 },
    uShadowSteps: { value: 16 },
    // Byte heights need a larger bias to avoid quantization becoming self-shadow acne.
    uShadowBias: { value: halfFloat ? 0.00006 : 0.00016 },
    uLight: { value: new THREE.Vector3(-1.2, 0.55, 0.6) },
    uView: { value: new THREE.Vector3(0, 0, 1) },
    uShadow: { value: shadow },
    uLit: { value: litStone },
    uMaria: { value: maria },
  };
  const moon = new THREE.Mesh(
    new THREE.SphereGeometry(1, 96, 72),
    new THREE.ShaderMaterial({ vertexShader: MOON_VERTEX, fragmentShader: MOON_FRAGMENT, uniforms: moonUniforms }),
  );
  moonGroup.add(moon);

  const coronaUniforms = { uAccent: { value: accent }, uMoonlight: { value: 0.7 } };
  const corona = new THREE.Mesh(
    new THREE.PlaneGeometry(6.4, 6.4),
    new THREE.ShaderMaterial({
      vertexShader: CORONA_VERTEX,
      fragmentShader: CORONA_FRAGMENT,
      uniforms: coronaUniforms,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    }),
  );
  corona.renderOrder = -5;
  scene.add(corona);

  const dustCount = 420;
  const dustGeometry = new THREE.BufferGeometry();
  const positions = new Float32Array(dustCount * 3);
  const seeds = new Float32Array(dustCount);
  const sizes = new Float32Array(dustCount);
  for (let i = 0; i < dustCount; i++) {
    positions[i * 3] = (Math.random() - 0.5) * 14;
    positions[i * 3 + 1] = (Math.random() - 0.5) * 6;
    positions[i * 3 + 2] = (Math.random() - 0.5) * 5 - 0.5;
    seeds[i] = Math.random();
    sizes[i] = 1.5 + Math.random() * 3.5;
  }
  dustGeometry.setAttribute('position', new THREE.BufferAttribute(positions, 3));
  dustGeometry.setAttribute('aSeed', new THREE.BufferAttribute(seeds, 1));
  dustGeometry.setAttribute('aSize', new THREE.BufferAttribute(sizes, 1));
  const dustUniforms = {
    uTime: { value: 0 },
    uHeight: { value: 6 },
    uPixelRatio: { value: 1 },
    uAccent: { value: accent },
    uMoonPosition: { value: new THREE.Vector3() },
  };
  const dust = new THREE.Points(
    dustGeometry,
    new THREE.ShaderMaterial({
      vertexShader: DUST_VERTEX,
      fragmentShader: DUST_FRAGMENT,
      uniforms: dustUniforms,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    }),
  );
  dust.frustumCulled = false;
  scene.add(dust);

  // Layout: the moon sits in the hero's empty right column when there is one, and in the top
  // right corner, above the text, when the column would land on the text. The text column's own
  // width decides, not a breakpoint, so a landscape phone and a narrow window get the corner. The
  // status strip runs across the hero's foot, under both columns: the moon hangs in the band above
  // it, so the strip's glass never cuts the disc.
  let width = 1;
  let height = 1;
  let narrow = false;
  const moonOnScreen = { x: 0, y: 0, radius: 1 };
  function layout() {
    width = Math.max(hero.clientWidth, 1);
    height = Math.max(hero.clientHeight, 1);
    const origin = canvas.getBoundingClientRect();
    const column = hero.querySelector('.dw-hero__text, .dw-hero__grid > div');
    const textRight = column ? column.getBoundingClientRect().right - origin.left : width;
    const strip = hero.querySelector('.dw-hero__grid > .dw-strip');
    const band = strip ? Math.min(Math.max(strip.getBoundingClientRect().top - origin.top, height * 0.5), height) : height;
    const shell = Math.min(width, pageWidth);
    const wideCentre = width / 2 + shell / 2 - 14 * rem;
    const wideRadius = Math.min(height * 0.34, band * 0.42, 10.5 * rem);
    narrow = width < 761 || wideCentre - wideRadius < textRight + rem;
    hero.classList.toggle('l3i-hero--corner', narrow);
    const pixels = width * height;
    const ratio = Math.min(window.devicePixelRatio || 1, pixels > 1.6e6 ? 1 : 1.25);
    renderer.setPixelRatio(ratio);
    renderer.setSize(width, height, false);
    const halfHeight = HALF_HEIGHT;
    const halfWidth = halfHeight * (width / height);
    camera.left = -halfWidth;
    camera.right = halfWidth;
    camera.top = halfHeight;
    camera.bottom = -halfHeight;
    camera.updateProjectionMatrix();
    sky.material.uniforms.uResolution.value.set(width, height);
    sky.material.uniforms.uNarrow.value = narrow ? 1 : 0;
    dustUniforms.uPixelRatio.value = ratio;

    let centreX;
    let centreY;
    let radiusPx;
    if (narrow) {
      // Above the kicker, off to the right: the text runs the whole width below it.
      centreX = width * 0.8;
      centreY = 2.6 * rem;
      radiusPx = Math.min(width * 0.13, 4.4 * rem);
    } else {
      centreX = wideCentre;
      centreY = band * 0.5;
      radiusPx = wideRadius;
    }
    moonOnScreen.x = centreX;
    moonOnScreen.y = centreY;
    moonOnScreen.radius = radiusPx;
    moonUniforms.uNormalStep.value = Math.max(2 * Math.PI / bakeSize.x, 0.75 / (radiusPx * ratio));
    moonUniforms.uSurfaceLod.value = Math.max(0, Math.log2(moonUniforms.uNormalStep.value * bakeSize.x / (2 * Math.PI)));
    moonUniforms.uShadowSteps.value = radiusPx * ratio < 100 ? 8 : 16;
    const worldRadius = (radiusPx / height) * 2 * halfHeight;
    const nx = (centreX / width) * 2 - 1;
    const ny = 1 - (centreY / height) * 2;
    moonGroup.position.set(nx * halfWidth, ny * halfHeight, 0);
    moonGroup.scale.setScalar(worldRadius);
    corona.position.copy(moonGroup.position);
    corona.position.z = -worldRadius * 0.6;
    corona.scale.setScalar(worldRadius);
    dustUniforms.uMoonPosition.value.copy(moonGroup.position);
    sky.material.uniforms.uMoon.value.set((centreX / width) * (width / height), 1 - centreY / height);
    sky.material.uniforms.uMoonRadius.value = radiusPx / height;
  }
  layout();
  new ResizeObserver(layout).observe(hero);

  // Pointer parallax, eased; nothing moves under a reduced-motion preference.
  const pointer = new THREE.Vector2();
  const eased = new THREE.Vector2();
  if (!reduced) {
    window.addEventListener('pointermove', (event) => {
      pointer.set((event.clientX / window.innerWidth) * 2 - 1, (event.clientY / window.innerHeight) * 2 - 1);
    }, { passive: true });
  }

  // The render loop, defined below; a drag wakes it.
  let resume = () => {};

  // Drag the moon: the disc follows the pointer about the screen's axes and keeps turning
  // after release, the spin decaying, while the slow rotation of its own carries on beneath.
  const dragRotation = new THREE.Quaternion();
  const spinRotation = new THREE.Quaternion();
  const tilt = new THREE.Quaternion().setFromAxisAngle(new THREE.Vector3(0, 0, 1), 0.25);
  const turn = new THREE.Quaternion();
  const axisX = new THREE.Vector3(1, 0, 0);
  const axisY = new THREE.Vector3(0, 1, 0);
  const drag = { active: false, id: -1, lastX: 0, lastY: 0, vx: 0, vy: 0 };
  let spin = 0;
  function overMoon(event) {
    const box = canvas.getBoundingClientRect();
    const dx = event.clientX - box.left - moonOnScreen.x;
    const dy = event.clientY - box.top - moonOnScreen.y;
    return dx * dx + dy * dy <= moonOnScreen.radius * moonOnScreen.radius;
  }
  function rotateBy(dx, dy) {
    const perPixel = 1.6 / moonOnScreen.radius;
    turn.setFromAxisAngle(axisY, dx * perPixel);
    dragRotation.premultiply(turn);
    turn.setFromAxisAngle(axisX, dy * perPixel);
    dragRotation.premultiply(turn);
  }
  canvas.addEventListener('pointermove', (event) => {
    if (drag.active) return;
    canvas.style.cursor = overMoon(event) ? 'grab' : '';
  });
  canvas.addEventListener('pointerdown', (event) => {
    if (!event.isPrimary || !overMoon(event)) return;
    drag.active = true;
    drag.id = event.pointerId;
    drag.lastX = event.clientX;
    drag.lastY = event.clientY;
    drag.vx = 0;
    drag.vy = 0;
    canvas.setPointerCapture(event.pointerId);
    canvas.style.cursor = 'grabbing';
    event.preventDefault();
    resume();
  });
  canvas.addEventListener('pointermove', (event) => {
    if (!drag.active || event.pointerId !== drag.id) return;
    const dx = event.clientX - drag.lastX;
    const dy = event.clientY - drag.lastY;
    drag.lastX = event.clientX;
    drag.lastY = event.clientY;
    drag.vx = dx;
    drag.vy = dy;
    rotateBy(dx, dy);
    if (reduced) frame(0);
  });
  function release(event) {
    if (!drag.active || event.pointerId !== drag.id) return;
    drag.active = false;
    canvas.style.cursor = overMoon(event) ? 'grab' : '';
    if (reduced) {
      drag.vx = 0;
      drag.vy = 0;
    }
  }
  canvas.addEventListener('pointerup', release);
  canvas.addEventListener('pointercancel', release);

  const lightWorld = new THREE.Vector3();
  const worldRotation = new THREE.Quaternion();
  const clock = new THREE.Clock();
  let elapsed = 0;
  let framesDrawn = 0;
  function frame(delta) {
    elapsed += delta;
    eased.lerp(pointer, 1 - Math.exp(-delta * 1.4));
    sky.material.uniforms.uTime.value = elapsed;
    sky.material.uniforms.uDrift.value.set(eased.x * 0.02, -eased.y * 0.014);
    dustUniforms.uTime.value = elapsed;

    // The sun swings across the moon over about two minutes, always from the camera's side, so
    // the terminator wanders without the disc ever going dark.
    const angle = -1.05 + 0.4 * Math.sin(elapsed * 0.05);
    lightWorld.set(Math.sin(angle) * 1.4, 0.55 + 0.2 * Math.cos(elapsed * 0.033), Math.cos(angle) + 0.15);
    // Inertia after a drag decays over about a second and a half; the moon's own spin carries
    // on underneath either way.
    if (!drag.active && (drag.vx !== 0 || drag.vy !== 0)) {
      const decay = Math.exp(-delta * 2.2);
      drag.vx *= decay;
      drag.vy *= decay;
      if (Math.abs(drag.vx) + Math.abs(drag.vy) < 0.02) {
        drag.vx = 0;
        drag.vy = 0;
      } else {
        rotateBy(drag.vx * delta * 60, drag.vy * delta * 60);
      }
    }
    spin += delta * 0.035;
    spinRotation.setFromAxisAngle(axisY, spin);
    moon.quaternion.copy(dragRotation).multiply(tilt).multiply(spinRotation);
    moonGroup.rotation.x = eased.y * 0.05;
    moonGroup.rotation.y = eased.x * 0.06;
    // The surface shader lights in object space, where the sphere's normal is exact.
    moon.getWorldQuaternion(worldRotation).invert();
    moonUniforms.uLight.value.copy(lightWorld).applyQuaternion(worldRotation);
    moonUniforms.uView.value.set(0, 0, 1).applyQuaternion(worldRotation);
    const moonlight = (1 + lightWorld.z / lightWorld.length()) * 0.5;
    sky.material.uniforms.uMoonlight.value = moonlight;
    coronaUniforms.uMoonlight.value = moonlight;
    corona.quaternion.copy(camera.quaternion);
    renderer.render(scene, camera);
    framesDrawn += 1;
    if (framesDrawn === 1) {
      canvas.classList.add('is-ready');
      setTimeout(() => hero.classList.add('l3i-hero--live'), 1000);
    }
  }

  let visible = true;
  let running = false;
  if (reduced) {
    frame(0);
    frame(0);
    resume = () => {};
    return;
  }
  function tick() {
    if (!visible || document.hidden) {
      running = false;
      return;
    }
    frame(Math.min(clock.getDelta(), 0.1));
    requestAnimationFrame(tick);
  }
  resume = function () {
    if (running || !visible || document.hidden) return;
    running = true;
    clock.getDelta();
    requestAnimationFrame(tick);
  }
  new IntersectionObserver((entries) => {
    visible = entries.some((entry) => entry.isIntersecting);
    resume();
  }, { threshold: 0 }).observe(hero);
  document.addEventListener('visibilitychange', resume);
  resume();
}

const art = document.querySelector('[data-dw-hero-art]');
const hero = art ? art.closest('.dw-hero') || art.parentElement : document.querySelector('.dw-hero');
if (hero) start(hero, art);
