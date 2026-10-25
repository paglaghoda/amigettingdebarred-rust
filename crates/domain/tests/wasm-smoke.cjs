// Development-only WASM runtime check. Run from the workspace root after building
// the wasm_smoke example; Node is not a production dependency of the Rust engine.
const fs = require("node:fs");

async function main() {
  const bytes = fs.readFileSync(
    "target/wasm32-unknown-unknown/release/examples/wasm_smoke.wasm",
  );
  const { instance } = await WebAssembly.instantiate(bytes, {});
  if (instance.exports.run_contract_checks() !== 0) {
    throw new Error("WASM contract checks failed");
  }
  console.log(
    "WASM runtime: fixture, projections, leave deduplication, serialization and IST checks passed",
  );
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
