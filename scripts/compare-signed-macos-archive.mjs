#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Prove a signed macOS release archive differs from the RC Build archive it was
// made from only by its code signatures.
//
// release.yml no longer compiles. It takes the macOS archives RC Build built and
// the preflight judged, signs kin, kin-daemon and KinNotifier.app with the
// Developer ID, notarizes them, and packages them again. This check is what
// makes "the same bytes, signed" a measured statement rather than a plan:
//
//   - the two archives carry the same members under the same root, except that
//     the signed KinNotifier.app gains its sealed resource list
//     (Contents/_CodeSignature/CodeResources) and, where the notarization
//     ticket was stapled, Contents/CodeResources;
//   - every member that is not a Mach-O executable or the checksum list is
//     byte-identical, with the same executable bit;
//   - each Mach-O executable is byte-identical to its RC original once both
//     have their signature removed by codesign, apart from the virtual size
//     of the __LINKEDIT segment, which codesign grows to hold a signature and
//     does not shrink when it removes one;
//   - checksums-sha256.txt lists exactly the sha256 of every regular file in
//     the signed root, which is how the archive documents the re-signed
//     binaries.
//
// Measured on 2026-10-12 with a Developer ID Application signature, hardened
// runtime and a secure timestamp, on a 74 MB arm64 kin binary and on
// linker-signed arm64 and unsigned x86_64 executables: codesign
// --remove-signature leaves exactly the 8 bytes of __LINKEDIT's vmsize
// different from the unsigned original, and nothing else.
//
// One exception surfaced on the v0.8.4 release. codesign starts a signature on
// a 16-byte boundary, so it first pads an unsigned binary whose length is not
// a multiple of 16 with zeros up to one. macOS 26's codesign drops that
// padding again when it removes the signature; macOS 15's, on the Intel
// signing runner, leaves it, with __LINKEDIT's file size grown to cover it.
// trimSignatureAlignment forgives exactly that padding and nothing more.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const MH_MAGIC_64 = 0xfeedfacf;
const LC_SEGMENT_64 = 0x19;
const LC_CODE_SIGNATURE = 0x1d;

export const MACHO_MEMBERS = Object.freeze(["kin", "kin-daemon", "KinNotifier.app/Contents/MacOS/KinNotifier"]);
export const CHECKSUM_MEMBER = "checksums-sha256.txt";
export const SIGNATURE_MEMBERS = Object.freeze({
  required: ["KinNotifier.app/Contents/_CodeSignature/CodeResources"],
  // Written by `xcrun stapler staple`. The Linux notarization backend staples
  // nothing, so its archives carry no ticket and still verify online.
  optional: ["KinNotifier.app/Contents/CodeResources"],
});
const BUNDLE = "KinNotifier.app";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

// A thin 64-bit Mach-O with its signature already removed, with the one field
// codesign leaves behind zeroed. Anything else about the file stays as it is,
// so two of these compare equal only when every other byte does.
export function normalizeUnsignedMachO(bytes, label = "Mach-O") {
  const copy = Buffer.from(bytes);
  if (copy.length < 32 || copy.readUInt32LE(0) !== MH_MAGIC_64) {
    throw new Error(`${label} is not a thin 64-bit little-endian Mach-O`);
  }
  const ncmds = copy.readUInt32LE(16);
  const sizeofcmds = copy.readUInt32LE(20);
  if (32 + sizeofcmds > copy.length) {
    throw new Error(`${label} declares load commands past its end`);
  }
  let offset = 32;
  let linkedit = 0;
  for (let index = 0; index < ncmds; index += 1) {
    if (offset + 8 > 32 + sizeofcmds) throw new Error(`${label} load command ${index} overruns the header`);
    const cmd = copy.readUInt32LE(offset);
    const cmdsize = copy.readUInt32LE(offset + 4);
    if (cmdsize < 8 || offset + cmdsize > 32 + sizeofcmds) {
      throw new Error(`${label} load command ${index} has an invalid size`);
    }
    if (cmd === LC_CODE_SIGNATURE) {
      throw new Error(`${label} still carries a code signature`);
    }
    if (cmd === LC_SEGMENT_64 && copy.toString("latin1", offset + 8, offset + 24).replace(/\0+$/, "") === "__LINKEDIT") {
      linkedit += 1;
      copy.writeBigUInt64LE(0n, offset + 32);
    }
    offset += cmdsize;
  }
  if (linkedit !== 1) {
    throw new Error(`${label} has ${linkedit} __LINKEDIT segments, expected exactly one`);
  }
  return copy;
}

