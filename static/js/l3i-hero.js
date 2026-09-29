// The l3i hero: a night sky drawn live with three.js behind the project page's header.
//
// Three layers, back to front. A full-screen shader draws stars and a volumetric purple mist, two
// domain-warped noise fields drifting at different speeds, lit by the moon's position on screen.
// A sphere with a procedural surface (maria from fractal noise, craters from a cellular field,
// bump-mapped by finite differences) is the moon, lit by a sun that swings slowly across it so the
// terminator moves, with a specular highlight, purple earthshine on the dark side, a fresnel rim
// and an additive halo. Dust rises through the moonlight as point sprites.
//
// The palette is read from the site's CSS tokens, so sass/brand.sass stays the single owner of
// the colours. The canvas is inert until the hero is on screen, stops when the tab is hidden, caps
// the device pixel ratio, and under prefers-reduced-motion draws one frame and stops. Without
// WebGL nothing is added: sass/brand.sass draws a still of the same scene.

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
      vec2 g = (p + uDrift * (0.6 + 0.3 * float(layer))) * scale + float(layer) * 31.7;
      vec2 cell = floor(g);
      vec2 f = fract(g) - 0.5;
      float h = hash21(cell + float(layer) * 7.3);
      vec2 offset = vec2(hash21(cell + 1.1), hash21(cell + 2.2)) - 0.5;
      float d = length(f - offset * 0.7);
      float twinkle = 0.55 + 0.45 * sin(uTime * (0.8 + 1.7 * h) + h * 40.0);
      float star = smoothstep(0.09, 0.0, d) * step(0.965, h) * twinkle;
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
    float glow = exp(-moonDist * 2.1) + 0.35 * exp(-moonDist * 0.9);
    vec3 mistColor = mix(uMistDeep, uMistBright, clamp(mist * 1.2, 0.0, 1.0));
    mistColor = mix(mistColor, mix(uMistBright, vec3(1.0), 0.35), clamp(glow * 0.9, 0.0, 1.0));
    col = mix(col, mistColor, clamp(mist * (0.55 + 0.45 * glow), 0.0, 0.95));

    // The halo: moonlight scattered by the mist itself, strongest where the mist is.
    float halo = exp(-max(moonDist - uMoonRadius, 0.0) * 3.2);
    col += uAccent * halo * (0.10 + 0.35 * mist);
    col += mix(uAccent, vec3(1.0), 0.5) * exp(-max(moonDist - uMoonRadius, 0.0) * 14.0) * 0.22;

    // A hint of grain, so the gradients never band.
    col += (hash21(gl_FragCoord.xy + fract(uTime)) - 0.5) * 0.012;
    gl_FragColor = vec4(col, 1.0);
  }
`;

// The moon's surface.
const MOON_VERTEX = /* glsl */ `
  varying vec3 vNormal;
  varying vec3 vObject;
  varying vec3 vTangent1;
  varying vec3 vTangent2;
  void main() {
    vObject = position;
    mat3 toWorld = mat3(modelMatrix);
    vNormal = normalize(toWorld * normal);
    // A tangent frame per vertex, in object space; the fragment samples the height field along
    // it and tilts the world normal by the same directions in world space.
    vec3 t1 = normalize(cross(normal, abs(normal.y) < 0.99 ? vec3(0.0, 1.0, 0.0) : vec3(1.0, 0.0, 0.0)));
    vec3 t2 = cross(normal, t1);
    vTangent1 = normalize(toWorld * t1);
    vTangent2 = normalize(toWorld * t2);
    gl_Position = projectionMatrix * viewMatrix * modelMatrix * vec4(position, 1.0);
  }
