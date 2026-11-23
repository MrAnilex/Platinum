// Prepares the user's logo drawing for the application: removes the white background around the
// drawing (white inside the outlines stays) and writes
//   ui/logo-wide.png   the whole drawing cropped tight (not square), for the window's header
//   ui/icon.png        48 × 48 icon version, for the window and the taskbar
//   tray.ico           16 to 256 px icon version, for the icon next to the clock
//   platinum.ico       16 to 256 px, tighter at small sizes, for the Start menu and the taskbar
//   <preview>          everything on dark and light backgrounds, to check it
// The icon version leaves out the sparkles (unreadable that small), fills the square with the head
// and gets a thin light outline at small sizes, so that it stands out on a dark taskbar.
// Usage, from platinum/: node ui/logo-prep.mjs ui/art/logo-dessin.png ui [preview.png]
import { readFileSync, writeFileSync } from "node:fs";
import { inflateSync, deflateSync, crc32 } from "node:zlib";
import { join } from "node:path";

const [input, outDir, previewPath] = process.argv.slice(2);

function decode(file) {
    const data = readFileSync(file);
    let offset = 8, width = 0, height = 0, colour = 0, depth = 0;
    const idat = [];
    while (offset < data.length) {
        const length = data.readUInt32BE(offset);
        const type = data.subarray(offset + 4, offset + 8).toString();
        const body = data.subarray(offset + 8, offset + 8 + length);
        if (type === "IHDR") {
            width = body.readUInt32BE(0);
            height = body.readUInt32BE(4);
            depth = body[8];
            colour = body[9];
            if (body[12] !== 0) throw new Error("interlaced PNG not handled");
        } else if (type === "IDAT") idat.push(body);
        offset += 12 + length;
    }
    if (depth !== 8 || (colour !== 2 && colour !== 6)) throw new Error(`PNG colour ${colour}/${depth} not handled`);
    const channels = colour === 6 ? 4 : 3;
    const raw = inflateSync(Buffer.concat(idat));
    const stride = width * channels;
    const pixels = Buffer.alloc(width * height * 4);
    const previous = Buffer.alloc(stride);
    const line = Buffer.alloc(stride);
    for (let y = 0; y < height; y++) {
        const filter = raw[y * (stride + 1)];
        const source = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
        for (let i = 0; i < stride; i++) {
            const left = i >= channels ? line[i - channels] : 0;
            const up = previous[i];
            const upLeft = i >= channels ? previous[i - channels] : 0;
            let value = source[i];
            if (filter === 1) value += left;
            else if (filter === 2) value += up;
            else if (filter === 3) value += (left + up) >> 1;
            else if (filter === 4) {
                const p = left + up - upLeft, pa = Math.abs(p - left), pb = Math.abs(p - up), pc = Math.abs(p - upLeft);
                value += pa <= pb && pa <= pc ? left : pb <= pc ? up : upLeft;
            }
            line[i] = value & 255;
        }
        line.copy(previous);
        for (let x = 0; x < width; x++) {
            const o = (y * width + x) * 4;
            pixels[o] = line[x * channels];
            pixels[o + 1] = line[x * channels + 1];
            pixels[o + 2] = line[x * channels + 2];
            pixels[o + 3] = channels === 4 ? line[x * channels + 3] : 255;
        }
    }
    return { width, height, pixels };
}