// The __LINKEDIT LC_SEGMENT_64 command's offset in a thin 64-bit Mach-O, or -1.
function linkeditCommand(bytes) {
  if (bytes.length < 32 || bytes.readUInt32LE(0) !== MH_MAGIC_64) return -1;
  const ncmds = bytes.readUInt32LE(16);
  const end = 32 + bytes.readUInt32LE(20);
  let offset = 32;
  for (let index = 0; index < ncmds && offset + 8 <= end && end <= bytes.length; index += 1) {
    const cmdsize = bytes.readUInt32LE(offset + 4);
    if (cmdsize < 8) return -1;
    if (
      bytes.readUInt32LE(offset) === LC_SEGMENT_64 &&
      offset + 56 <= end &&
      bytes.toString("latin1", offset + 8, offset + 24).replace(/\0+$/, "") === "__LINKEDIT"
    ) {
      return offset;
    }
    offset += cmdsize;
  }
  return -1;
}

// An unsigned RC binary and its signed copy after codesign --remove-signature.
// When the RC binary ends off a 16-byte boundary, codesign padded it with
// zeros up to one before placing the signature, and some codesign releases
// keep that padding when they remove the signature. Returns the signed copy
// with the padding and the matching growth of __LINKEDIT's file size removed,
// but only when the copy is exactly that: the RC length rounded up to 16,
// the extra bytes all zero, and __LINKEDIT ending at the end of the file in
// both, grown by exactly the padding. Anything else is returned unchanged for
// the byte comparison to refuse.
export function trimSignatureAlignment(rcBytes, signedBytes) {
  const padding = signedBytes.length - rcBytes.length;
  if (padding <= 0 || padding >= 16 || signedBytes.length % 16 !== 0) return signedBytes;
  if (!signedBytes.subarray(rcBytes.length).every((byte) => byte === 0)) return signedBytes;
  const rcLinkedit = linkeditCommand(rcBytes);
  const signedLinkedit = linkeditCommand(signedBytes);
  if (rcLinkedit < 0 || signedLinkedit !== rcLinkedit) return signedBytes;
  const rcEnd = rcBytes.readBigUInt64LE(rcLinkedit + 40) + rcBytes.readBigUInt64LE(rcLinkedit + 48);
  const signedSize = signedBytes.readBigUInt64LE(signedLinkedit + 48);
  const signedEnd = signedBytes.readBigUInt64LE(signedLinkedit + 40) + signedSize;
  if (rcEnd !== BigInt(rcBytes.length) || signedEnd !== BigInt(signedBytes.length)) return signedBytes;
  const trimmed = Buffer.from(signedBytes.subarray(0, rcBytes.length));
  trimmed.writeBigUInt64LE(signedSize - BigInt(padding), signedLinkedit + 48);
  return trimmed;
}

function firstDifference(left, right) {
  const length = Math.min(left.length, right.length);
  for (let index = 0; index < length; index += 1) {
    if (left[index] !== right[index]) return index;
  }
  return length;
}

// What scripts/write-release-archive-docs.mjs writes for a root: the sha256 of
// every regular file beside the list itself, sorted by name.
export function expectedChecksumList(root) {
  const lines = readdirSync(root)
    .sort()
    .filter((name) => name !== CHECKSUM_MEMBER && statSync(join(root, name)).isFile())
    .map((name) => `${sha256(readFileSync(join(root, name)))}  ${name}`);
  return `${lines.join("\n")}\n`;
}

function listFiles(root) {
  const out = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const full = join(dir, name);
      const stat = statSync(full);
      if (stat.isDirectory()) walk(full);
      else if (stat.isFile()) out.push(relative(root, full).split("\\").join("/"));
      else throw new Error(`${relative(root, full)} is neither a file nor a directory`);
    }
  };
  walk(root);
  return out;
}

function unpack(archive, artifact, into) {
  execFileSync("tar", ["-xzf", archive, "-C", into]);
  const roots = readdirSync(into);
  if (JSON.stringify(roots) !== JSON.stringify([artifact])) {
    throw new Error(`${archive} unpacks to ${roots.join(", ") || "nothing"}, expected exactly ${artifact}/`);
  }
  return join(into, artifact);
}

const run = (file, args) => execFileSync(file, args, { stdio: ["ignore", "pipe", "pipe"] });

