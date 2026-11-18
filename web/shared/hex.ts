export function toHex(bytes: Uint8Array): string {
  let out = "0x";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

export function fromHex(hex: string): Uint8Array {
  const digits = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (digits.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(digits)) throw new Error(`not hex: ${hex.slice(0, 20)}`);
  const out = new Uint8Array(digits.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(digits.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

export function u64le(value: bigint): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, value, true);
  return out;
}

export const hexNum = (value: bigint | number): string => "0x" + BigInt(value).toString(16);

/** Shannons as CKB, without trailing zeros. */
export function ckb(shannons: bigint): string {
  const whole = shannons / 100_000_000n;
  const rest = (shannons % 100_000_000n).toString().padStart(8, "0").replace(/0+$/, "");
  return rest ? `${whole}.${rest}` : `${whole}`;
}

/** CKB written by a person, as shannons. */
export function parseCkb(text: string): bigint {
  const match = /^(\d+)(?:\.(\d{1,8}))?$/.exec(text.trim());
  if (!match) throw new Error(`不是合法的 CKB 数量: ${text}`);
  return BigInt(match[1]) * 100_000_000n + BigInt((match[2] ?? "").padEnd(8, "0") || "0");
}