`;

const MOON_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec3 vNormal;
  varying vec3 vObject;
  varying vec3 vTangent1;
  varying vec3 vTangent2;
  uniform vec3 uLight;
  uniform vec3 uAccent;
  uniform vec3 uShadow;
  uniform vec3 uLit;
  uniform vec3 uMaria;
  ${NOISE}

  // A bowl per cell of a jittered lattice: a pit, a raised rim, a flat floor for the wide ones.
  float craters(vec3 p, float scale, float density) {
    vec3 q = p * scale;
    vec3 i = floor(q);
    float h = 0.0;
    for (int x = -1; x <= 1; x++) {
      for (int y = -1; y <= 1; y++) {
        for (int z = -1; z <= 1; z++) {
          vec3 c = i + vec3(float(x), float(y), float(z));
          if (hash31(c + 19.1) > density) continue;
          vec3 o = vec3(hash31(c), hash31(c + 7.1), hash31(c + 13.7));
          float radius = 0.22 + 0.33 * hash31(c + 3.3);
          float d = length(c + o - q) / radius;
          if (d < 1.2) {
            // A flat floor, a wall from a third of the way out, a low raised rim outside it.
            float bowl = -0.16 * (1.0 - smoothstep(0.25, 1.0, d));
            float rim = 0.07 * smoothstep(0.7, 1.0, d) * smoothstep(1.2, 1.0, d);
            h += bowl + rim;
          }
        }
      }
    }
    return h;
  }

  float height(vec3 p) {
    float plains = fbm3(p * 2.2) * 0.3;
    float fine = fbm3Coarse(p * 5.0 + 4.0) * 0.3;
    float big = craters(p, 2.4, 0.3) * 1.1;
    float medium = craters(p + 5.0, 5.0, 0.28) * 0.6;
    float small = craters(p + 11.0, 8.0, 0.3) * 0.25;
    return plains + fine + big + medium + small;
  }

  void main() {
    vec3 n = normalize(vNormal);
    vec3 op = normalize(vObject);

    // Bump mapping by finite differences of the height field in the tangent plane.
    vec3 t1 = normalize(cross(op, abs(op.y) < 0.99 ? vec3(0.0, 1.0, 0.0) : vec3(1.0, 0.0, 0.0)));
    vec3 t2 = cross(op, t1);
    float e = 0.02;
    float h0 = height(op);
    float h1 = height(normalize(op + t1 * e));
    float h2 = height(normalize(op + t2 * e));
    vec3 wt1 = normalize(vTangent1);
    vec3 wt2 = normalize(vTangent2);
    float strength = 0.55;
    vec3 bumped = normalize(n - (wt1 * (h1 - h0) + wt2 * (h2 - h0)) * (strength / e));

    // Maria: the dark plains, where the low-frequency field is low.
    float maria = smoothstep(0.44, 0.58, fbm3(op * 1.6 + 2.0));
    vec3 albedo = mix(uLit, uMaria, maria * 0.85);
    albedo *= 0.88 + 0.24 * fbm3Coarse(op * 6.0);

    // The camera is orthographic and looks down -z, so every fragment is seen along +z.
    vec3 view = vec3(0.0, 0.0, 1.0);
    vec3 light = normalize(uLight);
    float diffuse = max(dot(bumped, light), 0.0);
    float wrap = max((dot(bumped, light) + 0.25) / 1.25, 0.0);
    float terminator = smoothstep(-0.12, 0.22, dot(n, light));
    vec3 halfway = normalize(light + view);
    float spec = pow(max(dot(n, halfway), 0.0), 24.0) * 0.22 * terminator;
    float fresnel = pow(1.0 - max(dot(n, view), 0.0), 3.5);

    // Crater floors sit in shadow, rims catch the light.
    albedo *= 0.78 + 0.35 * clamp(h0 * 0.8 + 0.6, 0.0, 1.0);

    // Earthshine: the dark side glows faintly purple, from the sky it hangs in.
    vec3 ambient = uShadow + uAccent * 0.14 * (0.5 + 0.5 * dot(n, vec3(0.0, -0.4, 0.9)));
    vec3 col = albedo * (diffuse * 0.9 + wrap * 0.25) * vec3(1.0, 0.97, 1.0);
    col += ambient * (1.0 - terminator * 0.85) * 0.6;
    col += spec * vec3(1.0, 0.96, 1.0);
    col += uAccent * fresnel * (0.35 + 0.45 * terminator);
    gl_FragColor = vec4(col, 1.0);
  }
`;

// The halo: a fresnel shell just outside the surface, added to whatever is behind.
const HALO_VERTEX = /* glsl */ `
  varying vec3 vNormal;
  void main() {
    vNormal = normalize(mat3(modelMatrix) * normal);
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const HALO_FRAGMENT = /* glsl */ `
  precision highp float;
  varying vec3 vNormal;
  uniform vec3 uAccent;
  uniform float uPulse;
  void main() {
    vec3 view = vec3(0.0, 0.0, 1.0);
    float rim = pow(1.0 - max(dot(normalize(vNormal), view), 0.0), 3.5);
    gl_FragColor = vec4(uAccent * rim * (0.9 + 0.25 * uPulse), rim);
  }
`;

// The corona: a billboard behind the moon with a wide, soft falloff.
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
  uniform float uPulse;
  void main() {
    float d = length(vUv - 0.5) * 2.0;
    float a = exp(-d * d * 5.5) * 0.45 + exp(-d * 9.0) * 0.35;
    a *= 1.0 + 0.12 * uPulse;
    gl_FragColor = vec4(mix(uAccent, vec3(1.0), 0.25) * a, a);
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
    float a = smoothstep(1.0, 0.1, d) * vAlpha;
    gl_FragColor = vec4(mix(uAccent, vec3(1.0), 0.4) * a, a);
  }
`;

