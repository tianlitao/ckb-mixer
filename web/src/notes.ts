// Notes: the two secrets a deposit is withdrawn with. They are made in this
// browser, kept in its local storage, and sent nowhere.

import { commitment as commit } from "../wasm/mixer_wasm.js";
import { fromHex, toHex } from "../shared/hex";
import type { Message } from "../shared/tx";

const PREFIX = "ckbmix2-testnet-";
const STORE = "ckbmix2.testnet.notes";

export interface Note {
  /** 31 random bytes each, as 32-byte little-endian field elements, hex. */
  nullifier: string;
  secret: string;
  commitment: string;
  created: number;
  /** The transaction that made the intent cell, once it was sent from here. */
  intent?: { tx: string; reclaimAfter: string };
  /** The transaction that took the intent back, if it was sent from here. */
  reclaimed?: string;
  /** The last withdrawal message proved for this note. It holds no secrets. */
  withdrawal?: { message: Message; via?: "aggregator" | "self"; tx?: string };
}

function randomField(): Uint8Array {
  const out = new Uint8Array(32);
  crypto.getRandomValues(out.subarray(0, 31)); // 248 bits: always below the field's modulus
  return out;
}

export function newNote(): Note {
  const [nullifier, secret] = [randomField(), randomField()];
  return { nullifier: toHex(nullifier), secret: toHex(secret), commitment: toHex(commit(nullifier, secret)), created: Date.now() };
}

/** The string a person backs up: all a withdrawal needs besides the chain. */
export function noteString(note: Note): string {
  return PREFIX + note.nullifier.slice(2, 64) + note.secret.slice(2, 64);
}

export function parseNote(text: string): Note {
  const match = new RegExp(`^${PREFIX}([0-9a-f]{62})([0-9a-f]{62})$`).exec(text.trim());
  if (!match) throw new Error("这不是一张测试网 v2 池子的票据");
  const [nullifier, secret] = [fromHex(match[1] + "00"), fromHex(match[2] + "00")];
  return { nullifier: toHex(nullifier), secret: toHex(secret), commitment: toHex(commit(nullifier, secret)), created: Date.now() };
}

export function loadNotes(): Note[] {
  try {
    const notes = JSON.parse(localStorage.getItem(STORE) ?? "[]");
    return Array.isArray(notes) ? notes : [];
  } catch {
    return [];
  }
}

export function saveNotes(notes: Note[]) {
  localStorage.setItem(STORE, JSON.stringify(notes));
}
