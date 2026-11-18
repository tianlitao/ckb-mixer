declare module "*.wasm" {
  const module: WebAssembly.Module;
  export default module;
}

declare module "*.wasm?url" {
  const url: string;
  export default url;
}

declare module "snarkjs" {
  export const groth16: {
    fullProve(
      input: unknown,
      wasm: string | Uint8Array,
      zkey: string | Uint8Array,
    ): Promise<{ proof: unknown; publicSignals: string[] }>;
  };
}
