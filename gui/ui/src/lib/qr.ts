// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * A QR code out of a string — the pairing code, so that a camera can read it off
 * the screen.
 *
 * ONE mode (8-bit bytes) and ONE error-correction level (M, about 15 % of the
 * symbol recoverable), which is all this project ever encodes: a pairing payload
 * is base64url and hex, and it is read from a screen a hand's width away rather
 * than off a crumpled label. Everything else is the standard's (ISO/IEC 18004):
 * the capacity tables, the Reed-Solomon code over GF(256), the module layout,
 * and the eight masks with their penalty rules.
 *
 * Written here rather than pulled in: the interface has exactly two runtime
 * dependencies, and this is a pure function of a string — arithmetic, no I/O.
 * What makes that defensible is the test file next to it, which pins every
 * version and every mask against an independent implementation.
 */

/** Error-correction level M, as the two format bits encode it. */
const EC_LEVEL_BITS = 0b00;

/**
 * Error-correction codewords per block, level M, indexed by version − 1. From
 * the standard's table 13; the arithmetic around it is derived, this is the one
 * part that has to be tabulated.
 */
const ECC_PER_BLOCK = [
  10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26,
  26, 26, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
  28, 28,
];

/** Error-correction blocks, level M, indexed by version − 1. Same table. */
const BLOCKS = [
  1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18,
  20, 21, 23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49,
];

const MIN_VERSION = 1;
const MAX_VERSION = 40;

/** Penalty weights for the four mask rules (standard §7.8.3.1). */
const PENALTY_RUN = 3;
const PENALTY_BOX = 3;
const PENALTY_FINDER_LIKE = 40;
const PENALTY_IMBALANCE = 10;

/** A square of modules: `true` is dark. Row-major, `matrix[y][x]`. */
export type QrMatrix = boolean[][];

/**
 * The smallest symbol that holds `text` (UTF-8, byte mode). Throws when none
 * holds it — past 2331 bytes at this level, twenty times any code this project
 * shows.
 */
export function qrMatrix(text: string): QrMatrix {
  const data = [...new TextEncoder().encode(text)];
  const version = smallestVersion(data.length);
  const codewords = addEccAndInterleave(dataCodewords(data, version), version);
  return draw(codewords, version);
}

/**
 * The dark modules as one SVG path — a single node instead of a thousand rects.
 * Coordinates are in modules, the origin being the symbol's top-left corner.
 */
export function qrPath(matrix: QrMatrix): string {
  const parts: string[] = [];
  for (let y = 0; y < matrix.length; y++) {
    for (let x = 0; x < matrix.length; x++) {
      // One square per dark module. Adjacent squares share an edge, and the
      // renderer's fill rule joins them: no seams at any zoom.
      if (matrix[y][x]) parts.push(`M${x} ${y}h1v1h-1z`);
    }
  }
  return parts.join("");
}

// -- Capacity ---------------------------------------------------------------

/** Modules available for data and error correction, function patterns removed. */
function rawDataModules(version: number): number {
  let modules = (16 * version + 128) * version + 64;
  if (version >= 2) {
    const align = Math.floor(version / 7) + 2;
    modules -= (25 * align - 10) * align - 55;
    // The two version-information blocks, from version 7 on.
    if (version >= 7) modules -= 36;
  }
  return modules;
}

const totalCodewords = (version: number) =>
  Math.floor(rawDataModules(version) / 8);

const eccCodewords = (version: number) =>
  ECC_PER_BLOCK[version - 1] * BLOCKS[version - 1];

/** Bits the byte-mode character count takes — the standard widens it at 10. */
const countBits = (version: number) => (version <= 9 ? 8 : 16);

/** Bits left for the payload itself, header included. */
function capacityBits(version: number): number {
  return (totalCodewords(version) - eccCodewords(version)) * 8;
}

function smallestVersion(bytes: number): number {
  for (let version = MIN_VERSION; version <= MAX_VERSION; version++) {
    if (4 + countBits(version) + bytes * 8 <= capacityBits(version)) {
      return version;
    }
  }
  throw new Error(`${bytes} bytes do not fit in a QR code`);
}

// -- Codewords --------------------------------------------------------------

/** Header, payload, terminator and padding — the data half of the symbol. */
function dataCodewords(data: readonly number[], version: number): number[] {
  const bits: number[] = [];
  const push = (value: number, width: number) => {
    for (let i = width - 1; i >= 0; i--) bits.push((value >>> i) & 1);
  };
  push(0b0100, 4); // byte mode
  push(data.length, countBits(version));
  for (const byte of data) push(byte, 8);

  const capacity = capacityBits(version);
  // Terminator, then to a whole codeword, then the standard's alternating
  // filler for whatever room is left.
  push(0, Math.min(4, capacity - bits.length));
  push(0, (8 - (bits.length % 8)) % 8);
  for (let pad = 0xec; bits.length < capacity; pad ^= 0xec ^ 0x11) {
    push(pad, 8);
  }

  const codewords: number[] = [];
  for (let i = 0; i < bits.length; i += 8) {
    let byte = 0;
    for (let j = 0; j < 8; j++) byte = (byte << 1) | bits[i + j];
    codewords.push(byte);
  }
  return codewords;
}