function encode(width, height, pixels) {
    const rows = Buffer.alloc(height * (width * 4 + 1));
    for (let y = 0; y < height; y++) pixels.copy(rows, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
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
    header[8] = 8;
    header[9] = 6;
    return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", header), chunk("IDAT", deflateSync(rows, { level: 9 })), chunk("IEND", Buffer.alloc(0))]);
}

const { width, height, pixels } = decode(input);
const brightness = (i) => (pixels[i * 4] + pixels[i * 4 + 1] + pixels[i * 4 + 2]) / 3;

// 1. Background: near-white pixels reachable from the border.
const background = new Uint8Array(width * height);
const stack = [];
for (let x = 0; x < width; x++) stack.push(x, (height - 1) * width + x);
for (let y = 0; y < height; y++) stack.push(y * width, y * width + width - 1);
while (stack.length) {
    const i = stack.pop();
    if (background[i] || brightness(i) < 230) continue;
    background[i] = 1;
    const x = i % width, y = (i / width) | 0;
    if (x > 0) stack.push(i - 1);
    if (x < width - 1) stack.push(i + 1);
    if (y > 0) stack.push(i - width);
    if (y < height - 1) stack.push(i + width);
}
// 2. Background becomes transparent; the light anti-aliased pixels along it become partly
//    transparent dark ink, so the outline keeps a soft edge without a white fringe.
for (let i = 0; i < width * height; i++) {
    if (background[i]) {
        pixels[i * 4 + 3] = 0;
        continue;
    }
    const x = i % width, y = (i / width) | 0;
    const touches = [i - 1, i + 1, i - width, i + width].some((j, k) => (k === 0 ? x > 0 : k === 1 ? x < width - 1 : k === 2 ? y > 0 : y < height - 1) && background[j]);
    const b = brightness(i);
    if (touches && b > 90) {
        const ink = Math.round(((255 - b) / (255 - 90)) * 255);
        pixels[i * 4] = pixels[i * 4 + 1] = pixels[i * 4 + 2] = 30;
        pixels[i * 4 + 3] = Math.max(0, Math.min(255, ink));
    }
}

// 3. Pieces of the drawing: the head (largest connected piece) and the sparkles around it.
function bounds(test) {
    let minX = width, minY = height, maxX = -1, maxY = -1;
    for (let y = 0; y < height; y++)
        for (let x = 0; x < width; x++)
            if (test(y * width + x)) {
                minX = Math.min(minX, x); maxX = Math.max(maxX, x);
                minY = Math.min(minY, y); maxY = Math.max(maxY, y);
            }
    return { minX, minY, maxX, maxY, w: maxX - minX + 1, h: maxY - minY + 1 };
}
const label = new Int32Array(width * height).fill(-1);
const pieceSizes = [];
for (let start = 0; start < width * height; start++) {
    if (label[start] >= 0 || pixels[start * 4 + 3] <= 8) continue;
    const id = pieceSizes.length;
    let count = 0;
    const todo = [start];
    label[start] = id;
    while (todo.length) {
        const i = todo.pop();
        count++;
        const x = i % width, y = (i / width) | 0;
        for (const [dx, dy] of [[1, 0], [-1, 0], [0, 1], [0, -1], [1, 1], [-1, -1], [1, -1], [-1, 1]]) {
            const nx = x + dx, ny = y + dy;
            if (nx < 0 || ny < 0 || nx >= width || ny >= height) continue;
            const j = ny * width + nx;
            if (label[j] < 0 && pixels[j * 4 + 3] > 8) {
                label[j] = id;
                todo.push(j);
            }
        }
    }
    pieceSizes.push(count);
}
const head = pieceSizes.indexOf(Math.max(...pieceSizes));

// Copies a box of the drawing (optionally only the head) into a new w × h image, at (dx, dy).
function crop(box, w, h, dx, dy, onlyHead) {
    const out = Buffer.alloc(w * h * 4);
    for (let y = 0; y < box.h; y++)
        for (let x = 0; x < box.w; x++) {
            const sx = box.minX + x, sy = box.minY + y, tx = dx + x, ty = dy + y;
            if (tx < 0 || ty < 0 || tx >= w || ty >= h) continue;
            const i = sy * width + sx;
            if (onlyHead && label[i] !== head) continue;
            pixels.copy(out, (ty * w + tx) * 4, i * 4, i * 4 + 4);
        }
    return out;
}

// 4. Resample by area averaging, with premultiplied alpha.
function resize(source, fw, fh, tw, th) {
    const out = Buffer.alloc(tw * th * 4);
    const sx = fw / tw, sy = fh / th;
    for (let y = 0; y < th; y++)
        for (let x = 0; x < tw; x++) {
            let r = 0, g = 0, b = 0, a = 0, n = 0;
            for (let yy = Math.floor(y * sy); yy < Math.min(fh, Math.ceil((y + 1) * sy)); yy++)
                for (let xx = Math.floor(x * sx); xx < Math.min(fw, Math.ceil((x + 1) * sx)); xx++) {
                    const o = (yy * fw + xx) * 4, alpha = source[o + 3] / 255;
                    r += source[o] * alpha; g += source[o + 1] * alpha; b += source[o + 2] * alpha; a += alpha; n++;
                }
            const o = (y * tw + x) * 4;
            if (a > 0) {
                out[o] = Math.round(r / a); out[o + 1] = Math.round(g / a); out[o + 2] = Math.round(b / a);
            }
            out[o + 3] = Math.round((a / n) * 255);
        }
    return out;
}

// A light outline of `thickness` pixels around the shape, under it.
function outline(img, s, thickness) {
    if (thickness <= 0) return img;
    const out = Buffer.alloc(s * s * 4);
    for (let y = 0; y < s; y++)
        for (let x = 0; x < s; x++) {
            let around = 0;
            for (let dy = -thickness; dy <= thickness; dy++)
                for (let dx = -thickness; dx <= thickness; dx++) {
                    if (dx * dx + dy * dy > thickness * thickness + 1) continue;
                    const nx = x + dx, ny = y + dy;
                    if (nx >= 0 && ny >= 0 && nx < s && ny < s) around = Math.max(around, img[(ny * s + nx) * 4 + 3]);
                }
            const o = (y * s + x) * 4, a = img[o + 3] / 255, edge = around / 255;
            // the drawing over a light (#e6e6e6) halo
            const ra = a + edge * (1 - a);
            for (let c = 0; c < 3; c++) out[o + c] = ra > 0 ? Math.round((img[o + c] * a + 230 * edge * (1 - a)) / ra) : 0;
            out[o + 3] = Math.round(ra * 255);
        }
    return out;
}

// 5. Header: the whole drawing, cropped tight, 128 px high.
const all = bounds((i) => pixels[i * 4 + 3] > 8);
const wide = crop(all, all.w, all.h, 0, 0, false);
const wideH = 128, wideW = Math.round((all.w / all.h) * wideH);
const wideImage = resize(wide, all.w, all.h, wideW, wideH);
writeFileSync(join(outDir, "ui", "logo-wide.png"), encode(wideW, wideH, wideImage));

// 6. Icon version: the head alone, filling the square. The drills make it much wider than high:
//    up to 40 px (the icon next to the clock, the taskbar) their tips are cut (22 % of the width
//    left out) so that the face is bigger; larger sizes keep the whole head.
const hb = bounds((i) => label[i] === head);
const squared = (side) => ({ side, image: crop(hb, side, side, Math.round((side - hb.w) / 2), Math.round((side - hb.h) / 2), true) });
const tight = squared(Math.ceil(Math.max(hb.h * 1.08, hb.w * 0.78)));
const whole = squared(Math.ceil(Math.max(hb.w, hb.h) * 1.06));
const iconAt = (s) => {
    const { side, image } = s <= 40 ? tight : whole;
    return outline(resize(image, side, side, s, s), s, s <= 24 ? 1 : s <= 64 ? 2 : 0);
};
// The window's icon, which Windows shows at 24 to 32 px on the taskbar button: cropped even
// tighter (the head almost fills the height, the drills are shortened), at the user's request.
const taskbar = squared(Math.ceil(Math.max(hb.h * 1.02, hb.w * 0.6)));
const taskbarIcon = outline(resize(taskbar.image, taskbar.side, taskbar.side, 48, 48), 48, 2);
writeFileSync(join(outDir, "ui", "icon.png"), encode(48, 48, taskbarIcon));

// ICO files with PNG entries (read by Windows Vista and later).
function writeIco(name, imageAt) {
    const sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    const images = sizes.map((s) => encode(s, s, imageAt(s)));
    const header = Buffer.alloc(6 + 16 * sizes.length);
    header.writeUInt16LE(0, 0);
    header.writeUInt16LE(1, 2);
    header.writeUInt16LE(sizes.length, 4);
    let at = header.length;
    sizes.forEach((s, k) => {
        const e = 6 + 16 * k;
        header[e] = s === 256 ? 0 : s;
        header[e + 1] = s === 256 ? 0 : s;
        header.writeUInt16LE(1, e + 4);
        header.writeUInt16LE(32, e + 6);
        header.writeUInt32LE(images[k].length, e + 8);
        header.writeUInt32LE(at, e + 12);
        at += images[k].length;
    });
    writeFileSync(join(outDir, name), Buffer.concat([header, ...images]));
}
// The icon next to the clock (built into platinum.exe).
writeIco("tray.ico", iconAt);
// The application's icon (Start menu shortcut, and the taskbar button, which Windows takes from
// the shortcut): the tightest crop up to 48 px.
writeIco("platinum.ico", (s) => (s <= 48 ? outline(resize(taskbar.image, taskbar.side, taskbar.side, s, s), s, s <= 24 ? 1 : 2) : iconAt(s)));

// 7. Preview: dark (the window, a dark taskbar) above, light (a light taskbar) below.
if (previewPath) {
    const W = 560, H = 300, preview = Buffer.alloc(W * H * 4);
    for (let y = 0; y < H; y++)
        for (let x = 0; x < W; x++) {
            const o = (y * W + x) * 4, light = y >= 220;
            preview[o] = light ? 238 : 15; preview[o + 1] = light ? 238 : 16; preview[o + 2] = light ? 240 : 18; preview[o + 3] = 255;
        }
    const draw = (img, w, h, px, py) => {
        for (let y = 0; y < h; y++)
            for (let x = 0; x < w; x++) {
                const o = (y * w + x) * 4, a = img[o + 3] / 255, d = ((py + y) * W + px + x) * 4;
                for (let c = 0; c < 3; c++) preview[d + c] = Math.round(img[o + c] * a + preview[d + c] * (1 - a));
            }
    };
    const headerW = Math.round((all.w / all.h) * 44);
    draw(resize(wide, all.w, all.h, headerW, 44), headerW, 44, 12, 12);
    draw(wideImage, wideW, wideH, 12, 76);
    let x = 250;
    for (const s of [64, 48, 32, 24, 20, 16]) {
        draw(iconAt(s), s, s, x, 30);
        draw(iconAt(s), s, s, x, 232);
        x += s + 14;
    }
    const t32 = resize(taskbarIcon, 48, 48, 32, 32), t24 = resize(taskbarIcon, 48, 48, 24, 24);
    draw(t32, 32, 32, 250, 120); draw(t24, 24, 24, 300, 124);
    draw(t32, 32, 32, 250, 262); draw(t24, 24, 24, 300, 266);
    writeFileSync(previewPath, encode(W, H, preview));
}
console.log(`drawing ${width}x${height}; head ${hb.w}x${hb.h}; wrote ui/logo-wide.png (${wideW}x${wideH}), ui/icon.png, tray.ico and platinum.ico`);
