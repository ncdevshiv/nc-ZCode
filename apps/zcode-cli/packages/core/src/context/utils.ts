// ============================================================
// Token Estimation
// ============================================================

import { ESTIMATED_TOKEN_CHAR_DIVISOR } from "@zcode/shared";

/**
* Estimate the number of tokens in the text
* Use shared characters to estimate the divisor, Chinese characters retain extra weight
* This is a rough estimate, sufficient for debugging and monitoring
 */
export function estimateTokens(text: string): number {
// Chinese characters usually take up more tokens than English characters, so are counted as two estimated characters.
  const chineseChars = (text.match(/[一-鿿]/g) || []).length;
  const otherChars = text.length - chineseChars;

  return Math.ceil((chineseChars * 2 + otherChars) / ESTIMATED_TOKEN_CHAR_DIVISOR);
}