/**
 * Splits the data into the version's blocks, appends each block's error
 * correction, and interleaves the lot the way the standard reads it back: one
 * codeword per block in turn, so that damage to one region is spread over every
 * block instead of destroying one.
 */
function addEccAndInterleave(data: readonly number[], version: number): number[] {
  const blocks = BLOCKS[version - 1];
  const eccLen = ECC_PER_BLOCK[version - 1];
  const raw = totalCodewords(version);
  // The blocks are not all the same size: the short ones come first, and the
  // long ones carry one extra data codeword each.
  const shortBlocks = blocks - (raw % blocks);
  const shortLen = Math.floor(raw / blocks);
  const divisor = rsDivisor(eccLen);

  const parts: number[][] = [];
  for (let i = 0, at = 0; i < blocks; i++) {
    const length = shortLen - eccLen + (i < shortBlocks ? 0 : 1);
    const chunk = data.slice(at, at + length);
    at += length;
    const ecc = rsRemainder(chunk, divisor);
    // A short block is padded to the long blocks' length so the interleaving
    // below can walk a rectangle; the pad is skipped when it is read back.
    if (i < shortBlocks) chunk.push(0);
    parts.push([...chunk, ...ecc]);
  }

  const result: number[] = [];
  for (let i = 0; i < parts[0].length; i++) {
    for (let j = 0; j < parts.length; j++) {
      if (i !== shortLen - eccLen || j >= shortBlocks) result.push(parts[j][i]);
    }
  }
  return result;
}

/** Multiplication in GF(256) modulo the standard's primitive polynomial. */
function gfMul(x: number, y: number): number {
  let z = 0;
  for (let i = 7; i >= 0; i--) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z;
}

/** Coefficients of the generator polynomial of the given degree, leading 1 implied. */
function rsDivisor(degree: number): number[] {
  const result = new Array<number>(degree).fill(0);
  result[degree - 1] = 1;
  // Multiply by (x − r) for r = 1, α, α², … : one root per correctable codeword.
  let root = 1;
  for (let i = 0; i < degree; i++) {
    for (let j = 0; j < degree; j++) {
      result[j] = gfMul(result[j], root);
      if (j + 1 < degree) result[j] ^= result[j + 1];
    }
    root = gfMul(root, 0x02);
  }
  return result;
}

function rsRemainder(data: readonly number[], divisor: readonly number[]): number[] {
  const result = new Array<number>(divisor.length).fill(0);
  for (const byte of data) {
    const factor = byte ^ (result.shift() ?? 0);
    result.push(0);
    for (let i = 0; i < divisor.length; i++) {
      result[i] ^= gfMul(divisor[i], factor);
    }
  }
  return result;
}

// -- Layout -----------------------------------------------------------------

interface Canvas {
  size: number;
  modules: boolean[][];
  /** A module the mask must not touch and the data must not land on. */
  reserved: boolean[][];
}

function draw(codewords: readonly number[], version: number): QrMatrix {
  const size = version * 4 + 17;
  const canvas: Canvas = {
    size,
    modules: grid(size),
    reserved: grid(size),
  };
  drawFunctionPatterns(canvas, version);
  drawCodewords(canvas, codewords);

  // The mask is chosen, not fixed: whichever of the eight leaves the symbol
  // easiest to read, by the standard's four penalty rules.
  let best = 0;
  let bestPenalty = Infinity;
  for (let mask = 0; mask < 8; mask++) {
    applyMask(canvas, mask);
    drawFormatBits(canvas, mask);
    const penalty = penaltyScore(canvas);
    if (penalty < bestPenalty) {
      bestPenalty = penalty;
      best = mask;
    }
    applyMask(canvas, mask); // XOR again: back to the unmasked symbol
  }
  applyMask(canvas, best);
  drawFormatBits(canvas, best);
  return canvas.modules;
}

const grid = (size: number): boolean[][] =>
  Array.from({ length: size }, () => new Array<boolean>(size).fill(false));

function setFunction(canvas: Canvas, x: number, y: number, dark: boolean): void {
  canvas.modules[y][x] = dark;
  canvas.reserved[y][x] = true;
}

