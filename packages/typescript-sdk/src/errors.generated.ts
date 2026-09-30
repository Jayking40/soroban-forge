// Generated file. Do not edit manually.
// Generated from crates/shared-utils/src/errors.rs

/**
 * Mapping of ForgeError codes to their names.
 */
export const FORGE_ERRORS: Record<number, string> = {
  [1]: "Unauthorized",
  [2]: "NotFound",
  [3]: "InvalidInput",
  [4]: "InsufficientFunds",
  [5]: "AlreadyInitialized",
  [6]: "NotInitialized",
  [7]: "DeadlineReached",
  [8]: "InsufficientAllowance",
  [9]: "ArithmeticOverflow",
  [10]: "Custom",
  [11]: "TokenTransferFailed",
  [12]: "ContractInvocationFailed",
  [13]: "WithdrawalLimitExceeded",
  [14]: "AccrualEmpty",
  [15]: "InvalidState"
};

/**
 * Get the name of a ForgeError by its numeric code.
 * @param code The error code
 * @returns The error name, or undefined if not found
 */
export function forgeErrorName(code: number): string | undefined {
  return FORGE_ERRORS[code];
}
