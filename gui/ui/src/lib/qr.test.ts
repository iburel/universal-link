// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// The encoder against fixed points taken from an INDEPENDENT implementation of
// ISO/IEC 18004 (Nayuki's reference, run outside this repo): the exact symbol
// for a small payload, and the size + fingerprint of the largest payload every
// one of the 40 versions holds. Byte for byte, this encoder produced the same
// symbol as the reference for every version, every one of the eight masks and
// several hundred payloads; what is kept here is what makes a REGRESSION
// visible, not a rerun of that comparison.

import { expect, test } from "vitest";

import { qrMatrix, qrPath, type QrMatrix } from "./qr";

/** A deterministic payload of `n` bytes: printable ASCII, cycling. */
const payload = (n: number) =>
  Array.from({ length: n }, (_, i) => String.fromCharCode(33 + (i % 94))).join(
    "",
  );

/** FNV-1a over the modules: the whole symbol in eight hex digits. */
function digest(matrix: QrMatrix): string {
  let hash = 0x811c9dc5;
  for (const row of matrix) {
    for (const module of row) {
      hash ^= module ? 1 : 0;
      hash = Math.imul(hash, 0x01000193) >>> 0;
    }
  }
  return hash.toString(16).padStart(8, "0");
}

const art = (matrix: QrMatrix) =>
  matrix.map((row) => row.map((m) => (m ? "#" : ".")).join(""));

// The payload's bytes, the symbol's side, and its fingerprint — one row per
// version, the payload being the very largest that version holds. So a wrong
// capacity shows up as a size (the version chosen), and a wrong anything else
// as a fingerprint.
const PER_VERSION: [number, number, string][] = [
  [14, 21, "14082f75"],
  [26, 25, "abd3155d"],
  [42, 29, "591c0bc6"],
  [62, 33, "f0d1115e"],
  [84, 37, "6f0c9b85"],
  [106, 41, "6419e653"],
  [122, 45, "06ed2b33"],
  [152, 49, "1f2c2795"],
  [180, 53, "984c0ac4"],
  [213, 57, "758b4b33"],
  [251, 61, "6cbad8df"],
  [287, 65, "0224d1b7"],
  [331, 69, "6b98e9ef"],
  [362, 73, "a7fa9c97"],
  [412, 77, "d6af08c8"],
  [450, 81, "70786b34"],
  [504, 85, "7a216ba9"],
  [560, 89, "56e985e5"],
  [624, 93, "6f4271bd"],
  [666, 97, "e2ed94c9"],
  [711, 101, "4a7381fb"],
  [779, 105, "d70663b5"],
  [857, 109, "d3bc6e41"],
  [911, 113, "b115a33b"],
  [997, 117, "56496545"],
  [1059, 121, "625602f3"],
  [1125, 125, "02e2c6d9"],
  [1190, 129, "c4f980aa"],
  [1264, 133, "06f7f541"],
  [1370, 137, "57cb0730"],
  [1452, 141, "d6fa56a7"],
  [1538, 145, "ed8dbff8"],
  [1628, 149, "95963404"],
  [1722, 153, "b16f659c"],
  [1809, 157, "feaeb763"],
  [1911, 161, "ce2f48ac"],
  [1989, 165, "8fe9143f"],
  [2099, 169, "2b4fa289"],
  [2213, 173, "1d9f4b10"],
  [2331, 177, "9cf053af"],
];

test("a small payload gives exactly this symbol", () => {
  // Readable on purpose: the version-1 symbol for "HELLO", mask 4, as the
  // reference draws it. If anything in the encoder moves, this is the failure
  // that can be read with the eye.
  expect(art(qrMatrix("HELLO"))).toEqual([
    "#######.##.#..#######",
    "#.....#..##.#.#.....#",
    "#.###.#..####.#.###.#",
    "#.###.#.#..#..#.###.#",
    "#.###.#.#...#.#.###.#",
    "#.....#.#.##..#.....#",
    "#######.#.#.#.#######",
    "........#####........",
    "#...#.######.#####..#",
    "...###..#.###..#.####",
    "#.##..#.#.##..###..#.",
    "###..#...#...##.#....",
    "..#.###..#..###...##.",
    "........###.###..#.##",
    "#######.##..##...#.#.",
    "#.....#....##..#...#.",
    "#.###.#.#..#..###.#.#",
    "#.###.#....##....#.##",
    "#.###.#..###..####...",
    "#.....#..#...##......",
    "#######.#...#####.#.#",
  ]);
});