function start(hero) {
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
  hero.prepend(canvas);
  hero.classList.add('l3i-hero--live');

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
  const litStone = accent.clone().lerp(new THREE.Color('#ffffff'), 0.7);
  const maria = accent.clone().lerp(new THREE.Color('#24163d'), 0.62);
  const shadow = bg0.clone().lerp(accent, 0.18);

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

  const moonUniforms = {
    uLight: { value: new THREE.Vector3(-1.2, 0.55, 0.6) },
    uAccent: { value: accent },
    uShadow: { value: shadow },
    uLit: { value: litStone },
    uMaria: { value: maria },
  };
  const moon = new THREE.Mesh(
    new THREE.SphereGeometry(1, 128, 96),
    new THREE.ShaderMaterial({ vertexShader: MOON_VERTEX, fragmentShader: MOON_FRAGMENT, uniforms: moonUniforms }),
  );
  moon.rotation.z = 0.25;
  moonGroup.add(moon);

  const haloUniforms = { uAccent: { value: accent }, uPulse: { value: 0 } };
  const halo = new THREE.Mesh(
    new THREE.SphereGeometry(1.045, 96, 64),
    new THREE.ShaderMaterial({
      vertexShader: HALO_VERTEX,
      fragmentShader: HALO_FRAGMENT,
      uniforms: haloUniforms,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    }),
  );
  moonGroup.add(halo);

  const coronaUniforms = { uAccent: { value: accent }, uPulse: { value: 0 } };
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
  corona.position.z = -0.6;
  corona.renderOrder = -5;
  moonGroup.add(corona);

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

  // Layout: the moon sits in the hero's empty right column on wide screens and in the top right
  // corner on narrow ones, sized from the hero itself.
  let width = 1;
  let height = 1;
  let narrow = false;
  function layout() {
    width = Math.max(hero.clientWidth, 1);
    height = Math.max(hero.clientHeight, 1);
    narrow = width < 761;
    const pixels = width * height;
    const ratio = Math.min(window.devicePixelRatio || 1, pixels > 1.6e6 ? 1.25 : 1.5);
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
      const shell = Math.min(width, pageWidth);
      centreX = width / 2 + shell / 2 - 14 * rem;
      centreY = height * 0.5;
      radiusPx = Math.min(height * 0.34, 10.5 * rem);
    }
    const worldRadius = (radiusPx / height) * 2 * halfHeight;
    const nx = (centreX / width) * 2 - 1;
    const ny = 1 - (centreY / height) * 2;
    moonGroup.position.set(nx * halfWidth, ny * halfHeight, 0);
    moonGroup.scale.setScalar(worldRadius);
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

  const clock = new THREE.Clock();
  let elapsed = 0;
  function frame(delta) {
    elapsed += delta;
    eased.lerp(pointer, 1 - Math.exp(-delta * 3));
    sky.material.uniforms.uTime.value = elapsed;
    sky.material.uniforms.uDrift.value.set(eased.x * 0.03, -eased.y * 0.02);
    dustUniforms.uTime.value = elapsed;

    // The sun swings across the moon over about two minutes, always from the camera's side, so
    // the terminator wanders without the disc ever going dark.
    const angle = -1.05 + 0.4 * Math.sin(elapsed * 0.05);
    moonUniforms.uLight.value.set(Math.sin(angle) * 1.4, 0.55 + 0.2 * Math.cos(elapsed * 0.033), Math.cos(angle) + 0.15);
    moon.rotation.y = elapsed * 0.035;
    moonGroup.rotation.x = eased.y * 0.05;
    moonGroup.rotation.y = eased.x * 0.06;
    const pulse = 0.5 + 0.5 * Math.sin(elapsed * 0.7);
    haloUniforms.uPulse.value = pulse;
    coronaUniforms.uPulse.value = pulse;
    corona.quaternion.copy(camera.quaternion);
    renderer.render(scene, camera);
  }

  if (reduced) {
    frame(0);
    frame(0);
    return;
  }

  let visible = true;
  let running = false;
  function tick() {
    if (!visible || document.hidden) {
      running = false;
      return;
    }
    frame(Math.min(clock.getDelta(), 0.1));
    requestAnimationFrame(tick);
  }
  function resume() {
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

const hero = document.querySelector('.dw-hero');
if (hero) start(hero);
