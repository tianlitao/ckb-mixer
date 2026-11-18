// Generate a Groth16 withdrawal proof with snarkjs.
//
// Usage: node prove.mjs <input.json> <out_dir>
//
// input.json holds the circuit inputs as decimal strings:
//   root, nullifierHash, recipient, fee, nullifier, secret,
//   pathElements[20], pathIndices[20]
// Writes <out_dir>/proof.json and <out_dir>/public.json.
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as snarkjs from "snarkjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const build = path.join(here, "..", "build");

const [inputPath, outDir] = process.argv.slice(2);
if (!inputPath || !outDir) {
    console.error("usage: node prove.mjs <input.json> <out_dir>");
    process.exit(2);
}

const input = JSON.parse(readFileSync(inputPath, "utf8"));
const { proof, publicSignals } = await snarkjs.groth16.fullProve(
    input,
    path.join(build, "withdraw_js", "withdraw.wasm"),
    path.join(build, "withdraw_final.zkey"),
);

const vk = JSON.parse(readFileSync(path.join(build, "verification_key.json"), "utf8"));
if (!(await snarkjs.groth16.verify(vk, publicSignals, proof))) {
    console.error("snarkjs rejected its own proof");
    process.exit(1);
}

writeFileSync(path.join(outDir, "proof.json"), JSON.stringify(proof));
writeFileSync(path.join(outDir, "public.json"), JSON.stringify(publicSignals));
// snarkjs keeps worker threads alive; exit explicitly.
process.exit(0);
