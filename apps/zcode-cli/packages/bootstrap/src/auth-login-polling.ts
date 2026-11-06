export async function pollUntilReady(): Promise<never> {
  throw new Error("ZCode OAuth login is disabled.");
}
