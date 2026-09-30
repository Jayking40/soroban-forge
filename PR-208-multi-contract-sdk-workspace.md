# PR: Multi-contract TypeScript SDK workspace with fixture-based drift tests

Closes #208

## Description

Restructures `packages/typescript-sdk` from a single-contract escrow client into a unified, multi-contract TypeScript SDK workspace. It generates and exports typed clients for multiple Soroban Forge contracts (`escrow`, `subscription-payments`, `multi-sig-wallet`, and `marketplace-royalties`), replaces fragile hand-maintained drift test arrays with deterministic fixture-based expectation snapshots, unifies build and test orchestration under `npm run build` / `npm test`, and wires CI so contract interface drift is caught automatically.

---

## Design Decisions Settled

1. **ABI Source**:
   - ABIs are derived deterministically from workspace WASM build outputs (`target/wasm32-unknown-unknown/release/*.wasm`) using `stellar contract inspect` / `stellar contract bindings typescript`.
   - Generates byte-stable TypeScript definitions decoupled from network availability.

2. **Package Layout & Subpath Exports**:
   - Direct subpath and namespace exports for all contracts:
     - `@soroban-forge/sdk/escrow`
     - `@soroban-forge/sdk/subscription-payments`
     - `@soroban-forge/sdk/multi-sig-wallet`
     - `@soroban-forge/sdk/marketplace-royalties`
     - Top-level `@soroban-forge/sdk` re-exporting shared errors (`ForgeError`), shared network configurations, and all contract namespaces.

3. **Fixture-based Drift Test Format**:
   - JSON-serialized ABI and event surface snapshot fixtures located in `src/tests/fixtures/*.json`.
   - Drift tests compare contract bindings against checked-in expectation fixtures rather than hand-maintained arrays.
   - Deterministic regeneration script provided at `scripts/regen-fixtures.sh` (or `npm run test:regen-fixtures`).

---

## Source Tree Restructure

### Before:
```text
packages/typescript-sdk/
├── src/
│   ├── index.ts              # Escrow client only
│   └── index.test.ts         # 25 hand-written tests with hardcoded arrays
├── package.json
└── tsconfig.json
```

### After:
```text
packages/typescript-sdk/
├── src/
│   ├── index.ts              # Unified entrypoint exporting all clients & shared types
│   ├── errors.ts             # Centralized ForgeError shared codes (1-15)
│   ├── contracts/
│   │   ├── escrow/           # Escrow client, types, and networks
│   │   ├── subscription/     # Subscription payments client, types, and networks
│   │   ├── multisig/         # Multi-sig wallet client, types, and networks
│   │   └── marketplace/      # Marketplace royalties client, types, and networks
│   ├── fixtures/
│   │   ├── escrow.abi.json
│   │   ├── subscription.abi.json
│   │   ├── multisig.abi.json
│   │   └── marketplace.abi.json
│   └── tests/
│       ├── drift.test.ts     # Fixture-driven drift assertion suite
│       ├── errors.test.ts    # Shared ForgeError table coverage
│       └── escrow.test.ts    # Adapted full 25-test escrow behavioral suite
├── scripts/
│   └── regen-fixtures.sh
├── package.json
└── tsconfig.json
```

---

## Drift Test Example

```typescript
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import * as EscrowClient from "../contracts/escrow";

test("escrow client matches checked-in ABI fixture specification", () => {
  const fixturePath = join(__dirname, "../fixtures/escrow.abi.json");
  const expectedAbi = JSON.parse(readFileSync(fixturePath, "utf8"));

  const actualMethods = Object.keys(EscrowClient.Client.prototype).filter(
    (key) => typeof EscrowClient.Client.prototype[key] === "function" && !key.startsWith("_")
  );

  assert.deepStrictEqual(
    actualMethods.sort(),
    expectedAbi.methods.sort(),
    "Detected drift between contract methods and checked-in SDK ABI fixture"
  );
});
```

---

## Verification & Test Suite

- [x] All 25 original escrow tests pass in adapted suite without weakened assertions.
- [x] Shared `ForgeError` table tested centrally across common error discriminants (1–15).
- [x] Deterministic fixture regeneration script produces clean diff on current tree.
- [x] `cd packages/typescript-sdk && npm ci && npm run build && npm test` passes cleanly.
- [x] CI workflow in `.github/workflows/ci.yml` verifies TypeScript SDK build and test execution on Node 18.
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [x] `cargo test --workspace --all-targets --locked`

---

## Type of Change

- [x] New feature (non-breaking restructure with expanded multi-contract client support)
- [x] Test infrastructure improvement (fixture-based drift tests)
- [x] Documentation updates (`packages/typescript-sdk/README.md`, `docs/DEVELOPMENT.md`)

---

## Checklist

- [x] My code follows the code style and guidelines of this repository.
- [x] No contract crate runtime code or dependencies were altered.
- [x] Lockfile (`package-lock.json`) is committed and pinned cleanly without floating ranges.
- [x] Documentation updated reflecting the new multi-contract import paths and regeneration instructions.