function drawFunctionPatterns(canvas: Canvas, version: number): void {
  const { size } = canvas;
  // Timing patterns: one alternating row and one column, right through.
  for (let i = 0; i < size; i++) {
    setFunction(canvas, 6, i, i % 2 === 0);
    setFunction(canvas, i, 6, i % 2 === 0);
  }
  // Three finders, with the light separator that surrounds them.
  for (const [x, y] of [
    [3, 3],
    [size - 4, 3],
    [3, size - 4],
  ]) {
    drawFinder(canvas, x, y);
  }
  // Alignment patterns, skipping the three whose place a finder already holds.
  const positions = alignmentPositions(version);
  const last = positions.length - 1;
  for (let i = 0; i <= last; i++) {
    for (let j = 0; j <= last; j++) {
      const corner =
        (i === 0 && j === 0) ||
        (i === 0 && j === last) ||
        (i === last && j === 0);
      if (!corner) drawAlignment(canvas, positions[i], positions[j]);
    }
  }
  // Format information (its value comes with the mask), and the version blocks.
  drawFormatBits(canvas, 0);
  drawVersionBits(canvas, version);
}

/** The 7×7 finder and its separator: dark except two rings out of five. */
function drawFinder(canvas: Canvas, cx: number, cy: number): void {
  for (let dy = -4; dy <= 4; dy++) {
    for (let dx = -4; dx <= 4; dx++) {
      const ring = Math.max(Math.abs(dx), Math.abs(dy));
      const x = cx + dx;
      const y = cy + dy;
      if (x >= 0 && x < canvas.size && y >= 0 && y < canvas.size) {
        setFunction(canvas, x, y, ring !== 2 && ring !== 4);
      }
    }
  }
}

/** The 5×5 alignment pattern: dark except the ring one module out. */
function drawAlignment(canvas: Canvas, cx: number, cy: number): void {
  for (let dy = -2; dy <= 2; dy++) {
    for (let dx = -2; dx <= 2; dx++) {
      setFunction(
        canvas,
        cx + dx,
        cy + dy,
        Math.max(Math.abs(dx), Math.abs(dy)) !== 1,
      );
    }
  }
}

/**
 * Where the alignment patterns sit: the standard spaces them as evenly as an
 * even number of modules allows, anchored on 6 and on the far timing pattern.
 * Version 32 is the one the rule does not fit, and the standard names its step.
 */
function alignmentPositions(version: number): number[] {
  if (version === 1) return [];
  const count = Math.floor(version / 7) + 2;
  const size = version * 4 + 17;
  const step =
    version === 32 ? 26 : Math.ceil((size - 13) / (count * 2 - 2)) * 2;
  const positions = [6];
  for (let pos = size - 7; positions.length < count; pos -= step) {
    positions.splice(1, 0, pos);
  }
  return positions;
}

/**
 * The 15 format bits — level and mask, BCH-protected — written twice, so that
 * losing a corner does not lose the way in.
 */
function drawFormatBits(canvas: Canvas, mask: number): void {
  const { size } = canvas;
  const data = (EC_LEVEL_BITS << 3) | mask;
  let rem = data;
  for (let i = 0; i < 10; i++) rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
  const bits = ((data << 10) | rem) ^ 0x5412;
  const bit = (i: number) => ((bits >>> i) & 1) !== 0;

  for (let i = 0; i <= 5; i++) setFunction(canvas, 8, i, bit(i));
  setFunction(canvas, 8, 7, bit(6));
  setFunction(canvas, 8, 8, bit(7));
  setFunction(canvas, 7, 8, bit(8));
  for (let i = 9; i < 15; i++) setFunction(canvas, 14 - i, 8, bit(i));

  for (let i = 0; i < 8; i++) setFunction(canvas, size - 1 - i, 8, bit(i));
  for (let i = 8; i < 15; i++) setFunction(canvas, 8, size - 15 + i, bit(i));
  setFunction(canvas, 8, size - 8, true); // always dark
}

/** The 18 version bits, from version 7 on: two copies, transposed. */
function drawVersionBits(canvas: Canvas, version: number): void {
  if (version < 7) return;
  let rem = version;
  for (let i = 0; i < 12; i++) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
  const bits = (version << 12) | rem;
  for (let i = 0; i < 18; i++) {
    const dark = ((bits >>> i) & 1) !== 0;
    const a = canvas.size - 11 + (i % 3);
    const b = Math.floor(i / 3);
    setFunction(canvas, a, b, dark);
    setFunction(canvas, b, a, dark);
  }
}

/**
 * Lays the codewords out in the standard's snake: two-module columns walked from
 * the right, alternating upward and downward, stepping over everything the
 * function patterns hold. The last few modules of a symbol may go unused — the
 * standard leaves them light.
 */
