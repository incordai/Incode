/**
 * Pictures attached to a prompt: what is accepted, and the bounds. Pure, so
 * every rule is tested without the editor (test/images.test.ts).
 *
 * # What is accepted
 *
 * PNG and JPEG only, recognised from their own first bytes, whatever the
 * name or the claimed type says. Those are the two formats incode takes
 * (src/acp_agent/present.rs `picture`, src/toolbox/image.rs `sniff`); sending
 * a GIF or a WebP would only get the whole prompt refused by the agent.
 *
 * # The bounds, and why
 *
 * - {@link MAX_IMAGE_BYTES}, 3 750 000 bytes per picture: incode's own
 *   `MAX_IMAGE_BYTES`. Its base64 is 5 000 000 characters, where model
 *   endpoints commonly stop taking an image at all.
 * - {@link MAX_IMAGES}, 4 per prompt: incode's `MAX_IMAGES_PER_RESULT`.
 * - {@link MAX_TOTAL_IMAGE_BYTES}, 5 MiB for all of them together: a prompt
 *   is ONE line of JSON-RPC, and incode drops a line over 8 MiB
 *   (src/acp_agent/transport.rs `MAX_LINE_BYTES`). Base64 makes 5 MiB about
 *   6.7 MiB, which leaves room for the text beside it.
 *
 * media/chat.js repeats these numbers to refuse a file before reading it;
 * this module is the check that counts, applied again to what the page sends.
 */

export const IMAGE_MIME_TYPES = ["image/png", "image/jpeg"] as const;
export type ImageMimeType = (typeof IMAGE_MIME_TYPES)[number];

export const MAX_IMAGE_BYTES = 3_750_000;
export const MAX_IMAGES = 4;
export const MAX_TOTAL_IMAGE_BYTES = 5 * 1024 * 1024;

/** One picture as it travels from the page to the agent: base64 `data`. */
export interface Attachment {
  name: string;
  mimeType: ImageMimeType;
  data: string;
}

export type Checked<T> = { ok: true; value: T } | { ok: false; reason: string };

/** The format the first bytes say, or undefined when it is neither PNG nor JPEG. */
export function sniffImage(bytes: Uint8Array): ImageMimeType | undefined {
  const png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (bytes.length >= png.length && png.every((b, i) => bytes[i] === b)) return "image/png";
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) return "image/jpeg";
  return undefined;
}

function tooLarge(name: string, size: number): string {
  return `${name} is ${formatBytes(size)}; a picture may be at most ${formatBytes(MAX_IMAGE_BYTES)}`;
}

export function formatBytes(n: number): string {
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KiB`;
  return `${n} bytes`;
}

/** A file read from disk, as an attachment, or why it is refused. */
export function imageFromFile(name: string, bytes: Uint8Array): Checked<Attachment> {
  if (bytes.length > MAX_IMAGE_BYTES) return { ok: false, reason: tooLarge(name, bytes.length) };
  const mimeType = sniffImage(bytes);
  if (!mimeType) return { ok: false, reason: `${name} is not a PNG or JPEG picture` };
  return { ok: true, value: { name, mimeType, data: Buffer.from(bytes).toString("base64") } };
}

const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;

/**
 * The attachments the page sent with a prompt, checked again: the shape, the
 * type (claimed AND sniffed), each size, the count and the total.
 */
export function checkAttachments(list: unknown): Checked<Attachment[]> {
  if (list === undefined || list === null) return { ok: true, value: [] };
  if (!Array.isArray(list)) return { ok: false, reason: "the attachments are not a list" };
  if (list.length > MAX_IMAGES) return { ok: false, reason: `at most ${MAX_IMAGES} pictures go with one prompt` };
  const out: Attachment[] = [];
  let total = 0;
  for (const item of list as unknown[]) {
    const a = item as Partial<Attachment> | null;
    const name = typeof a?.name === "string" && a.name ? a.name : "a picture";
    if (!a || typeof a.data !== "string" || typeof a.mimeType !== "string") {
      return { ok: false, reason: `${name} is not a picture this client can send` };
    }
    if (!(IMAGE_MIME_TYPES as readonly string[]).includes(a.mimeType)) {
      return { ok: false, reason: `${name} is ${a.mimeType}; only PNG and JPEG pictures are accepted` };
    }
    // Bounded BEFORE decoding, and exactly: base64 is 4 characters for every
    // 3 bytes, so MAX_IMAGE_BYTES encode to at most this many characters and
    // one byte more to more than it.
    if (a.data.length > Math.ceil(MAX_IMAGE_BYTES / 3) * 4) return { ok: false, reason: tooLarge(name, Math.floor((a.data.length / 4) * 3)) };
    if (a.data.length % 4 !== 0 || !BASE64.test(a.data)) return { ok: false, reason: `${name} is not valid base64` };
    const bytes = Buffer.from(a.data, "base64");
    if (sniffImage(bytes) !== a.mimeType) return { ok: false, reason: `${name} does not hold the ${a.mimeType} picture it claims to` };
    total += bytes.length;
    if (total > MAX_TOTAL_IMAGE_BYTES) {
      return { ok: false, reason: `the pictures come to more than ${formatBytes(MAX_TOTAL_IMAGE_BYTES)} together` };
    }
    out.push({ name, mimeType: a.mimeType as ImageMimeType, data: a.data });
  }
  return { ok: true, value: out };
}