export function compare({ rcArchive, signedArchive, artifact, exec = run, log = console.log }) {
  const work = mkdtempSync(join(tmpdir(), `${artifact}-compare-`));
  try {
    const rcRoot = unpack(rcArchive, artifact, mkdtempSync(join(work, "rc-")));
    const signedRoot = unpack(signedArchive, artifact, mkdtempSync(join(work, "signed-")));
    const rcFiles = listFiles(rcRoot);
    const signedFiles = listFiles(signedRoot);

    for (const member of [...SIGNATURE_MEMBERS.required, ...SIGNATURE_MEMBERS.optional]) {
      if (rcFiles.includes(member)) {
        throw new Error(`the RC archive already carries ${member}, so it is not the unsigned build`);
      }
    }
    const missing = rcFiles.filter((member) => !signedFiles.includes(member));
    const added = signedFiles.filter((member) => !rcFiles.includes(member));
    const unexpected = added.filter(
      (member) => !SIGNATURE_MEMBERS.required.includes(member) && !SIGNATURE_MEMBERS.optional.includes(member),
    );
    const absentSignature = SIGNATURE_MEMBERS.required.filter((member) => !signedFiles.includes(member));
    if (missing.length || unexpected.length || absentSignature.length) {
      throw new Error(
        `the signed ${artifact} archive does not carry the RC members plus a signature: ` +
          `missing ${missing.join(", ") || "none"}, unexpected ${unexpected.join(", ") || "none"}, ` +
          `unsigned ${absentSignature.join(", ") || "none"}`,
      );
    }
    for (const member of MACHO_MEMBERS) {
      if (!rcFiles.includes(member)) throw new Error(`the RC archive carries no ${member}`);
    }

    // The bundle is verified as the unit codesign sealed; each executable is
    // verified on its own, strictly.
    exec("codesign", ["--verify", "--strict", "--deep", "--verbose=2", join(signedRoot, BUNDLE)]);
    for (const member of rcFiles) {
      const rcFile = join(rcRoot, member);
      const signedFile = join(signedRoot, member);
      if ((statSync(rcFile).mode & 0o111) !== (statSync(signedFile).mode & 0o111)) {
        throw new Error(`${member} changed its executable bits between the RC and signed archives`);
      }
      if (MACHO_MEMBERS.includes(member)) {
        exec("codesign", ["--verify", "--strict", "--verbose=2", signedFile]);
        const [rcBytes, signedBytes] = [rcFile, signedFile].map((source, index) => {
          const copy = join(work, `${index === 0 ? "rc" : "signed"}-${member.replaceAll("/", "_")}`);
          copyFileSync(source, copy);
          // An RC binary may carry no signature at all (x86_64) or the
          // linker's ad hoc one (arm64); --remove-signature handles both.
          exec("codesign", ["--remove-signature", copy]);
          return readFileSync(copy);
        });
        const pair = [
          normalizeUnsignedMachO(rcBytes, `RC ${member}`),
          normalizeUnsignedMachO(trimSignatureAlignment(rcBytes, signedBytes), `signed ${member}`),
        ];
        if (!pair[0].equals(pair[1])) {
          throw new Error(
            `${member} differs from the RC build by more than its code signature ` +
              `(unsigned, the RC copy is ${rcBytes.length} bytes and the signed copy ${signedBytes.length}; ` +
              `they first differ at byte ${firstDifference(pair[0], pair[1])})`,
          );
        }
        log(`${member}: identical to the RC build apart from its signature`);
        continue;
      }
      if (member === CHECKSUM_MEMBER) {
        if (readFileSync(signedFile, "utf8") !== expectedChecksumList(signedRoot)) {
          throw new Error(`${CHECKSUM_MEMBER} does not list the signed archive's own files`);
        }
        if (readFileSync(rcFile, "utf8") !== expectedChecksumList(rcRoot)) {
          throw new Error(`the RC ${CHECKSUM_MEMBER} does not list the RC archive's own files`);
        }
        log(`${CHECKSUM_MEMBER}: lists the signed files`);
        continue;
      }
      if (!readFileSync(rcFile).equals(readFileSync(signedFile))) {
        throw new Error(`${member} differs between the RC and signed archives`);
      }
    }
    log(
      `${artifact}: the signed archive is the RC archive plus ${added.join(", ")}, ` +
        "with every executable identical apart from its signature",
    );
    return { added };
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    const key = { "--rc-archive": "rcArchive", "--signed-archive": "signedArchive", "--artifact": "artifact" }[argv[i]];
    if (!key || argv[i + 1] === undefined) {
      throw new Error("usage: compare-signed-macos-archive.mjs --rc-archive <tar.gz> --signed-archive <tar.gz> --artifact <name>");
    }
    args[key] = argv[i + 1];
  }
  for (const key of ["rcArchive", "signedArchive", "artifact"]) {
    if (!args[key]) throw new Error(`missing --${key.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)}`);
  }
  return args;
}

function isDirectRun() {
  const entry = process.argv[1];
  if (!entry) return false;
  const self = fileURLToPath(import.meta.url);
  if (entry === self) return true;
  try {
    return realpathSync(entry) === realpathSync(self);
  } catch {
    return false;
  }
}

if (isDirectRun()) {
  try {
    compare(parseArgs(process.argv.slice(2)));
  } catch (error) {
    const detail = error.stderr ? `: ${String(error.stderr).trim()}` : "";
    console.error(`::error::${error.message}${detail}`);
    process.exit(1);
  }
}
