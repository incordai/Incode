// The picture rules of src/images.ts: accepted types, each bound at its edge.
import { strict as assert } from "node:assert";
import { describe, it } from "node:test";
import { MAX_IMAGE_BYTES, MAX_IMAGES, MAX_TOTAL_IMAGE_BYTES, checkAttachments, imageFromFile, sniffImage } from "../src/images";

const PNG_SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/** `size` bytes that start like a PNG. */
function png(size: number): Buffer {
  const b = Buffer.alloc(size);
  Buffer.from(PNG_SIGNATURE).copy(b);
  return b;
}
const jpeg = (size: number): Buffer => {
  const b = Buffer.alloc(size);
  b[0] = 0xff;
  b[1] = 0xd8;
  b[2] = 0xff;
  return b;
};
const sent = (name: string, mimeType: string, bytes: Buffer) => ({ name, mimeType, data: bytes.toString("base64") });

describe("pictures (src/images.ts)", () => {
  it("are recognised by their first bytes: PNG and JPEG, nothing else", () => {
    assert.equal(sniffImage(png(16)), "image/png");
    assert.equal(sniffImage(jpeg(16)), "image/jpeg");
    assert.equal(sniffImage(Buffer.from("GIF89a....")), undefined);
    assert.equal(sniffImage(Buffer.from("<svg xmlns=")), undefined);
    assert.equal(sniffImage(Buffer.from(PNG_SIGNATURE.slice(0, 7))), undefined, "a cut signature is not a PNG");
  });

  it("a file is taken up to the per-picture bound and refused one byte past it", () => {
    const at = imageFromFile("at.png", png(MAX_IMAGE_BYTES));
    assert.ok(at.ok && at.value.mimeType === "image/png" && at.value.data.length > 0);
    const past = imageFromFile("past.png", png(MAX_IMAGE_BYTES + 1));
    assert.ok(!past.ok);
    assert.match(past.reason, /^past\.png is .*a picture may be at most/);
    const text = imageFromFile("notes.png", Buffer.from("hello"));
    assert.deepEqual(text, { ok: false, reason: "notes.png is not a PNG or JPEG picture" });
  });

  it("what the page sends is checked again: type claimed and sniffed, base64, size, count, total", () => {
    assert.deepEqual(checkAttachments(undefined), { ok: true, value: [] });
    const one = checkAttachments([sent("a.jpg", "image/jpeg", jpeg(10))]);
    assert.ok(one.ok && one.value.length === 1 && one.value[0].mimeType === "image/jpeg");

    const refused = (list: unknown): string => {
      const r = checkAttachments(list);
      assert.ok(!r.ok, JSON.stringify(list).slice(0, 100));
      return r.reason;
    };
    assert.match(refused([sent("a.gif", "image/gif", Buffer.from("GIF89a"))]), /a\.gif is image\/gif; only PNG and JPEG/);
    assert.match(refused([sent("a.png", "image/png", jpeg(10))]), /a\.png does not hold the image\/png picture it claims to/);
    assert.match(refused([{ name: "a.png", mimeType: "image/png", data: "not base64!" }]), /a\.png is not valid base64/);
    assert.match(refused([{ name: "a.png", mimeType: "image/png" }]), /not a picture this client can send/);
    assert.match(refused("a.png"), /not a list/);
    assert.match(refused([sent("big.png", "image/png", png(MAX_IMAGE_BYTES + 1))]), /big\.png is .*at most/);
    assert.ok(checkAttachments([sent("edge.png", "image/png", png(MAX_IMAGE_BYTES))]).ok, "exactly the bound is allowed");
    assert.match(refused(Array.from({ length: MAX_IMAGES + 1 }, () => sent("a.png", "image/png", png(10)))), /at most 4 pictures/);
    const half = Math.floor(MAX_TOTAL_IMAGE_BYTES / 2) + 1;
    assert.match(refused([sent("a.png", "image/png", png(half)), sent("b.png", "image/png", png(half))]), /more than 5\.0 MiB together/);
  });
});
