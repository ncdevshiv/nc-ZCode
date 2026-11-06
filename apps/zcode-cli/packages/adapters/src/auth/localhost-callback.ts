import { createServer, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

const LOCALHOST = "127.0.0.1";
const SUCCESS_TEXT = "Authorization successful! You may close this window and return to the CLI.";
const FAILURE_TEXT = "Authorization failed. You may close this window and return to the CLI.";

/** The stable error code used by the authorization server when returning `error` according to RFC 6749 §4.1.2.1. */
export const MCP_OAUTH_CALLBACK_DENIED_ERROR_CODE = "MCP_OAUTH_CALLBACK_DENIED";

export interface McpOAuthCallbackDeniedError extends Error {
  code: typeof MCP_OAUTH_CALLBACK_DENIED_ERROR_CODE;
  oauthError: string;
  oauthErrorDescription?: string;
}

function createCallbackDeniedError(
  oauthError: string,
  oauthErrorDescription: string | null,
): McpOAuthCallbackDeniedError {
//Do not rely on error text for process judgment: the caller distinguishes "user rejection" and timeout based on the code/oauthError structured field.
  const error = new Error(
    `OAuth authorization was rejected by the authorization server: ${oauthError}`,
  ) as McpOAuthCallbackDeniedError;
  error.code = MCP_OAUTH_CALLBACK_DENIED_ERROR_CODE;
  error.oauthError = oauthError;
  if (oauthErrorDescription) error.oauthErrorDescription = oauthErrorDescription;
  return error;
}

export interface LocalhostOAuthCallback {
  code: string;
  url: string;
}

export interface LocalhostOAuthCallbackServer {
  callbackPath: string;
  callbackUrl: string;
  close(): Promise<void>;
  waitForCallback(): Promise<LocalhostOAuthCallback>;
}

export async function createLocalhostOAuthCallbackServer(input: {
  callbackPath: string;
  state: string;
}): Promise<LocalhostOAuthCallbackServer> {
  let resolveCallback: (value: LocalhostOAuthCallback) => void = () => undefined;
  let rejectCallback: (error: Error) => void = () => undefined;
  let settled = false;
  const callbackPromise = new Promise<LocalhostOAuthCallback>((resolve, reject) => {
    resolveCallback = resolve;
    rejectCallback = reject;
  });

  const server = createServer((request, response) => {
    try {
      const requestUrl = new URL(request.url ?? "/", `http://${LOCALHOST}`);
      if (requestUrl.pathname !== input.callbackPath) {
        writeText(response, 404, FAILURE_TEXT);
        return;
      }

// If the state does not match, the callback promise of this transaction will be rejected in the past. Concurrent authorization transactions,
// Any old authorization URL or prefetch request remaining in the browser will hit this listener; an unfamiliar state can make
// The correct callback arriving subsequently can never succeed again. Now just return 400 and continue to wait for the callback of this state.
      const state = requestUrl.searchParams.get("state") ?? "";
      if (state !== input.state) {
        writeText(response, 400, FAILURE_TEXT);
        return;
      }

// The state match indicates that this is indeed the authorization response for this transaction. When the user clicks "Reject", the authorization server replies
// error=access_denied, in the past it had to wait until the caller timed out before failing; now it is settled immediately.
      const oauthError = requestUrl.searchParams.get("error");
      if (oauthError) {
        writeText(response, 400, FAILURE_TEXT);
        if (!settled) {
          settled = true;
          rejectCallback(
            createCallbackDeniedError(
              oauthError,
              requestUrl.searchParams.get("error_description"),
            ),
          );
        }
        return;
      }

      const code =
        requestUrl.searchParams.get("authCode") ?? requestUrl.searchParams.get("code") ?? "";
      if (!code) {
// state matches but there is neither code nor error: the authorization response is illegal, this transaction cannot succeed and fails directly.
//Do not consume the remaining authorization window.
        writeText(response, 400, FAILURE_TEXT);
        if (!settled) {
          settled = true;
          rejectCallback(new Error("OAuth callback is missing an authorization code."));
        }
        return;
      }

      writeText(response, 200, SUCCESS_TEXT);
      if (!settled) {
        settled = true;
        resolveCallback({
          code,
          url: requestUrl.toString(),
        });
      }
    } catch (error) {
      writeText(response, 500, FAILURE_TEXT);
      if (!settled) {
        settled = true;
        rejectCallback(error instanceof Error ? error : new Error(String(error)));
      }
    }
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, LOCALHOST, () => {
      server.off("error", reject);
      resolve();
    });
  });

  const address = server.address();
  if (!isAddressInfo(address)) {
    await closeServer(server);
    throw new Error("Unable to resolve localhost callback server address.");
  }

  const callbackUrl = `http://${LOCALHOST}:${address.port}${input.callbackPath}`;
  return {
    callbackPath: input.callbackPath,
    callbackUrl,
    close: async () => {
      await closeServer(server);
    },
    waitForCallback: () => callbackPromise,
  };
}

function writeText(response: ServerResponse, status: number, message: string): void {
  response.writeHead(status, {
    "Content-Type": "text/plain; charset=utf-8",
  });
  response.end(message);
}

function closeServer(server: ReturnType<typeof createServer>): Promise<void> {
  if (!server.listening) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    server.close((error) => {
      if (error) {
        reject(error);
        return;
      }
      resolve();
    });
  });
}

function isAddressInfo(value: unknown): value is AddressInfo {
  return typeof value === "object" && value !== null && "port" in value;
}