function drawCodewords(canvas: Canvas, codewords: readonly number[]): void {
  const { size } = canvas;
  let i = 0;
  for (let right = size - 1; right >= 1; right -= 2) {
    // The vertical timing pattern is not a data column: shift past it.
    if (right === 6) right = 5;
    for (let vert = 0; vert < size; vert++) {
      for (let j = 0; j < 2; j++) {
        const x = right - j;
        const upward = ((right + 1) & 2) === 0;
        const y = upward ? size - 1 - vert : vert;
        if (!canvas.reserved[y][x] && i < codewords.length * 8) {
          canvas.modules[y][x] = ((codewords[i >>> 3] >>> (7 - (i & 7))) & 1) !== 0;
          i++;
        }
      }
    }
  }
}

/** XORs the mask over every module the function patterns do not hold. */
function applyMask(canvas: Canvas, mask: number): void {
  const { size } = canvas;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      if (canvas.reserved[y][x]) continue;
      if (maskBit(mask, x, y)) canvas.modules[y][x] = !canvas.modules[y][x];
    }
  }
}

function maskBit(mask: number, x: number, y: number): boolean {
  switch (mask) {
    case 0:
      return (x + y) % 2 === 0;
    case 1:
      return y % 2 === 0;
    case 2:
      return x % 3 === 0;
    case 3:
      return (x + y) % 3 === 0;
    case 4:
      return (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0;
    case 5:
      return ((x * y) % 2) + ((x * y) % 3) === 0;
    case 6:
      return (((x * y) % 2) + ((x * y) % 3)) % 2 === 0;
    default:
      return (((x + y) % 2) + ((x * y) % 3)) % 2 === 0;
  }
}

// -- Mask penalties ---------------------------------------------------------

/**
 * How hard the masked symbol is to read, by the standard's four rules: long runs
 * of one colour, 2×2 blocks of it, anything that looks like a finder pattern,
 * and an overall imbalance between dark and light. The lowest score wins.
 */
function penaltyScore(canvas: Canvas): number {
  const { size, modules } = canvas;
  let result = 0;

  for (let y = 0; y < size; y++) {
    result += lineScore(size, (x) => modules[y][x]);
  }
  for (let x = 0; x < size; x++) {
    result += lineScore(size, (y) => modules[y][x]);
  }

  // 2×2 blocks of one colour.
  for (let y = 0; y < size - 1; y++) {
    for (let x = 0; x < size - 1; x++) {
      const c = modules[y][x];
      if (
        c === modules[y][x + 1] &&
        c === modules[y + 1][x] &&
        c === modules[y + 1][x + 1]
      ) {
        result += PENALTY_BOX;
      }
    }
  }

  let dark = 0;
  for (const row of modules) for (const module of row) if (module) dark++;
  const total = size * size;
  // How far off half the modules are dark, in whole steps of 5 %.
  const off = Math.ceil(Math.abs(dark * 20 - total * 10) / total) - 1;
  return result + off * PENALTY_IMBALANCE;
}

/**
 * One row or column: the run rule and the finder-like rule. The runs are kept as
 * a rolling history of seven, which is what lets the finder rule see the light
 * margin on either side of a 1:1:3:1:1 pattern — including the quiet zone
 * outside the symbol, which the standard counts as light.
 */
function lineScore(size: number, at: (i: number) => boolean): number {
  let result = 0;
  const history = [0, 0, 0, 0, 0, 0, 0];
  let dark = false;
  let run = 0;

  const remember = (length: number) => {
    // Nothing recorded yet ⇒ this run touches the symbol's edge, and what lies
    // beyond is the quiet zone: light, and the standard counts it.
    const padded = history[0] === 0 ? length + size : length;
    history.pop();
    history.unshift(padded);
  };

  for (let i = 0; i < size; i++) {
    if (at(i) === dark) {
      run++;
      if (run === 5) result += PENALTY_RUN;
      else if (run > 5) result++;
      continue;
    }
    remember(run);
    if (!dark) result += finderLike(history) * PENALTY_FINDER_LIKE;
    dark = at(i);
    run = 1;
  }
  // The last run, then the quiet zone past the far edge.
  if (dark) {
    remember(run);
    run = 0;
  }
  remember(run + size);
  return result + finderLike(history) * PENALTY_FINDER_LIKE;
}

/**
 * How many finder-like patterns the last runs make: a 1:1:3:1:1 dark-light
 * sequence with four times its unit of light on one side and its unit on the
 * other — the shape a decoder looks for when it hunts for the corners.
 */
function finderLike(history: readonly number[]): number {
  const unit = history[1];
  const core =
    unit > 0 &&
    history[2] === unit &&
    history[3] === unit * 3 &&
    history[4] === unit &&
    history[5] === unit;
  return (
    (core && history[0] >= unit * 4 && history[6] >= unit ? 1 : 0) +
    (core && history[6] >= unit * 4 && history[0] >= unit ? 1 : 0)
  );
}
