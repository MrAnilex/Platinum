// Turns a window snapshot (PLATINUM_UI_SNAPSHOT: width, height as little-endian u32, then RGBA
// bytes) into a PNG. Usage: node snapshot-to-png.mjs <snapshot> <png>
import { readFileSync, writeFileSync } from "node:fs";
import { deflateSync, crc32 } from "node:zlib";

const [input, output] = process.argv.slice(2);
const data = readFileSync(input);
const width = data.readUInt32LE(0);
const height = data.readUInt32LE(4);
const rgba = data.subarray(8);
const rows = Buffer.alloc(height * (width * 4 + 1));
for (let y = 0; y < height; y++) {
    rgba.copy(rows, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
}
const chunk = (type, body) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(body.length);
    const typed = Buffer.concat([Buffer.from(type), body]);
    const check = Buffer.alloc(4);
    check.writeUInt32BE(crc32(typed) >>> 0);
    return Buffer.concat([length, typed, check]);
};
const header = Buffer.alloc(13);
header.writeUInt32BE(width, 0);
header.writeUInt32BE(height, 4);
header[8] = 8; // bits per channel
header[9] = 6; // RGBA
writeFileSync(output, Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", header), chunk("IDAT", deflateSync(rows)), chunk("IEND", Buffer.alloc(0))]));
console.log(`${output}: ${width}x${height}`);