test("every version is laid out as the standard says", () => {
  for (const [bytes, size, expected] of PER_VERSION) {
    const matrix = qrMatrix(payload(bytes));
    expect(matrix.length, `${bytes} bytes`).toBe(size);
    expect(matrix.every((row) => row.length === size)).toBe(true);
    expect(digest(matrix), `${bytes} bytes`).toBe(expected);
  }
});

// The other side of the same table: one byte more than a version holds must
// move up to the next one. A capacity computed one byte too generously would
// silently overflow the symbol's data region.
test("one byte past a version moves up to the next", () => {
  for (let i = 0; i < PER_VERSION.length - 1; i++) {
    const [bytes] = PER_VERSION[i];
    const [, nextSize] = PER_VERSION[i + 1];
    expect(qrMatrix(payload(bytes + 1)).length, `${bytes + 1} bytes`).toBe(
      nextSize,
    );
  }
});

test("what no symbol holds is refused, not truncated", () => {
  expect(() => qrMatrix(payload(2332))).toThrow(/do not fit/);
});

// The count is in BYTES, not characters: a version chosen on the character
// count would overflow on anything non-ASCII.
test("multi-byte characters count as the bytes they are", () => {
  expect(qrMatrix("é".repeat(7)).length).toBe(21); // 14 bytes: version 1
  expect(qrMatrix("é".repeat(8)).length).toBe(25); // 16 bytes: version 2
});

// A pairing code is ~105 characters (tag, 128-bit secret, X25519 public key,
// session id). It has to land in a symbol a phone can read off a screen from a
// hand's width away: 41 modules is 6 px each in a 250 px tile.
test("a pairing code fits a symbol a camera can read", () => {
  const code = `UL1:${"A".repeat(22)}:${"B".repeat(43)}:p_${"9".repeat(32)}`;
  expect(code.length).toBe(105);
  expect(qrMatrix(code).length).toBe(41); // version 6
});

test("the patterns a camera hunts for are where it looks", () => {
  const matrix = qrMatrix(payload(106));
  const size = matrix.length;
  for (const [cx, cy] of [
    [3, 3],
    [size - 4, 3],
    [3, size - 4],
  ]) {
    for (let dy = -4; dy <= 4; dy++) {
      for (let dx = -4; dx <= 4; dx++) {
        const x = cx + dx;
        const y = cy + dy;
        if (x < 0 || y < 0 || x >= size || y >= size) continue;
        const ring = Math.max(Math.abs(dx), Math.abs(dy));
        // Dark 7×7 with a light ring inside it, and a light separator around.
        expect(matrix[y][x], `${x},${y}`).toBe(ring !== 2 && ring !== 4);
      }
    }
  }
  // The timing patterns join the finders: one alternating row, one column.
  for (let i = 8; i < size - 8; i++) {
    expect(matrix[6][i], `timing row at ${i}`).toBe(i % 2 === 0);
    expect(matrix[i][6], `timing column at ${i}`).toBe(i % 2 === 0);
  }
  // The one module the standard fixes dark, whatever the data.
  expect(matrix[size - 8][8]).toBe(true);
});

test("the path draws one square per dark module", () => {
  const matrix = qrMatrix("HELLO");
  const path = qrPath(matrix);
  const dark = matrix.flat().filter(Boolean).length;
  expect(path.match(/M/g)?.length).toBe(dark);
  // The top-left module of the top-left finder, at the origin.
  expect(path.startsWith("M0 0h1v1h-1z")).toBe(true);
  // Nothing is drawn for a light module: (7,0) is the finder's separator.
  expect(path).not.toContain("M7 0h");
});
